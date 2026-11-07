//! The application layer: the chat command/event boundary, the pure chat
//! state fold, and the compio thread that ties the protocol client, the
//! control socket and the channels together.
//!
//! The compio driver is thread-local, so this layer owns one std thread with
//! its `compio::runtime::Runtime`; GPUI never blocks on it. Both directions
//! cross through `async-channel`, which is executor-agnostic — channels, not
//! nested executors. tako serves the control socket
//! (`$XDG_RUNTIME_DIR/rchat-blazar.sock`) so background tooling can drive and
//! verify the app without the compositor.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use tako::body::TakoBody;
use tako::extractors::json::Json as JsonExtractor;
use tako::responder::Responder;
use tako::router::Router;
use tako::types::Response;
use tako::{CompioServer, StatusCode, header};

use crate::rc::{DdpLive, LiveCommand, Msg, RcClient, RcError, RcSettings, Room, websocket_url};
use crate::store::Store;

/// What the UI (or the control socket) asks the application to do.
pub enum ChatCommand {
    Login { user: String, password: String },
    RefreshRooms,
    OpenRoom { room_id: String },
    Send { room_id: String, text: String },
}

/// What the application reports back; the UI folds these into [`ChatState`].
pub enum ChatEvent {
    Failed(String),
    LoggedIn {
        username: String,
    },
    Rooms {
        rooms: Vec<Room>,
    },
    Messages {
        room_id: String,
        messages: Vec<Msg>,
    },
    Sent {
        message: Msg,
    },
    /// A message delivered by the live WebSocket stream (M2 S2).
    LiveMessage(Msg),
    Quit,
}

/// The pure chat state: every [`ChatEvent`] folds in here, and the fold may
/// return the follow-up command (rooms refresh after login). No GPUI, no
/// I/O — unit-testable on its own.
#[derive(Default)]
pub struct ChatState {
    pub username: Option<String>,
    pub rooms: Vec<Room>,
    pub active_room: Option<String>,
    pub messages: std::collections::HashMap<String, Vec<Msg>>,
    pub error: Option<String>,
}

impl ChatState {
    /// Folds one event; returns the command the app should issue next.
    pub fn apply(&mut self, event: ChatEvent) -> Option<ChatCommand> {
        match event {
            ChatEvent::Failed(message) => {
                self.error = Some(message);
                None
            }
            ChatEvent::LoggedIn { username } => {
                self.username = Some(username);
                self.error = None;
                Some(ChatCommand::RefreshRooms)
            }
            ChatEvent::Rooms { rooms } => {
                self.rooms = rooms;
                None
            }
            ChatEvent::Messages { room_id, messages } => {
                self.messages.insert(room_id.clone(), messages);
                self.active_room = Some(room_id);
                self.error = None;
                None
            }
            ChatEvent::Sent { message } => {
                let room_id = message.room_id.clone();
                self.messages.entry(room_id).or_default().push(message);
                None
            }
            ChatEvent::LiveMessage(message) => {
                // The stream may redeliver what REST already folded in:
                // dedup by the message's server id.
                let room_id = message.room_id.clone();
                let entry = self.messages.entry(room_id).or_default();
                if !entry.iter().any(|known| known.id == message.id) {
                    entry.push(message);
                }
                self.error = None;
                None
            }
            ChatEvent::Quit => None,
        }
    }

    /// Messages of the active room, oldest first.
    pub fn active_messages(&self) -> &[Msg] {
        match self.active_room.as_deref() {
            Some(room_id) => self.messages.get(room_id).map(Vec::as_slice).unwrap_or(&[]),
            None => &[],
        }
    }
}

/// Mirror of the window state, written by the UI entity and served by the
/// control socket so background tooling can verify without the compositor.
#[derive(Clone, Default, Serialize)]
pub struct UiSnapshot {
    pub status: String,
    pub username: Option<String>,
    pub rooms: Vec<String>,
    pub active_room: Option<String>,
    pub messages: Vec<String>,
    pub error: Option<String>,
}

