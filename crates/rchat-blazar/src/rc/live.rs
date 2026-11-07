//! DDP over WebSocket — the live-updates connection (M2 S2).
//!
//! Wire facts verified against the vendored server sources
//! (`packages/ddp-client/src/MinimalDDPClient.ts`):
//! connect `{msg:"connect",version:"1",support:[..]}` → `{msg:"connected"}`;
//! method `{msg:"method",method,params,id}` → `{msg:"result",id}`;
//! sub `{msg:"sub",id,name,params}` → added/changed collection frames;
//! ping/pong `{msg:"ping"|"pong",id?}`. Rocket.Chat layers login
//! (`method: "login"`, params `[{resume}]`) and stream subscriptions
//! (`stream-room-messages`, param `[roomId]`) on top; events arrive as
//! `added`/`changed` frames whose `fields.args[0]` is an IMessage.
//!
//! Architecture: everything the connection can observe — wire frames, the
//! keepalive tick, control commands from the application — decodes into one
//! [`Frame`] stream ([`DdpLive::next_frame`]); `run` is a flat loop of
//! one-line arms over that stream.

use serde_json::Value;

use crate::rc::{Msg, RcError, Session};
use async_channel::{Receiver, Sender};
use compio::runtime::time::sleep;
use compio_ws::tungstenite::{Error as WsError, Message};
use compio_ws::{WebSocketStream, connect_async_tls_with_config};

/// The collection that carries the watched room's chat messages.
const ROOM_STREAM: &str = "stream-room-messages";

/// One unit of work for the live loop: a decoded wire frame, the keepalive
/// tick, or a control command from the application.
#[derive(Debug)]
enum Frame {
    /// DDP-level ping from the server — answer with a pong.
    Ping,
    /// A decoded chat message from the watched room's stream.
    RoomMessage(Msg),
    /// Watch another room's message stream (control command).
    Watch(String),
    /// Nothing arrived within the keepalive window — send a ping.
    Idle,
    /// Any other DDP document (connected, result, foreign collections):
    /// the handshake inspects these, the run loop ignores them.
    Document(Value),
    /// The transport closed.
    Closed(String),
}

/// The connection state after the DDP handshake and resume login.
pub struct DdpLive {
    ws: WebSocketStream<compio::net::TcpStream>,
    next_id: u32,
    /// The id of the current `stream-room-messages` subscription, if any.
    room_sub: Option<String>,
}

/// What the live connection watches. The only permitted write target is the
/// self-DM (AGENTS.md live-send policy); the probe uses it for liveness.
pub enum LiveCommand {
    /// Watch this room's message stream.
    Watch(String),
}

/// The keepalive window during the handshake: inbound pings are answered,
/// outbound ones start only in [`DdpLive::run`].
const HANDSHAKE_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

/// The `msg` discriminator of a DDP document.
fn msg_kind(document: &Value) -> Option<&str> {
    document.get("msg").and_then(Value::as_str)
}

impl DdpLive {
    /// Opens the WebSocket, performs the DDP handshake and logs in with the
    /// session's resume token.
    pub async fn connect(url: &str, session: &Session) -> Result<Self, RcError> {
        let (ws, _response) = connect_async_tls_with_config(
            url.to_owned(),
            compio_ws::tungstenite::protocol::WebSocketConfig::default(),
            None,
        )
        .await
        .map_err(live_io)?;
        let mut live = Self {
            ws,
            next_id: 0,
            room_sub: None,
        };

        // DDP handshake: the server either accepts our protocol version or
        // tells us `failed` and closes.
        live.send_json(ddp_connect()).await?;
        let connected = live
            .wait_document(&|document| {
                matches!(msg_kind(document), Some("connected") | Some("failed"))
            })
            .await?;
        if msg_kind(&connected) == Some("failed") {
            return Err(RcError::Config("server refused our DDP version".to_owned()));
        }

        // Resume login: the stored auth token is the DDP credentials.
        let id = live.next_id();
        live.send_json(serde_json::json!({
            "msg": "method", "method": "login", "id": id,
            "params": [{"resume": session.auth_token}],
        }))
        .await?;
        let result = live
            .wait_document(&|document| {
                msg_kind(document) == Some("result")
                    && document.get("id").and_then(Value::as_str) == Some(id.as_str())
            })
            .await?;
        if result.get("error").is_some() {
            return Err(RcError::Config(format!(
                "resume login rejected: {}",
                result
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )));
        }
        Ok(live)
    }

