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

use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender, unbounded};
use serde::Serialize;

use crate::control;
use crate::rc::{AttachedFile, LiveCommand, Msg, RcClient, RcError, RcSettings, Room};
use crate::store::Store;
use crate::tasks::{spawn_emoji, spawn_live};

/// What the UI (or the control socket) asks the application to do.
#[derive(Debug)]
pub enum ChatCommand {
    Login {
        user: String,
        password: String,
    },
    RefreshRooms,
    OpenRoom {
        room_id: String,
    },
    Send {
        room_id: String,
        text: String,
    },
    EditMessage {
        room_id: String,
        msg_id: String,
        text: String,
    },
    DeleteMessage {
        room_id: String,
        msg_id: String,
    },
    React {
        room_id: String,
        msg_id: String,
        emoji: String,
    },
    /// Uploads a local file into the room and posts it as a message
    /// (two-phase rooms.media + mediaConfirm).
    UploadFile {
        room_id: String,
        path: String,
        msg: String,
    },
    /// Fetches avatars for the given usernames into the cache (cache
    /// first; missing ones download).
    FetchAvatars {
        usernames: Vec<String>,
    },
    /// Sets or clears the local search query (empty clears). Search runs
    /// over cached messages only — core Rocket.Chat has no server-side
    /// message search, and neither does the original client.
    Search(String),
    /// Opens a thread view: fetches the thread's messages.
    OpenThread {
        tmid: String,
    },
    /// Closes the thread view.
    CloseThread,
    /// Posts a thread reply.
    SendThread {
        room_id: String,
        tmid: String,
        text: String,
    },
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
    /// The server accepted an edit and answered with the fresh message.
    MessageUpdated {
        message: Msg,
    },
    /// The server accepted the deletion.
    MessageDeleted {
        room_id: String,
        msg_id: String,
    },
    /// A popup-grade notification for the user (M4).
    Notification(crate::rc::Notification),
    /// Cached avatar paths for message senders.
    Avatars {
        avatars: Vec<(String, String)>,
    },
    /// The server accepted the reaction toggle; the fold applies it
    /// locally until the live stream redelivers the message.
    ReactionToggled {
        room_id: String,
        msg_id: String,
        emoji: String,
    },
    /// The user's `subscriptions-changed` stream reported a room's
    /// unread count (initial burst + every new message).
    UnreadCount {
        room_id: String,
        unread: u32,
    },
    /// Local echo of [`ChatCommand::Search`]; empty clears the results.
    SearchQuery(String),
    /// The thread's messages arrived; the pane switches to the thread.
    ThreadMessages {
        room_id: String,
        tmid: String,
        messages: Vec<Msg>,
    },
    /// The thread view closed; the pane returns to the room.
    ThreadClosed,
    /// The live layer connected or dropped.
    LiveStatus(bool),
    /// The custom-emoji cache: (name, image path) pairs.
    Emoji {
        emojis: Vec<(String, String)>,
    },
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
    /// Custom emoji: name → cached image path.
    pub emoji: std::collections::BTreeMap<String, String>,
    /// Unread counts that arrived before the room list (the DDP burst
    /// beats the REST `rooms.get`); applied when rooms fold in.
    pending_unread: std::collections::HashMap<String, u32>,
    /// Active local search query; `Some` switches the pane to results.
    pub search_query: Option<String>,
    /// Open thread view: `(room_id, tmid)`; the pane shows the thread.
    pub open_thread: Option<(String, String)>,
    /// Whether the live WebSocket is connected.
    pub live_connected: bool,
    /// Fetched threads by their root message id, oldest first.
    threads: std::collections::HashMap<String, Vec<Msg>>,
    /// Notifications for rooms other than the active one, pending
    /// display (newest last, bounded).
    pub notifications: Vec<crate::rc::Notification>,
    /// Sender username → cached avatar path.
    pub avatars: std::collections::HashMap<String, String>,
}