impl UiSnapshot {
    pub fn from_state(state: &ChatState) -> Self {
        Self {
            status: match (&state.username, &state.active_room) {
                (Some(username), Some(room)) => format!("{username} in {room}"),
                (Some(username), None) => format!("{username} — no room open"),
                (None, _) => "not logged in".to_owned(),
            },
            username: state.username.clone(),
            rooms: state.rooms.iter().map(|room| room.name.clone()).collect(),
            active_room: state.active_room.clone(),
            messages: state
                .active_messages()
                .iter()
                .map(|message| format!("{}: {}", message.sender, message.text))
                .collect(),
            error: state.error.clone(),
        }
    }
}

/// The UI-facing half of the service boundary.
pub struct ChatService {
    /// UI consumes events produced on the compio thread.
    pub events: Receiver<ChatEvent>,
    /// UI asks the compio thread for work.
    pub commands: Sender<ChatCommand>,
    /// Latest UI state, read by the control socket.
    pub snapshot: Arc<Mutex<Option<UiSnapshot>>>,
}

/// Spawns the compio thread and returns its UI-facing handles.
pub fn spawn(settings: RcSettings) -> ChatService {
    let (events, event_rx) = unbounded();
    let (command_tx, commands) = unbounded();
    let snapshot = Arc::new(Mutex::new(None));
    let thread_snapshot = Arc::clone(&snapshot);
    let thread_commands = command_tx.clone();
    std::thread::Builder::new()
        .name("compio".into())
        .spawn(move || run(settings, events, commands, thread_commands, thread_snapshot))
        .expect("spawn the compio service thread");
    ChatService {
        events: event_rx,
        commands: command_tx,
        snapshot,
    }
}

fn run(
    settings: RcSettings,
    events: Sender<ChatEvent>,
    commands: Receiver<ChatCommand>,
    self_commands: Sender<ChatCommand>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
) {
    let runtime = compio::runtime::Runtime::new().expect("start the compio runtime");
    runtime.block_on(drive(settings, events, commands, self_commands, snapshot));
}