    /// Waits for the next DDP document matching `accept`, answering pings
    /// on the way. A closed transport errors out.
    async fn wait_document(&mut self, accept: &dyn Fn(&Value) -> bool) -> Result<Value, RcError> {
        loop {
            match self.next_wire_frame(HANDSHAKE_IDLE).await? {
                Frame::Document(document) if accept(&document) => return Ok(document),
                Frame::Ping => self.reply_pong(None).await?,
                Frame::Closed(reason) => {
                    return Err(RcError::Io(std::io::Error::other(format!(
                        "handshake: {reason}"
                    ))));
                }
                _ => continue,
            }
        }
    }

    /// The live loop: decodes everything into [`Frame`]s and handles each in
    /// one line. Returns when the transport closes or the control channel
    /// does (application gone).
    pub async fn run(
        mut self,
        events: Sender<Msg>,
        commands: Receiver<LiveCommand>,
        keepalive: std::time::Duration,
    ) -> Result<(), RcError> {
        loop {
            match self.next_frame(keepalive, &commands).await? {
                Frame::RoomMessage(message) => {
                    let _ = events.send(message).await;
                }
                Frame::Watch(room_id) => self.watch(&room_id).await?,
                Frame::Ping => self.reply_pong(None).await?,
                Frame::Idle => {
                    // Prove liveness to the server and proxies.
                    self.send_json(serde_json::json!({"msg": "ping"})).await?
                }
                Frame::Document(_) => {}
                Frame::Closed(reason) => {
                    return Err(RcError::Io(std::io::Error::other(format!(
                        "live connection closed: {reason}"
                    ))));
                }
            }
        }
    }

    /// Switches the watched room: unsubscribes the previous stream, then
    /// subscribes `stream-room-messages` for the new room.
    pub async fn watch(&mut self, room_id: &str) -> Result<(), RcError> {
        if let Some(sub) = self.room_sub.take() {
            self.send_json(serde_json::json!({"msg": "unsub", "id": sub}))
                .await?;
        }
        let id = self.next_id();
        self.send_json(serde_json::json!({
            "msg": "sub", "id": id, "name": ROOM_STREAM, "params": [room_id],
        }))
        .await?;
        self.room_sub = Some(id.to_owned());
        Ok(())
    }

    /// Next frame for the run loop: control commands are drained without
    /// blocking first (they are rare — a room open), then the wire read and
    /// the keepalive tick race. A switch therefore lands within one
    /// keepalive window at worst.
    async fn next_frame(
        &mut self,
        keepalive: std::time::Duration,
        commands: &Receiver<LiveCommand>,
    ) -> Result<Frame, RcError> {
        if let Ok(LiveCommand::Watch(room)) = commands.try_recv() {
            return Ok(Frame::Watch(room));
        }
        // A closed control channel surfaces as a wire closure instead: the
        // application is gone, the read notices the dropped sender side.
        self.next_wire_frame(keepalive).await
    }

    /// Next frame from the wire alone: the incoming stream or the keepalive
    /// tick, whichever first. WebSocket-level pings are answered inside the
    /// read and never surface here.
    async fn next_wire_frame(&mut self, keepalive: std::time::Duration) -> Result<Frame, RcError> {
        futures_lite::future::or(async { self.read_wire().await }, async {
            sleep(keepalive).await;
            Ok::<Frame, RcError>(Frame::Idle)
        })
        .await
    }