/// One local-search hit.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub room_id: String,
    pub room: String,
    pub sender: String,
    pub text: String,
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
                // Apply unread counts that arrived before the list did.
                for room in &mut self.rooms {
                    if let Some(unread) = self.pending_unread.remove(&room.id) {
                        room.unread = unread;
                    }
                }
                None
            }
            ChatEvent::Messages { room_id, messages } => {
                self.messages.insert(room_id.clone(), messages);
                // The room is on screen now; the server-side read receipt
                // went out with the history call.
                if let Some(room) = self.rooms.iter_mut().find(|room| room.id == room_id) {
                    room.unread = 0;
                }
                // Senders without a cached avatar fetch one now; the
                // event follows the history into the pane.
                let missing: Vec<String> = self
                    .messages
                    .get(&room_id)
                    .map(|messages| {
                        messages
                            .iter()
                            .map(|message| message.sender.clone())
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .filter(|sender| !self.avatars.contains_key(sender))
                            .collect()
                    })
                    .unwrap_or_default();
                self.active_room = Some(room_id);
                self.error = None;
                (!missing.is_empty()).then_some(ChatCommand::FetchAvatars { usernames: missing })
            }
            ChatEvent::Avatars { avatars } => {
                self.avatars.extend(avatars.iter().cloned());
                None
            }
            ChatEvent::Sent { message } => {
                self.bump_room_activity(&message.room_id, message.ts.clone());
                match message.tmid.clone() {
                    // Thread replies surface in their thread view only.
                    Some(tmid) => {
                        self.threads.entry(tmid).or_default().push(message);
                    }
                    None => {
                        let room_id = message.room_id.clone();
                        self.messages.entry(room_id).or_default().push(message);
                    }
                }
                None
            }
            ChatEvent::LiveMessage(message) => {
                self.bump_room_activity(&message.room_id, message.ts.clone());
                // The stream redelivers what REST already folded in and
                // re-sends edited messages under the same id: replace by
                // the server id, newest delivery wins.
                let room_id = message.room_id.clone();
                let entry = self.messages.entry(room_id).or_default();
                match entry.iter_mut().find(|known| known.id == message.id) {
                    Some(known) => *known = message,
                    None => entry.push(message),
                }
                self.error = None;
                None
            }
            ChatEvent::MessageUpdated { message } => {
                let room_id = message.room_id.clone();
                if let Some(room) = self.messages.get_mut(&room_id)
                    && let Some(known) = room.iter_mut().find(|known| known.id == message.id)
                {
                    *known = message;
                }
                None
            }
            ChatEvent::MessageDeleted { room_id, msg_id } => {
                if let Some(room) = self.messages.get_mut(&room_id) {
                    room.retain(|known| known.id != msg_id);
                }
                None
            }
            ChatEvent::ReactionToggled {
                room_id,
                msg_id,
                emoji,
            } => {
                // Local toggle until the authoritative live redelivery.
                if let Some(room) = self.messages.get_mut(&room_id)
                    && let Some(message) = room.iter_mut().find(|known| known.id == msg_id)
                {
                    let count = message.reactions.entry(emoji.clone()).or_default();
                    match *count {
                        0 => *count = 1,
                        1 => {
                            message.reactions.remove(&emoji);
                        }
                        past => *count = past - 1,
                    }
                }
                None
            }
            ChatEvent::UnreadCount { room_id, unread } => {
                match self.rooms.iter_mut().find(|room| room.id == room_id) {
                    Some(room) => room.unread = unread,
                    // The room list is still in flight — remember until it
                    // arrives.
                    None => {
                        self.pending_unread.insert(room_id, unread);
                    }
                }
                None
            }
            ChatEvent::SearchQuery(query) => {
                self.search_query = (!query.is_empty()).then_some(query);
                None
            }
            ChatEvent::ThreadMessages {
                room_id,
                tmid,
                messages,
            } => {
                // Merge the fetch into any optimistic replies already in
                // the thread; dedup by server id keeps re-fetches stable.
                let entry = self.threads.entry(tmid.clone()).or_default();
                for message in messages {
                    if !entry.iter().any(|known| known.id == message.id) {
                        entry.push(message);
                    }
                }
                self.open_thread = Some((room_id, tmid));
                None
            }
            ChatEvent::ThreadClosed => {
                self.open_thread = None;
                None
            }
            ChatEvent::LiveStatus(connected) => {
                self.live_connected = connected;
                None
            }
            ChatEvent::Emoji { emojis } => {
                self.emoji = emojis.iter().cloned().collect();
                None
            }
            ChatEvent::Notification(note) => {
                // The active room's traffic is on screen already; popups
                // carry only the rest. Bounded: the oldest fall off.
                if self.active_room.as_deref() != Some(note.room_id.as_str()) {
                    self.notifications.push(note);
                    let excess = self.notifications.len().saturating_sub(8);
                    self.notifications.drain(0..excess);
                }
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

    /// The open thread's messages, oldest first.
    pub fn thread(&self, tmid: &str) -> &[Msg] {
        self.threads.get(tmid).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Room activity bump: new messages raise the room to the top.
    fn bump_room_activity(&mut self, room_id: &str, ts: String) {
        if let Some(room) = self.rooms.iter_mut().find(|room| room.id == room_id) {
            room.lm = Some(ts);
            self.rooms
                .sort_by(|a, b| b.lm.as_deref().cmp(&a.lm.as_deref()));
        }
    }

    /// Case-insensitive substring search over the cached messages (the
    /// original client's local search covers its cache too); results in
    /// room-list order, capped.
    pub fn search(&self, query: &str) -> Vec<SearchHit> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for room in &self.rooms {
            let Some(messages) = self.messages.get(&room.id) else {
                continue;
            };
            for message in messages {
                if message.text.to_lowercase().contains(&needle) {
                    hits.push(SearchHit {
                        room_id: room.id.clone(),
                        room: room.name.clone(),
                        sender: message.sender.clone(),
                        text: message.text.clone(),
                    });
                    if hits.len() >= 50 {
                        return hits;
                    }
                }
            }
        }
        hits
    }
}

