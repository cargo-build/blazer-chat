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

use super::Notification;
use serde_json::Value;

use crate::rc::{Msg, RcError, Session};
use async_channel::{Receiver, Sender};
use compio::runtime::time::sleep;
use compio_ws::tungstenite::{Error as WsError, Message};
use compio_ws::{WebSocketStream, connect_async_tls_with_config};

/// The collection that carries the watched room's chat messages.
const ROOM_STREAM: &str = "stream-room-messages";

/// The collection that carries per-user updates; we subscribe the
/// `subscriptions-changed` event for unread badges.
const NOTIFY_STREAM: &str = "stream-notify-user";

/// The notify event carrying per-room unread counts.
const SUBSCRIPTIONS_CHANGED: &str = "subscriptions-changed";
/// The notify event carrying popups for messages outside the active
/// room.
const NOTIFICATION_EVENT: &str = "notification";

/// One live update: a chat message from the watched room, or an unread
/// count from the user's subscription stream.
#[derive(Debug)]
pub enum LiveEvent {
    Message(Box<Msg>),
    Unread { room_id: String, unread: u32 },
    Notification(super::Notification),
}

/// One unit of work for the live loop: a decoded wire frame, the keepalive
/// tick, or a control command from the application.
#[derive(Debug)]
enum Frame {
    /// DDP-level ping from the server — answer with a pong.
    Ping,
    /// A decoded chat message from the watched room's stream.
    RoomMessage(Box<Msg>),
    /// An unread-count update from the user's subscription stream.
    Notify { room_id: String, unread: u32 },
    /// A popup-grade notification for the user.
    Notification(super::Notification),
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
    /// The id of the always-on `subscriptions-changed` subscription.
    notify_sub: Option<String>,
    /// The id of the always-on `notification` subscription.
    notification_sub: Option<String>,
    /// The watched room, remembered across reconnects (resubscribe).
    watched: Option<String>,
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
            notify_sub: None,
            notification_sub: None,
            watched: None,
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
        // The unread stream is per-user and always on: the initial burst
        // carries every room's current unread count.
        let id = live.next_id();
        live.send_json(serde_json::json!({
            "msg": "sub", "id": id, "name": NOTIFY_STREAM,
            "params": [format!("{}/{}", session.user_id, SUBSCRIPTIONS_CHANGED)],
        }))
        .await?;
        live.notify_sub = Some(id.to_owned());
        // The per-user notification stream feeds the popups.
        let id = live.next_id();
        live.send_json(serde_json::json!({
            "msg": "sub", "id": id, "name": NOTIFY_STREAM,
            "params": [format!("{}/{}", session.user_id, NOTIFICATION_EVENT)],
        }))
        .await?;
        live.notification_sub = Some(id.to_owned());
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
        &mut self,
        events: Sender<LiveEvent>,
        commands: Receiver<LiveCommand>,
        keepalive: std::time::Duration,
    ) -> Result<(), RcError> {
        loop {
            match self.next_frame(keepalive, &commands).await? {
                Frame::RoomMessage(message) => {
                    let _ = events.send(LiveEvent::Message(message)).await;
                }
                Frame::Notify { room_id, unread } => {
                    let _ = events.send(LiveEvent::Unread { room_id, unread }).await;
                }
                Frame::Notification(note) => {
                    let _ = events.send(LiveEvent::Notification(note)).await;
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
        self.watched = Some(room_id.to_owned());
        Ok(())
    }

    /// The watched room, to resubscribe after a reconnect.
    pub fn watched(&self) -> Option<&str> {
        self.watched.as_deref()
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
                Some(message) => Frame::RoomMessage(Box::new(message)),
                None => Frame::Document(document),
            }
        }
        // The per-user notify stream: `subscriptions-changed` sets the
        // unread badges (args = [type, subscription]; the initial burst
        // after subscribing carries every room), `notification` carries
        // popups for messages outside the active room.
        Some("changed")
            if document.get("collection").and_then(Value::as_str) == Some(NOTIFY_STREAM) =>
        {
            let fields = document.get("fields");
            let event = fields
                .and_then(|fields| fields.get("eventName"))
                .and_then(Value::as_str)
                .and_then(|event| event.rsplit('/').next())
                .unwrap_or_default();
            match event {
                NOTIFICATION_EVENT => fields
                    .and_then(|fields| fields.get("args"))
                    .and_then(notification_from_args)
                    .map_or_else(|| Frame::Document(document), Frame::Notification),
                SUBSCRIPTIONS_CHANGED => {
                    let notify = fields.and_then(|fields| {
                        let args = fields.get("args")?.as_array()?;
                        let kind = args.first()?.as_str()?;
                        if !matches!(kind, "updated" | "inserted") {
                            return None;
                        }
                        let subscription = args.get(1)?;
                        Some((
                            subscription.get("rid")?.as_str()?.to_owned(),
                            subscription.get("unread")?.as_u64()? as u32,
                        ))
                    });
                    match notify {
                        Some((room_id, unread)) => Frame::Notify { room_id, unread },
                        None => Frame::Document(document),
                    }
                }
                _ => Frame::Document(document),
            }
        }
        _ => Frame::Document(document),
    }
}