    /// Reads one websocket frame and decodes it. Transparently answers
    /// WebSocket-level pings so they never surface as [`Frame`]s.
    async fn read_wire(&mut self) -> Result<Frame, RcError> {
        loop {
            let message = self.ws.read().await.map_err(|error| {
                if matches!(error, WsError::ConnectionClosed | WsError::AlreadyClosed) {
                    RcError::Io(std::io::Error::other("closed"))
                } else {
                    live_io(error)
                }
            })?;
            match message {
                Message::Text(text) => {
                    let document: Value = match serde_json::from_str(&text) {
                        Ok(document) => document,
                        Err(_) => continue, // not DDP — ignore
                    };
                    return Ok(decode_document(document));
                }
                Message::Ping(payload) => {
                    self.ws
                        .send(Message::Pong(payload))
                        .await
                        .map_err(live_io)?;
                }
                Message::Close(frame) => {
                    return Ok(Frame::Closed(format!("{frame:?}")));
                }
                _ => continue,
            }
        }
    }

    async fn reply_pong(&mut self, id: Option<&Value>) -> Result<(), RcError> {
        let mut pong = serde_json::json!({"msg": "pong"});
        if let Some(id) = id
            && !id.is_null()
        {
            pong["id"] = id.clone();
        }
        self.send_json(pong).await
    }

    async fn send_json(&mut self, value: Value) -> Result<(), RcError> {
        self.ws
            .send(Message::text(value.to_string()))
            .await
            .map_err(live_io)
    }

    fn next_id(&mut self) -> String {
        self.next_id += 1;
        format!("rchat-{}", self.next_id)
    }
}

/// Decodes a DDP document into the frame the live loop acts on.
fn decode_document(document: Value) -> Frame {
    match document.get("msg").and_then(Value::as_str) {
        Some("ping") => Frame::Ping,
        Some("added" | "changed")
            if document.get("collection").and_then(Value::as_str) == Some(ROOM_STREAM) =>
        {
            match message_from_event(&document) {
                Some(message) => Frame::RoomMessage(message),
                None => Frame::Document(document),
            }
        }
        _ => Frame::Document(document),
    }
}

/// The DDP handshake payload.
fn ddp_connect() -> Value {
    serde_json::json!({"msg": "connect", "version": "1", "support": ["1"]})
}

/// The websocket endpoint for the parsed server settings.
pub fn websocket_url(settings: &crate::rc::RcSettings) -> String {
    let (host, port) = settings.addr();
    let scheme = if settings.is_tls() { "wss" } else { "ws" };
    format!("{scheme}://{host}:{port}/websocket")
}