/// Serves the control socket and answers chat commands until the boundary
/// closes (UI dropped). tako spawns accept work on the ambient runtime, so
/// this must run inside `block_on` for the process lifetime.
async fn drive(
    settings: RcSettings,
    events: Sender<ChatEvent>,
    commands: Receiver<ChatCommand>,
    self_commands: Sender<ChatCommand>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
) {
    // Persistence is an optimization: an unreadable store degrades to
    // in-memory-only and never blocks the chat.
    let store = match Store::open() {
        Ok(store) => Some(store),
        Err(error) => {
            eprintln!("rchat-blazar: local store unavailable: {error}");
            None
        }
    };

    // The live layer follows the session (clone before the client moves it)
    // and is started from the loop once a session exists.
    let settings_for_live = settings.clone();
    let mut client = RcClient::new(settings);
    let mut live: Option<Sender<LiveCommand>> = None;

    // Resumable start: stored session and rooms paint the UI instantly; a
    // stale token surfaces as a 401 on the first refresh and gets cleared.
    if let Some(store) = store.as_ref() {
        if let Some(session) = store.load_session() {
            client.set_session(session.clone());
            let _ = events
                .send(ChatEvent::LoggedIn {
                    username: session.username,
                })
                .await;
            let _ = self_commands.try_send(ChatCommand::RefreshRooms);
        }
        if let Some(rooms) = store.load_rooms() {
            let _ = events.send(ChatEvent::Rooms { rooms }).await;
        }
    }

    let mut control = Router::new();
    control.get("/state", {
        let snapshot = Arc::clone(&snapshot);
        move || {
            let snapshot = Arc::clone(&snapshot);
            async move { JsonBody(state_json(&snapshot)) }
        }
    });
    control.post("/login", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<CredentialsBody>| {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(
                    &self_commands,
                    ChatCommand::Login {
                        user: body.user,
                        password: body.password,
                    },
                ) {
                    (StatusCode::OK, JsonBody(r#"{"queued":true}"#.to_owned()))
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/refresh", {
        let self_commands = self_commands.clone();
        move || {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(&self_commands, ChatCommand::RefreshRooms) {
                    (StatusCode::OK, JsonBody(r#"{"queued":true}"#.to_owned()))
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/open", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<RoomBody>| {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(&self_commands, ChatCommand::OpenRoom { room_id: body.room }) {
                    (StatusCode::OK, JsonBody(r#"{"queued":true}"#.to_owned()))
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/send", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<SendBody>| {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(
                    &self_commands,
                    ChatCommand::Send {
                        room_id: body.room,
                        text: body.text,
                    },
                ) {
                    (StatusCode::OK, JsonBody(r#"{"queued":true}"#.to_owned()))
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/quit", {
        let events = events.clone();
        move || {
            let events = events.clone();
            async move {
                if enqueue(&events, ChatEvent::Quit) {
                    (StatusCode::OK, JsonBody(r#"{"quitting":true}"#.to_owned()))
                } else {
                    unavailable(r#"{"quitting":false}"#)
                }
            }
        }
    });

    let server = CompioServer::builder().build();
    if let Err(error) = server
        .try_spawn_unix_http(control_socket_path(), control)
        .await
    {
        // Background tooling has no handle; the UI loop still works.
        eprintln!("rchat-blazar: control socket unavailable: {error}");
    }

    // Convenience: credentials from the environment log in at startup.
    if let (Ok(user), Ok(password)) = (std::env::var("RCHAT_USER"), std::env::var("RCHAT_PASSWORD"))
    {
        let _ = self_commands.try_send(ChatCommand::Login { user, password });
    }

    while let Ok(command) = commands.recv().await {
        let event = execute(&mut client, store.as_ref(), command).await;
        match &event {
            ChatEvent::LoggedIn { .. } if live.is_none() => {
                if let Some(session) = store.as_ref().and_then(|store| store.load_session()) {
                    live = Some(spawn_live(
                        settings_for_live.clone(),
                        session,
                        events.clone(),
                    ));
                } else {
                    eprintln!("rchat-blazar: live layer disabled: no stored session");
                }
            }
            ChatEvent::Messages { room_id, .. } => {
                if let Some(live) = live.as_ref() {
                    let _ = live.try_send(LiveCommand::Watch(room_id.clone()));
                }
            }
            _ => {}
        }
        let _ = events.send(event).await;
    }
}

/// Starts the live WebSocket connection in its own task on the compio
/// thread; returns the control channel. Failures degrade to a stderr note —
/// the REST loop keeps working without the live layer.
fn spawn_live(
    settings: RcSettings,
    session: crate::rc::Session,
    events: Sender<ChatEvent>,
) -> Sender<LiveCommand> {
    let (commands, receiver) = async_channel::unbounded();
    compio::runtime::spawn(async move {
        let url = websocket_url(&settings);
        match DdpLive::connect(&url, &session).await {
            Ok(live) => {
                // The protocol layer emits raw messages; the application
                // boundary wraps them into chat events.
                let (messages, message_rx) = async_channel::unbounded();
                let forward = compio::runtime::spawn(async move {
                    while let Ok(message) = message_rx.recv().await {
                        if events.send(ChatEvent::LiveMessage(message)).await.is_err() {
                            break;
                        }
                    }
                });
                // The messages sender moves into run() and drops when the
                // connection ends, which unblocks the forward task below.
                let result = live
                    .run(messages, receiver, std::time::Duration::from_secs(30))
                    .await;
                let _ = forward.await;
                if let Err(error) = result {
                    eprintln!("rchat-blazar: live connection ended: {error}");
                }
            }
            Err(error) => eprintln!("rchat-blazar: live layer unavailable: {error}"),
        }
    })
    .detach(); // background task; failures degrade to the stderr notes above
    commands
}

/// One command → one event; failures always surface as `Failed`. `store`
/// persists the session and room cache; a 401 clears the stored session.
async fn execute(client: &mut RcClient, store: Option<&Store>, command: ChatCommand) -> ChatEvent {
    match command {
        ChatCommand::Login { user, password } => match client.login(&user, &password).await {
            Ok(session) => {
                if let Some(store) = store
                    && let Err(error) = store.save_session(&session)
                {
                    eprintln!("rchat-blazar: session not stored: {error}");
                }
                ChatEvent::LoggedIn {
                    username: session.username,
                }
            }
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
        ChatCommand::RefreshRooms => match client.rooms().await {
            Ok(rooms) => {
                if let Some(store) = store
                    && let Err(error) = store.save_rooms(&rooms)
                {
                    eprintln!("rchat-blazar: rooms not cached: {error}");
                }
                ChatEvent::Rooms { rooms }
            }
            Err(RcError::Http { status: 401, body }) => {
                // The stored token stopped working: forget it and ask for a
                // fresh login instead of retrying forever.
                if let Some(store) = store {
                    let _ = store.clear_session();
                }
                ChatEvent::Failed(format!("session rejected (401): {body} — log in again"))
            }
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
        ChatCommand::OpenRoom { room_id } => match client.history(&room_id).await {
            Ok(messages) => ChatEvent::Messages { room_id, messages },
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
        ChatCommand::Send { room_id, text } => match client.send(&room_id, &text).await {
            Ok(message) => ChatEvent::Sent { message },
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
    }
}

/// Queues one item; `false` means the receiving side is gone.
fn enqueue<T>(queue: &Sender<T>, item: T) -> bool {
    queue.try_send(item).is_ok()
}

fn unavailable(body: &str) -> (StatusCode, JsonBody) {
    (StatusCode::SERVICE_UNAVAILABLE, JsonBody(body.to_owned()))
}

/// Control socket under the runtime dir; one process owns the fixed name per
/// user, so a second instance fails to bind instead of shadowing the first.
fn control_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("rchat-blazar.sock")
}

/// JSON view of the mirrored UI state; poisoned locks mean a UI-thread panic,
/// and stale data is still the best available answer.
fn state_json(snapshot: &Mutex<Option<UiSnapshot>>) -> String {
    let state = snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match state.as_ref() {
        Some(state) => serde_json::to_string(state).unwrap_or_default(),
        None => r#"{"status":"no window reported yet"}"#.to_owned(),
    }
}

#[derive(Deserialize)]
struct CredentialsBody {
    user: String,
    password: String,
}

#[derive(Deserialize)]
struct RoomBody {
    room: String,
}

#[derive(Deserialize)]
struct SendBody {
    room: String,
    text: String,
}

/// A JSON responder with the content type set.
struct JsonBody(String);

impl Responder for JsonBody {
    fn into_response(self) -> Response {
        let mut response = Response::new(TakoBody::from(self.0));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        response
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    fn room(id: &str, name: &str) -> Room {
        Room {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: 'c',
        }
    }

    fn message(id: &str, sender: &str, text: &str) -> Msg {
        Msg {
            id: id.to_owned(),
            room_id: "GENERAL".to_owned(),
            sender: sender.to_owned(),
            text: text.to_owned(),
            ts: String::new(),
        }
    }

    #[test]
    fn login_requests_a_rooms_refresh() {
        let mut state = ChatState::default();
        let follow_up = state.apply(ChatEvent::LoggedIn {
            username: "tester".to_owned(),
        });
        assert_eq!(state.username.as_deref(), Some("tester"));
        assert!(matches!(follow_up, Some(ChatCommand::RefreshRooms)));
    }

    #[test]
    fn messages_fold_per_room_and_sent_appends() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::Messages {
            room_id: "GENERAL".to_owned(),
            messages: vec![message("m1", "tester", "first")],
        });
        assert_eq!(state.active_room.as_deref(), Some("GENERAL"));

        state.apply(ChatEvent::Sent {
            message: message("m2", "tester", "second"),
        });
        let messages = state.active_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "first");
        assert_eq!(messages[1].text, "second");
    }

    #[test]
    fn failures_surface_without_clobbering_rooms() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::Rooms {
            rooms: vec![room("GENERAL", "general")],
        });
        state.apply(ChatEvent::Failed("connection refused".to_owned()));
        assert_eq!(state.error.as_deref(), Some("connection refused"));
        assert_eq!(state.rooms.len(), 1);
    }

    #[test]
    fn snapshot_names_rooms_and_messages() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::LoggedIn {
            username: "tester".to_owned(),
        });
        state.apply(ChatEvent::Rooms {
            rooms: vec![room("GENERAL", "general")],
        });
        state.apply(ChatEvent::Messages {
            room_id: "GENERAL".to_owned(),
            messages: vec![message("m1", "tester", "hello")],
        });

        let snapshot = UiSnapshot::from_state(&state);
        assert_eq!(snapshot.username.as_deref(), Some("tester"));
        assert_eq!(snapshot.rooms, vec!["general".to_owned()]);
        assert_eq!(snapshot.messages, vec!["tester: hello".to_owned()]);
        assert!(snapshot.status.contains("tester"));
    }
}