/// Parses `args[0]` of a `notification` event. Wire shape captured in
/// the vendored ruqola client: title/text on the envelope, the message
/// identity under `payload`.
fn notification_from_args(args: &Value) -> Option<Notification> {
    let arg = args.as_array()?.first()?;
    let payload = arg.get("payload")?;
    Some(Notification {
        title: arg
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        text: arg
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        room_id: payload.get("rid")?.as_str()?.to_owned(),
        sender: payload
            .pointer("/sender/username")
            .and_then(Value::as_str)
            .map(str::to_owned),
        room_type: payload
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        message_id: payload
            .get("_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tmid: payload
            .get("tmid")
            .and_then(Value::as_str)
            .map(str::to_owned),
        message: payload
            .pointer("/message/msg")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
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
                    let usernames = reaction
                        .get("usernames")?
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    (!usernames.is_empty()).then(|| {
                        (
                            name.trim_matches(':').to_owned(),
                            crate::rc::Reaction { usernames },
                        )
                    })
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
        tmid: message
            .get("tmid")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tcount: message
            .get("tcount")
            .and_then(Value::as_u64)
            .map(|n| n as u32),
        tlm: message
            .get("tlm")
            .and_then(Value::as_str)
            .map(str::to_owned),
        file: None,
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

    /// The probes' configuration: live URL (or the school server) and
    /// the self-DM — the only permitted write target (AGENTS.md
    /// live-send policy).
    fn probe_env() -> (crate::rc::RcSettings, String) {
        let url = std::env::var("RCHAT_LIVE_URL")
            .unwrap_or_else(|_| "https://rocketchat-student.21-school.ru".to_owned());
        let self_dm = std::env::var("RCHAT_SELF_DM").unwrap();
        (crate::rc::RcSettings::parse(&url).unwrap(), self_dm)
    }

    /// REST login + DDP connect; runs inside the probe's runtime.
    async fn probe_harness(
        settings: &crate::rc::RcSettings,
    ) -> (crate::rc::RcClient<crate::http::HttpTransport>, DdpLive) {
        let user = std::env::var("RCHAT_USER").unwrap();
        let password = std::env::var("RCHAT_PASSWORD").unwrap();
        let transport = crate::http::HttpTransport::new(
            settings.addr(),
            settings.host_header(),
            settings.is_tls(),
        );
        let mut rc = crate::rc::RcClient::with_transport(settings.clone(), transport);
        rc.login(&user, &password).await.unwrap();
        let session = rc.session().unwrap().clone();
        let live = DdpLive::connect(&websocket_url(settings), &session)
            .await
            .expect("ddp connect + resume login");
        (rc, live)
    }

    /// Drains the wire for `window`, answering server pings and skipping
    /// pong frames. Bounded, so a probe never dies on the unanswered-ping
    /// close.
    async fn drain_frames(live: &mut DdpLive, window: std::time::Duration) -> Vec<Frame> {
        let deadline = std::time::Instant::now() + window;
        let mut frames = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break frames;
            }
            match live.next_wire_frame(remaining).await {
                Ok(Frame::Ping) => {
                    let _ = live.reply_pong(None).await;
                }
                Ok(Frame::Document(document))
                    if document.get("msg").and_then(Value::as_str) == Some("pong") => {}
                Ok(frame) => frames.push(frame),
                Err(_) => break frames,
            }
        }
    }

    /// subscriptions-changed decodes into an unread update; foreign
    /// events and removed-type args fall through to Document.
    #[test]
    fn notify_frames_decode_into_unread_updates() {
        let frame = decode_document(serde_json::json!({
            "msg": "changed", "collection": "stream-notify-user",
            "fields": {
                "eventName": "27wWpti9Nii4yRxac/subscriptions-changed",
                "args": ["updated", {"rid": "dm1", "unread": 3}]
            }
        }));
        match frame {
            Frame::Notify { room_id, unread } => {
                assert_eq!(room_id, "dm1");
                assert_eq!(unread, 3);
            }
            other => panic!("expected Notify, got {other:?}"),
        }

        let removed = decode_document(serde_json::json!({
            "msg": "changed", "collection": "stream-notify-user",
            "fields": {
                "eventName": "id/subscriptions-changed",
                "args": ["removed", {"rid": "dm1"}]
            }
        }));
        assert!(matches!(removed, Frame::Document(_)));

        let foreign = decode_document(serde_json::json!({
            "msg": "changed", "collection": "stream-notify-user",
            "fields": {
                "eventName": "id/message",
                "args": [{"rid": "dm1"}]
            }
        }));
        assert!(matches!(foreign, Frame::Document(_)));
    }

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

    /// The notification event decodes per the wire shape captured in
    /// the vendored ruqola client (`rocketchataccount.cpp`).
    #[test]
    fn notification_events_decode_with_the_documented_shape() {
        let frame = decode_document(serde_json::json!({
            "msg": "changed", "collection": "stream-notify-user",
            "id": "id4",
            "fields": {
                "eventName": "MYUID/notification",
                "args": [{
                    "title": "laurent",
                    "text": "",
                    "payload": {
                        "_id": "vD3CXNB2oK5uB8nDT",
                        "message": {"msg": "hello there"},
                        "rid": "DMROOM",
                        "sender": {"_id": "SENDER", "name": "laurent",
                                   "username": "laurent-montel"},
                        "type": "d"
                    }
                }]
            }
        }));
        match frame {
            Frame::Notification(note) => {
                assert_eq!(note.title, "laurent");
                assert_eq!(note.room_id, "DMROOM");
                assert_eq!(note.sender.as_deref(), Some("laurent-montel"));
                assert_eq!(note.message.as_deref(), Some("hello there"));
                assert_eq!(note.room_type.as_deref(), Some("d"));
                assert_eq!(note.message_id.as_deref(), Some("vD3CXNB2oK5uB8nDT"));
            }
            other => panic!("expected Notification, got {other:?}"),
        }
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
    /// Live: the `subscriptions-changed` stream must carry unread counts —
    /// a REST send into the self-DM (the only permitted write target)
    /// while another room id is reported must surface as a Notify frame.
    #[test]
    #[ignore = "live network probe"]
    fn live_notify_stream_delivers_unread_counts() {
        let (settings, self_dm) = probe_env();
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let (rc, mut live) = probe_harness(&settings).await;
            live.watch(&self_dm).await.expect("subscribe the room");

            // One send while the stream is up: the subscription burst plus
            // the send's own unread update must arrive.
            sleep(std::time::Duration::from_secs(2)).await;
            rc.send(&self_dm, "notify probe").await.expect("rest send");

            let saw_notify = drain_frames(&mut live, std::time::Duration::from_secs(30))
                .await
                .iter()
                .any(|frame| matches!(frame, Frame::Notify { room_id, .. } if room_id == &self_dm));
            assert!(
                saw_notify,
                "no Notify frame for the self-DM within the window"
            );
        });
    }

    /// Live: the `notification` event shape. A REST send into the self-DM
    /// (the only permitted write target) while a different room is watched
    /// must surface a `stream-notify-user/{uid}/notification` document;
    /// the raw args are printed for the decode's fixture. Ignored by
    /// default; same env as the other probes.
    #[test]
    #[ignore = "live network probe"]
    fn live_notification_stream_delivers_the_documented_shape() {
        let (settings, self_dm) = probe_env();
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let (mut rc, mut live) = probe_harness(&settings).await;
            // Watch a room other than the sender: notifications fire for
            // messages the user is not looking at.
            let other = rc
                .rooms()
                .await
                .expect("rooms")
                .into_iter()
                .find(|room| room.id != self_dm)
                .expect("a non-self room");
            live.watch(&other.id).await.expect("subscribe the room");

            sleep(std::time::Duration::from_secs(2)).await;
            rc.send(&self_dm, "notification probe")
                .await
                .expect("rest send");

            // The handshake subscribed {uid}/notification. Servers may
            // skip notifying a message's own sender, so a self-DM send
            // proves only liveness here: the observation is recorded, the
            // wire shape is pinned by the unit fixture.
            let frames = drain_frames(&mut live, std::time::Duration::from_secs(20)).await;
            let mut notified = false;
            for frame in &frames {
                if let Frame::Notification(note) = frame {
                    println!("notification: {note:?}");
                    notified = true;
                }
            }
            assert!(!frames.is_empty(), "the wire closed within the window");
            println!("self-send produced a notification event: {notified}");
        });
    }

    #[test]
    #[ignore = "live network probe"]
    fn live_ddp_delivers_a_self_dm_message() {
        let (settings, self_dm) = probe_env();
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let (rc, mut live) = probe_harness(&settings).await;
            let (events, received) = async_channel::unbounded::<LiveEvent>();
            // The control channel never sends and never closes in the probe.
            let (never_tx, never_rx) = async_channel::unbounded();
            let _ = never_tx;
            live.watch(&self_dm).await.expect("subscribe the room");
            let run = async move {
                live.run(events, never_rx, std::time::Duration::from_secs(10))
                    .await
            };
            compio::runtime::spawn(run).detach();

            // Let the subscription register server-side, then write through
            // REST; the live stream must deliver what REST already returned.
            sleep(std::time::Duration::from_secs(1)).await;
            let sent = rc
                .send(&self_dm, "rchat-blazar live probe")
                .await
                .expect("rest send");

            // Unread bursts from subscriptions-changed may interleave;
            // wait for the probe message itself.
            let delivered = futures_lite::future::or(
                async {
                    loop {
                        match received.recv().await.ok()? {
                            LiveEvent::Message(message) if message.id == sent.id => {
                                break Some(message);
                            }
                            _ => continue,
                        }
                    }
                },
                async {
                    sleep(std::time::Duration::from_secs(15)).await;
                    None
                },
            )
            .await
            .expect("the live stream delivered the message within 15s");
            assert_eq!(delivered.id, sent.id, "same message, live-delivered");
            assert_eq!(delivered.room_id, self_dm);
            println!("live-delivered: {}: {}", delivered.sender, delivered.text);
        });
    }
}