/// Mirror of the window state, written by the UI entity and served by the
/// control socket so background tooling can verify without the compositor.
#[derive(Clone, Default, Serialize)]
pub struct UiSnapshot {
    pub status: String,
    pub username: Option<String>,
    /// Room display names in list order.
    pub rooms: Vec<String>,
    /// Rooms with a nonzero unread count: `"name ×count"`.
    pub badges: Vec<String>,
    /// Local-search hits while a query is active: `"room — sender: text"`.
    pub search: Vec<String>,
    /// The open thread's messages: `"sender: text"`.
    pub thread: Vec<String>,
    /// Whether the live WebSocket is connected.
    pub live: bool,
    /// Pending notifications: `"title: body"`.
    pub notifications: Vec<String>,
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
            badges: state
                .rooms
                .iter()
                .filter(|room| room.unread > 0)
                .map(|room| format!("{} ×{}", room.name, room.unread))
                .collect(),
            search: match &state.search_query {
                Some(query) => state
                    .search(query)
                    .iter()
                    .map(|hit| format!("{} — {}: {}", hit.room, hit.sender, hit.text))
                    .collect(),
                None => Vec::new(),
            },
            thread: match &state.open_thread {
                Some((_, tmid)) => state
                    .threads
                    .get(tmid)
                    .map(|messages| {
                        messages
                            .iter()
                            .map(|message| format!("{}: {}", message.sender, message.text))
                            .collect()
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            },
            live: state.live_connected,
            active_room: state.active_room.clone(),
            messages: state
                .active_messages()
                .iter()
                .map(|message| {
                    let mut line = match message.reactions.is_empty() {
                        true => format!("{}: {}", message.sender, message.text),
                        false => format!(
                            "{}: {} {}",
                            message.sender,
                            message.text,
                            message
                                .reactions
                                .iter()
                                .map(|(name, count)| format!("[{name} ×{count}]"))
                                .collect::<String>(),
                        ),
                    };
                    if let Some(file) = &message.file {
                        line.push_str(&format!(" [📎 {}]", file.name));
                    }
                    line
                })
                .collect(),
            notifications: state
                .notifications
                .iter()
                .map(|note| {
                    let body = note.message.as_deref().unwrap_or(&note.text);
                    format!("{}: {}", note.title, body)
                })
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
    // in-memory-only and never blocks the chat. Shared so background tasks
    // (emoji cache) can use the same redb handle.
    let store = match Store::open() {
        Ok(store) => Some(std::sync::Arc::new(store)),
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

    // The control socket: background tooling drives and verifies the app.
    control::serve(events.clone(), self_commands.clone(), Arc::clone(&snapshot)).await;

    // Convenience: credentials from the environment log in at startup.
    if let (Ok(user), Ok(password)) = (std::env::var("RCHAT_USER"), std::env::var("RCHAT_PASSWORD"))
    {
        let _ = self_commands.try_send(ChatCommand::Login { user, password });
    }

    while let Ok(command) = commands.recv().await {
        let event = execute(&mut client, store.as_deref(), command).await;
        match &event {
            ChatEvent::LoggedIn { .. } if live.is_none() => {
                if let Some(store) = store.as_ref()
                    && let Some(session) = store.load_session()
                {
                    live = Some(spawn_live(
                        settings_for_live.clone(),
                        session.clone(),
                        events.clone(),
                    ));
                    spawn_emoji(
                        settings_for_live.clone(),
                        session,
                        store.clone(),
                        events.clone(),
                    );
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

/// Fetches one image attachment into the cache, returning the path for
/// the UI. The chip is the fallback when this fails.
async fn fetch_image_preview(
    client: &mut RcClient,
    store: &Store,
    room_id: &str,
    file: &AttachedFile,
) -> Result<std::path::PathBuf, RcError> {
    let bytes = client.file_bytes(&file.path).await?;
    // `/file-upload/{file_id}/{name}` — the id segment, not the
    // dash-riddled display name.
    let file_id = file.path.split('/').nth(2).unwrap_or("file");
    store
        .save_attachment(room_id, file_id, &file.name, &bytes)
        .map_err(RcError::Config)
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
            Ok(mut messages) => {
                // Server-side read receipt: the unread stream stops
                // counting this room. Cosmetic — a refusal is logged, not
                // banner-worthy.
                if let Err(error) = client.mark_read(&room_id).await {
                    eprintln!("rchat-blazar: mark read: {error}");
                }
                // Inline image previews: fetch into the cache; a chip
                // remains the fallback for anything that fails.
                if let Some(store) = store {
                    for message in &mut messages {
                        if let Some(file) = &message.file
                            && file.is_image()
                        {
                            match fetch_image_preview(client, store, &room_id, file).await {
                                Ok(path) => {
                                    if let Some(fresh) = message.file.as_mut() {
                                        fresh.image_path = Some(path.display().to_string());
                                    }
                                }
                                Err(error) => {
                                    eprintln!("rchat-blazar: preview {}: {error}", file.name)
                                }
                            }
                        }
                    }
                }
                ChatEvent::Messages { room_id, messages }
            }
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
        ChatCommand::Send { room_id, text } => match client.send(&room_id, &text).await {
            Ok(message) => ChatEvent::Sent { message },
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
        ChatCommand::EditMessage {
            room_id,
            msg_id,
            text,
        } => match client.update_message(&room_id, &msg_id, &text).await {
            Ok(message) => ChatEvent::MessageUpdated { message },
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
        ChatCommand::DeleteMessage { room_id, msg_id } => {
            match client.delete_message(&room_id, &msg_id).await {
                Ok(()) => ChatEvent::MessageDeleted { room_id, msg_id },
                Err(error) => ChatEvent::Failed(error.to_string()),
            }
        }
        ChatCommand::React {
            room_id,
            msg_id,
            emoji,
        } => match client.react(&msg_id, &emoji).await {
            Ok(()) => ChatEvent::ReactionToggled {
                room_id,
                msg_id,
                emoji,
            },
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
        ChatCommand::UploadFile { room_id, path, msg } => {
            match client.upload_file(&room_id, &path, &msg).await {
                Ok(message) => ChatEvent::Sent { message },
                Err(error) => ChatEvent::Failed(error.to_string()),
            }
        }
        // Local search needs no I/O: the state owns the cache.
        ChatCommand::Search(query) => ChatEvent::SearchQuery(query),
        ChatCommand::OpenThread { tmid } => {
            let tmid_owned = tmid.clone();
            match client.thread_messages(&tmid).await {
                Ok(messages) => match messages.first().map(|message| message.room_id.clone()) {
                    Some(room_id) => ChatEvent::ThreadMessages {
                        room_id,
                        tmid,
                        messages,
                    },
                    None => ChatEvent::Failed(format!(
                        "thread {tmid_owned}: no messages (root not visible)"
                    )),
                },
                Err(error) => ChatEvent::Failed(error.to_string()),
            }
        }
        ChatCommand::CloseThread => ChatEvent::ThreadClosed,
        ChatCommand::FetchAvatars { usernames } => {
            // Avatars render from disk; without a store they degrade to
            // the initial discs.
            let Some(store) = store else {
                return ChatEvent::Avatars {
                    avatars: Vec::new(),
                };
            };
            let mut avatars = Vec::new();
            for username in usernames {
                if let Some(path) = store
                    .avatar_path(&username)
                    .map_err(|error| {
                        eprintln!("rchat-blazar: avatar cache: {error}");
                    })
                    .ok()
                    .flatten()
                {
                    avatars.push((username.clone(), path.display().to_string()));
                    continue;
                }
                if let Ok(bytes) = client.avatar_bytes(&username).await
                    && let Ok(path) = store.save_avatar(&username, &bytes)
                {
                    avatars.push((username, path.display().to_string()));
                }
            }
            ChatEvent::Avatars { avatars }
        }
        ChatCommand::SendThread {
            room_id,
            tmid,
            text,
        } => match client.send_thread(&room_id, &tmid, &text).await {
            Ok(message) => ChatEvent::Sent { message },
            Err(error) => ChatEvent::Failed(error.to_string()),
        },
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
            unread: 0,
            lm: None,
        }
    }

    fn message(id: &str, sender: &str, text: &str) -> Msg {
        Msg {
            id: id.to_owned(),
            room_id: "GENERAL".to_owned(),
            sender: sender.to_owned(),
            text: text.to_owned(),
            ts: String::new(),
            tmid: None,
            tcount: None,
            tlm: None,
            file: None,
            reactions: Default::default(),
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
    fn messages_fold_per_room_and_live_messages_dedup() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::Messages {
            room_id: "GENERAL".to_owned(),
            messages: vec![message("m1", "tester", "first")],
        });
        assert_eq!(state.active_room.as_deref(), Some("GENERAL"));

        // The live stream may redeliver what REST already folded in.
        state.apply(ChatEvent::Sent {
            message: message("m2", "tester", "second"),
        });
        state.apply(ChatEvent::LiveMessage(message("m2", "tester", "second")));
        state.apply(ChatEvent::LiveMessage(message("m3", "tester", "third")));
        let messages = state.active_messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].text, "second");
        assert_eq!(messages[2].text, "third");
    }

    #[test]
    fn edits_replace_and_deletes_remove_by_server_id() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::Messages {
            room_id: "GENERAL".to_owned(),
            messages: vec![
                message("m1", "tester", "first"),
                message("m2", "tester", "old"),
            ],
        });

        let mut edited = message("m2", "tester", "new text");
        edited.reactions.insert("tada".to_owned(), 2);
        state.apply(ChatEvent::MessageUpdated {
            message: edited.clone(),
        });
        let messages = state.active_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text, "new text");
        assert_eq!(messages[1].reactions.get("tada"), Some(&2));

        state.apply(ChatEvent::MessageDeleted {
            room_id: "GENERAL".to_owned(),
            msg_id: "m1".to_owned(),
        });
        let messages = state.active_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "m2");
    }

    #[test]
    fn reaction_toggle_adds_then_removes_locally() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::Messages {
            room_id: "GENERAL".to_owned(),
            messages: vec![message("m1", "tester", "hello")],
        });

        state.apply(ChatEvent::ReactionToggled {
            room_id: "GENERAL".to_owned(),
            msg_id: "m1".to_owned(),
            emoji: "thumbsup".to_owned(),
        });
        state.apply(ChatEvent::ReactionToggled {
            room_id: "GENERAL".to_owned(),
            msg_id: "m1".to_owned(),
            emoji: "thumbsup".to_owned(),
        });
        let messages = state.active_messages();
        assert!(messages[0].reactions.is_empty());
    }

    #[test]
    fn unread_folds_in_and_clears_when_the_room_opens() {
        let mut state = ChatState::default();

        // The DDP unread burst usually arrives before the REST room list:
        // counts buffered while rooms are in flight must surface when the
        // list folds in.
        state.apply(ChatEvent::UnreadCount {
            room_id: "dm1".to_owned(),
            unread: 7,
        });
        state.apply(ChatEvent::Rooms {
            rooms: vec![room("GENERAL", "general"), {
                let mut dm = room("dm1", "alice");
                dm.kind = 'd';
                dm
            }],
        });
        assert_eq!(state.rooms[1].unread, 7, "pre-rooms count applied");

        state.apply(ChatEvent::UnreadCount {
            room_id: "dm1".to_owned(),
            unread: 2,
        });
        assert_eq!(state.rooms[1].unread, 2);

        state.apply(ChatEvent::Messages {
            room_id: "dm1".to_owned(),
            messages: vec![message("m1", "alice", "hi")],
        });
        assert_eq!(state.rooms[1].unread, 0, "opening the room clears it");

        let snapshot = UiSnapshot::from_state(&state);
        assert!(snapshot.badges.is_empty());
    }

    #[test]
    fn thread_replies_stay_in_their_thread_view() {
        let mut state = ChatState::default();

        // A reply carries tmid: it lands in the thread, not the room pane.
        let mut reply = message("r1", "tester", "in thread");
        reply.tmid = Some("root".to_owned());
        state.apply(ChatEvent::Sent { message: reply });
        state.apply(ChatEvent::ThreadMessages {
            room_id: "GENERAL".to_owned(),
            tmid: "root".to_owned(),
            messages: vec![message("r0", "tester", "thread root")],
        });
        assert_eq!(
            state.open_thread.as_ref(),
            Some(&("GENERAL".to_owned(), "root".to_owned()))
        );
        assert_eq!(state.thread("root").len(), 2, "root + reply");
        assert!(state.active_messages().is_empty(), "room pane unaffected");

        state.apply(ChatEvent::ThreadClosed);
        assert!(state.open_thread.is_none());
        assert_eq!(state.thread("root").len(), 2, "the thread stays cached");
    }

    #[test]
    fn notifications_fold_for_other_rooms_and_skip_the_active_one() {
        let mut state = ChatState {
            active_room: Some("dm".to_owned()),
            ..ChatState::default()
        };
        let note = |room: &str| crate::rc::Notification {
            title: "laurent".to_owned(),
            text: String::new(),
            room_id: room.to_owned(),
            sender: Some("laurent".to_owned()),
            room_type: Some("d".to_owned()),
            message_id: None,
            tmid: None,
            message: Some("hello".to_owned()),
        };
        state.apply(ChatEvent::Notification(note("other")));
        state.apply(ChatEvent::Notification(note("dm")));
        assert_eq!(state.notifications.len(), 1, "the active room is skipped");
        assert_eq!(state.notifications[0].room_id, "other");
        let snapshot = UiSnapshot::from_state(&state);
        assert_eq!(snapshot.notifications, vec!["laurent: hello".to_owned()]);
    }

    #[test]
    fn messages_request_missing_avatars_and_avatars_fill_the_cache() {
        let mut state = ChatState::default();
        let follow_up = state.apply(ChatEvent::Messages {
            room_id: "dm".to_owned(),
            messages: vec![message("m1", "alice", "hi")],
        });
        match follow_up {
            Some(ChatCommand::FetchAvatars { usernames }) => {
                assert_eq!(usernames, vec!["alice".to_owned()]);
            }
            other => panic!("expected FetchAvatars, got {other:?}"),
        }
        state.apply(ChatEvent::Avatars {
            avatars: vec![("alice".to_owned(), "/cache/avatars/alice".to_owned())],
        });
        assert_eq!(
            state.avatars.get("alice").map(String::as_str),
            Some("/cache/avatars/alice")
        );
        // A repeat open fetches nothing new: the cache covers alice.
        assert!(
            state
                .apply(ChatEvent::Messages {
                    room_id: "dm".to_owned(),
                    messages: vec![message("m2", "alice", "again")],
                })
                .is_none()
        );
    }

    #[test]
    fn search_finds_hits_across_cached_rooms_case_insensitively() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::Rooms {
            rooms: vec![room("GENERAL", "general"), {
                let mut dm = room("dm1", "alice");
                dm.kind = 'd';
                dm
            }],
        });
        state.apply(ChatEvent::Messages {
            room_id: "GENERAL".to_owned(),
            messages: vec![message("m1", "tester", "Привет Мир")],
        });
        state.apply(ChatEvent::Messages {
            room_id: "dm1".to_owned(),
            messages: vec![message("m2", "alice", "hello world")],
        });
        state.apply(ChatEvent::SearchQuery("МИР".to_owned()));