/// Extracts the chat message carried by a `stream-room-messages` event:
/// `fields.args[0]` is an IMessage.
fn message_from_event(event: &Value) -> Option<Msg> {
    let message = event.get("fields")?.get("args")?.as_array()?.first()?;
    let reactions = message
        .get("reactions")
        .and_then(Value::as_object)
        .map(|reactions| {
            reactions
                .iter()
                .filter_map(|(name, reaction)| {
                    let count = reaction
                        .get("usernames")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len) as u32;
                    (count > 0).then(|| (name.trim_matches(':').to_owned(), count))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Msg {
        id: message.get("_id")?.as_str()?.to_owned(),
        room_id: message.get("rid")?.as_str()?.to_owned(),
        sender: message
            .get("u")
            .and_then(|user| user.get("username"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        text: message
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        ts: message
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        reactions,
    })
}

fn live_io(error: WsError) -> RcError {
    RcError::Io(std::io::Error::other(error.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    #[test]
    fn decodes_a_stream_event_into_a_room_message() {
        let frame = decode_document(serde_json::json!({
            "msg": "changed",
            "collection": "stream-room-messages",
            "id": "id1",
            "fields": {"args": [{
                "_id": "m9", "rid": "ROOM", "msg": "привет",
                "ts": "2026-11-07T17:30:00.000Z",
                "u": {"_id": "u1", "username": "alice"}
            }]}
        }));
        match frame {
            Frame::RoomMessage(message) => {
                assert_eq!(message.id, "m9");
                assert_eq!(message.room_id, "ROOM");
                assert_eq!(message.sender, "alice");
                assert_eq!(message.text, "привет");
            }
            other => panic!("expected RoomMessage, got {other:?}"),
        }
    }

    #[test]
    fn decodes_ping_and_foreign_collections_as_documents() {
        assert!(matches!(
            decode_document(serde_json::json!({"msg": "ping"})),
            Frame::Ping
        ));

        let notify = decode_document(serde_json::json!({
            "msg": "changed", "collection": "stream-notify-user",
            "id": "id2", "fields": {"args": ["something"]}
        }));
        assert!(matches!(notify, Frame::Document(_)));

        let empty_args = decode_document(serde_json::json!({
            "msg": "added", "collection": "stream-room-messages",
            "id": "id3", "fields": {"args": []}
        }));
        assert!(matches!(empty_args, Frame::Document(_)));
    }

    #[test]
    fn websocket_url_carries_scheme_host_and_path() {
        let mut settings = crate::rc::RcSettings::parse("https://example.com").unwrap();
        assert_eq!(websocket_url(&settings), "wss://example.com:443/websocket");
        settings = crate::rc::RcSettings::parse("http://127.0.0.1:3000").unwrap();
        assert_eq!(websocket_url(&settings), "ws://127.0.0.1:3000/websocket");
    }

    /// Live end-to-end: REST login, DDP handshake + resume login, subscribe
    /// the self-DM, send one message through REST (the only permitted write
    /// target), and expect the live stream to deliver it. Ignored by
    /// default; run with RCHAT_LIVE_URL / RCHAT_USER / RCHAT_PASSWORD /
    /// RCHAT_SELF_DM set: `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "live network probe"]
    fn live_ddp_delivers_a_self_dm_message() {
        let url = std::env::var("RCHAT_LIVE_URL")
            .unwrap_or_else(|_| "https://rocketchat-student.21-school.ru".to_owned());
        let settings = crate::rc::RcSettings::parse(&url).unwrap();
        let user = std::env::var("RCHAT_USER").unwrap();
        let password = std::env::var("RCHAT_PASSWORD").unwrap();
        let self_dm = std::env::var("RCHAT_SELF_DM").unwrap();

        compio::runtime::Runtime::new().unwrap().block_on(async {
            let transport = crate::http::HttpTransport::new(
                settings.addr(),
                settings.host_header(),
                settings.is_tls(),
            );
            let mut rc = crate::rc::RcClient::with_transport(settings.clone(), transport);
            rc.login(&user, &password).await.unwrap();
            let session = rc.session().unwrap().clone();

            let mut live = DdpLive::connect(&websocket_url(&settings), &session)
                .await
                .expect("ddp connect + resume login");
            live.watch(&self_dm).await.expect("subscribe the room");

            let (events, received) = async_channel::unbounded();
            // The control channel never sends and never closes in the probe.
            let (never_tx, never_rx) = async_channel::unbounded();
            let _ = never_tx;
            compio::runtime::spawn(live.run(events, never_rx, std::time::Duration::from_secs(10)))
                .detach();

            // Let the subscription register server-side, then write through
            // REST; the live stream must deliver what REST already returned.
            sleep(std::time::Duration::from_secs(1)).await;
            let sent = rc
                .send(&self_dm, "rchat-blazar live probe")
                .await
                .expect("rest send");

            let delivered = futures_lite::future::or(async { received.recv().await.ok() }, async {
                sleep(std::time::Duration::from_secs(15)).await;
                None
            })
            .await
            .expect("the live stream delivered the message within 15s");
            assert_eq!(delivered.id, sent.id, "same message, live-delivered");
            assert_eq!(delivered.room_id, self_dm);
            println!("live-delivered: {}: {}", delivered.sender, delivered.text);
        });
    }
}