        let hits = state.search(state.search_query.as_deref().unwrap());
        assert_eq!(hits.len(), 1, "case-insensitive across scripts");
        assert_eq!(hits[0].room, "general");
        assert_eq!(hits[0].sender, "tester");

        let snapshot = UiSnapshot::from_state(&state);
        assert_eq!(snapshot.search, vec!["general — tester: Привет Мир"]);

        state.apply(ChatEvent::SearchQuery(String::new()));
        assert!(state.search_query.is_none());
        assert!(UiSnapshot::from_state(&state).search.is_empty());
    }

    #[test]
    fn snapshot_lines_carry_reaction_badges() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::Messages {
            room_id: "GENERAL".to_owned(),
            messages: vec![message("m1", "tester", "hello")],
        });
        state.apply(ChatEvent::ReactionToggled {
            room_id: "GENERAL".to_owned(),
            msg_id: "m1".to_owned(),
            emoji: "tada".to_owned(),
        });
        let snapshot = UiSnapshot::from_state(&state);
        assert_eq!(
            snapshot.messages,
            vec!["tester: hello [tada ×1]".to_owned()]
        );
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
    fn emoji_map_folds_in_and_snapshot_names_rooms_and_messages() {
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
        state.apply(ChatEvent::Emoji {
            emojis: vec![("heart_21".to_owned(), "/emoji/heart_21.gif".to_owned())],
        });
        assert_eq!(
            state.emoji.get("heart_21").map(String::as_str),
            Some("/emoji/heart_21.gif")
        );

        let snapshot = UiSnapshot::from_state(&state);
        assert_eq!(snapshot.username.as_deref(), Some("tester"));
        assert_eq!(snapshot.rooms, vec!["general".to_owned()]);
        assert_eq!(snapshot.messages, vec!["tester: hello".to_owned()]);
        assert!(snapshot.status.contains("tester"));
    }
}
