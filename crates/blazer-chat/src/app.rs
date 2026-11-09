//! The application layer: the chat command/event boundary, the pure chat
//! state fold, and the compio thread that ties the protocol client, the
//! control socket and the channels together.
//!
//! The compio driver is thread-local, so this layer owns one std thread with
//! its `compio::runtime::Runtime`; GPUI never blocks on it. Both directions
//! cross through `async-channel`, which is executor-agnostic — channels, not
//! nested executors. tako serves the control socket
//! (`$XDG_RUNTIME_DIR/blazer-chat.sock`) so background tooling can drive and
//! verify the app without the compositor.

use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender, unbounded};
use serde::Serialize;

use crate::control;
use crate::http::Transport;
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
        /// The pending quote card's message id; `None` is a plain send
        /// (m12 F6).
        quote_msg_id: Option<String>,
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
    /// Stars or unstars a message (`chat.starMessage`/`unStarMessage`).
    StarMessage {
        msg_id: String,
        star: bool,
    },
    /// Logs out: invalidates the session server-side, drops the stored
    /// one and resets the UI to the logged-out view.
    Logout,
    /// Sets the account presence (`users.setStatus`).
    SetStatus {
        status: String,
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
    /// Re-fetches the newest history slice for the room (the network
    /// half of an instant cache-first open).
    RefreshRoom {
        room_id: String,
    },
    /// Loads the next older slice (triggered at the pane's top edge).
    FetchOlder {
        room_id: String,
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
    /// Flags a room unread (`subscriptions.read` unread flag) — the
    /// sidebar context menu's Mark-unread (m12 F5).
    MarkRoomUnread {
        room_id: String,
    },
    /// Hides the room for this account (`subscriptions.delete`) — the
    /// sidebar context menu's Hide (m12 F5).
    HideRoom {
        room_id: String,
    },
}

/// What the application reports back; the UI folds these into [`ChatState`].
pub enum ChatEvent {
    Failed(String),
    LoggedIn {
        username: String,
        origin: String,
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
    /// The cached slice painted instantly; the refresh follows.
    CachedMessages {
        room_id: String,
        messages: Vec<Msg>,
    },
    /// An older history slice (scroll-to-top paging): prepend + dedup.
    OlderMessages {
        room_id: String,
        messages: Vec<Msg>,
    },
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
    /// The server accepted the star toggle; the fold applies it locally.
    Starred {
        msg_id: String,
        star: bool,
    },
    /// The session ended (logout): the UI returns to the logged-out view.
    LoggedOut,
    /// A 401 invalidated the stored session mid-run: the shell stays up
    /// and shows the reason; the live layer drops its token.
    SessionExpired {
        reason: String,
    },
    /// The server accepted the presence change.
    PresenceSet {
        status: String,
    },
    /// The server flagged the room unread: the sidebar badge returns
    /// (m12 F5).
    RoomMarkedUnread {
        room_id: String,
    },
    /// The server hid the room: the sidebar drops it (m12 F5).
    RoomHidden {
        room_id: String,
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
    /// The server origin (`https://host:port`) — permalinks build on it.
    pub origin: Option<String>,
    /// The account presence the server accepted (`online`/`away`/`busy`).
    pub presence: Option<String>,
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
    /// Rooms whose older history ran out (paging stops asking).
    pub older_exhausted: std::collections::HashSet<String>,
    /// Bumped whenever cached messages change; the UI skips rebuilds
    /// otherwise.
    pub messages_version: u64,
    /// Room → the id of its first unread message, pinned when history
    /// lands with a positive unread count and cleared once a live
    /// message folds in for the open room (the user read past it).
    pub first_unread: std::collections::HashMap<String, String>,
    /// Bumped when the avatar cache fills.
    pub avatars_version: u64,
    /// Bumped when custom emoji arrive.
    pub emoji_version: u64,
    /// The pending quote card `(room_id, msg_id)`: a room switch or a
    /// send consumes it (m12 F6).
    pub quote: Option<(String, String)>,
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
            ChatEvent::LoggedIn { username, origin } => {
                self.username = Some(username);
                self.origin = Some(origin);
                self.error = None;
                Some(ChatCommand::RefreshRooms)
            }
            ChatEvent::PresenceSet { status } => {
                self.presence = Some(status);
                None
            }
            ChatEvent::LoggedOut => {
                // A full reset: leftover threads, avatars, searches,
                // notifications or exhausted-paging flags from the
                // previous account must not leak into the next one.
                let version = self.messages_version + 1;
                *self = ChatState::default();
                self.messages_version = version;
                None
            }
            ChatEvent::SessionExpired { reason } => {
                // Same reset as LoggedOut: the dead session's rooms,
                // messages, threads and drafts must not leak into
                // whatever account logs in next (cross-account leak,
                // audit sec-flow #5). The shell stays up — there is no
                // login view yet — and the reason surfaces in the
                // error banner.
                let version = self.messages_version + 1;
                *self = ChatState::default();
                self.messages_version = version;
                self.error = Some(reason.clone());
                None
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
                self.messages_version += 1;
                // The subscription count says how many of the newest
                // messages were unread when the room opened: pin the
                // boundary message before the count clears (m12 F1).
                let unread_before = self
                    .rooms
                    .iter()
                    .find(|room| room.id == room_id)
                    .map(|room| room.unread)
                    .unwrap_or_default();
                // Merge instead of replacing: live or paged messages that
                // arrived while the request was in flight must not vanish
                // with the refreshed slice.
                let entry = self.messages.entry(room_id.clone()).or_default();
                let fresh: std::collections::BTreeSet<&str> =
                    messages.iter().map(|m| m.id.as_str()).collect();
                entry.retain(|known| !fresh.contains(known.id.as_str()));
                entry.extend(messages.iter().cloned());
                entry.sort_by(|a, b| a.ts.cmp(&b.ts));
                // The room is on screen now; the server-side read receipt
                // went out with the history call.
                if let Some(room) = self.rooms.iter_mut().find(|room| room.id == room_id) {
                    room.unread = 0;
                }
                if unread_before > 0
                    && let Some(entry) = self.messages.get(&room_id)
                    && let Some(first) = entry
                        .len()
                        .checked_sub(unread_before as usize)
                        .and_then(|index| entry.get(index))
                {
                    self.first_unread.insert(room_id.clone(), first.id.clone());
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
                // A stale refresh must not drag the pane back to a room
                // the user already left: navigation follows only when
                // nothing else was selected meanwhile.
                if self
                    .active_room
                    .as_deref()
                    .is_none_or(|current| current == room_id.as_str())
                {
                    self.active_room = Some(room_id.clone());
                }
                // Opening a room navigates out of any open thread.
                self.open_thread = None;
                self.error = None;
                (!missing.is_empty()).then_some(ChatCommand::FetchAvatars { usernames: missing })
            }
            ChatEvent::Avatars { avatars } => {
                self.avatars.extend(avatars.iter().cloned());
                self.avatars_version += 1;
                None
            }
            ChatEvent::Sent { message } => {
                self.messages_version += 1;
                // The quote card is consumed by its send (m12 F6).
                self.quote = None;
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
            ChatEvent::LiveMessage(mut message) => {
                self.messages_version += 1;
                // A live message for the open room means the user read
                // past the boundary: the divider's job is done (m12 F1).
                if self.active_room.as_deref() == Some(message.room_id.as_str()) {
                    self.first_unread.remove(&message.room_id);
                }
                self.bump_room_activity(&message.room_id, message.ts.clone());
                // The stream redelivers what REST already folded in and
                // re-sends edited messages under the same id: replace by
                // the server id, newest delivery wins.
                let room_id = message.room_id.clone();
                let entry = self.messages.entry(room_id).or_default();
                match entry.iter_mut().find(|known| known.id == message.id) {
                    Some(known) => {
                        // Live deliveries do not carry the REST origin:
                        // keep the working chip URL from the fetched row.
                        if let (Some(incoming), Some(old)) =
                            (message.file.as_mut(), known.file.as_ref())
                            && incoming.url.is_empty()
                        {
                            incoming.url = old.url.clone();
                        }
                        *known = message;
                    }
                    None => entry.push(message),
                }
                self.error = None;
                None
            }
            ChatEvent::MessageUpdated { message } => {
                self.messages_version += 1;
                let room_id = message.room_id.clone();
                if let Some(room) = self.messages.get_mut(&room_id)
                    && let Some(known) = room.iter_mut().find(|known| known.id == message.id)
                {
                    *known = message;
                }
                None
            }
            ChatEvent::MessageDeleted { room_id, msg_id } => {
                self.messages_version += 1;
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
                self.messages_version += 1;
                // Local toggle until the authoritative live redelivery;
                // exact because the username is known here.
                if let Some(me) = self.username.clone()
                    && let Some(room) = self.messages.get_mut(&room_id)
                    && let Some(message) = room.iter_mut().find(|known| known.id == msg_id)
                {
                    let reaction = message.reactions.entry(emoji.clone()).or_default();
                    if !reaction.toggle(&me) {
                        message.reactions.remove(&emoji);
                    }
                }
                None
            }
            ChatEvent::Starred { msg_id, star } => {
                for room in self.messages.values_mut() {
                    for message in room.iter_mut() {
                        if message.id == *msg_id {
                            message.starred_by_me = star;
                        }
                    }
                }
                self.messages_version += 1;
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
            ChatEvent::RoomMarkedUnread { room_id } => {
                if let Some(room) = self.rooms.iter_mut().find(|room| room.id == room_id) {
                    // The badge must show: at least one unread even if
                    // the stale stream count was zero.
                    room.unread = room.unread.max(1);
                }
                None
            }
            ChatEvent::RoomHidden { room_id } => {
                self.rooms.retain(|room| room.id != room_id);
                if self.active_room.as_deref() == Some(room_id.as_str()) {
                    self.active_room = None;
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
                self.emoji_version += 1;
                None
            }
            ChatEvent::CachedMessages { room_id, messages } => {
                self.messages_version += 1;
                // A room switch drops an unconsumed quote card (m12 F6).
                self.quote = None;
                // The store snapshot is older than live state: merge
                // only rows the in-memory slice does not know yet.
                let entry = self.messages.entry(room_id.clone()).or_default();
                let known: std::collections::BTreeSet<&str> =
                    entry.iter().map(|m| m.id.as_str()).collect();
                let mut missing = messages
                    .into_iter()
                    .filter(|message| !known.contains(message.id.as_str()))
                    .collect::<Vec<_>>();
                entry.append(&mut missing);
                entry.sort_by(|a, b| a.ts.cmp(&b.ts));
                if let Some(room) = self.rooms.iter_mut().find(|room| room.id == room_id) {
                    room.unread = 0;
                }
                self.active_room = Some(room_id.clone());
                // Opening a room navigates out of any open thread.
                self.open_thread = None;
                self.error = None;
                // The network refresh replaces this slice right away.
                Some(ChatCommand::RefreshRoom { room_id })
            }
            ChatEvent::OlderMessages { room_id, messages } => {
                self.messages_version += 1;
                if messages.is_empty() {
                    self.older_exhausted.insert(room_id);
                } else {
                    let entry = self.messages.entry(room_id.clone()).or_default();
                    let known: std::collections::HashSet<String> =
                        entry.iter().map(|message| message.id.clone()).collect();
                    let fresh: Vec<Msg> = messages
                        .into_iter()
                        .filter(|message| !known.contains(&message.id))
                        .collect();
                    entry.splice(0..0, fresh);
                    let excess = entry.len().saturating_sub(400);
                    entry.drain(0..excess);
                }
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
            rooms: state
                .rooms
                .iter()
                .map(|room| match &room.preview {
                    Some((sender, text)) => {
                        let mut snippet = text.replace('\n', " ");
                        if snippet.chars().count() > 80 {
                            snippet = snippet.chars().take(80).collect();
                            snippet.push('…');
                        }
                        format!("{} — {}: {}", room.name, sender, snippet)
                    }
                    None => room.name.clone(),
                })
                .collect(),
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
                                .map(|(name, reaction)| {
                                    format!("[{name} ×{}]", reaction.count())
                                })
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

/// Spawns the live WebSocket and the emoji cache for the stored session.
/// Used by fresh logins (the loop arm) and by the startup resume, whose
/// `LoggedIn` event bypasses the loop.
fn ensure_live(
    settings: &RcSettings,
    store: Option<&std::sync::Arc<Store>>,
    events: &Sender<ChatEvent>,
) -> Option<Sender<LiveCommand>> {
    let store = store?;
    let session = store.load_session()?;
    let live = spawn_live(settings.clone(), session.clone(), events.clone());
    spawn_emoji(settings.clone(), session, store.clone(), events.clone());
    Some(live)
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
            eprintln!("blazer-chat: local store unavailable: {error}");
            None
        }
    };

    // The live layer follows the session (clone before the client moves it)
    // and is started from the loop once a session exists.
    let settings_for_live = settings.clone();
    let mut client = RcClient::new(settings);
    let mut live: Option<Sender<LiveCommand>> = None;
    // The paging cursor lives here, not in the store snapshot: the
    // store keeps only the refresh slice, so a cursor read from it
    // re-fetched the same page forever.
    let mut oldest: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    // Resumable start: stored session and rooms paint the UI instantly; a
    // stale token surfaces as a 401 on the first refresh and gets cleared.
    if let Some(store) = store.as_ref() {
        if let Some(session) = store.load_session() {
            client.set_session(session.clone());
            let origin = client.origin().to_owned();
            let _ = events
                .send(ChatEvent::LoggedIn {
                    username: session.username,
                    origin,
                })
                .await;
            // The resume path sends its LoggedIn event directly, so the
            // loop's spawn arm never sees it: start the live layer here.
            live = ensure_live(&settings_for_live, Some(store), &events);
        }
        if let Some(rooms) = store.load_rooms() {
            // History routing needs the room kinds before the first
            // live rooms() refresh (the startup race: opening a DM
            // queried channels.history and failed).
            client.seed_room_kinds(&rooms);
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
        let event = execute(&mut client, store.as_deref(), &mut oldest, command).await;
        match &event {
            ChatEvent::Messages { room_id, messages }
            | ChatEvent::CachedMessages { room_id, messages }
            | ChatEvent::OlderMessages { room_id, messages } => {
                if let Some(message) = messages.first() {
                    oldest.insert(room_id.clone(), message.ts.clone());
                }
            }
            _ => {}
        }
        match &event {
            ChatEvent::LoggedOut => live = None,
            ChatEvent::LoggedIn { .. } if live.is_none() => {
                match ensure_live(&settings_for_live, store.as_ref(), &events) {
                    Some(live_layer) => live = Some(live_layer),
                    None => eprintln!("blazer-chat: live layer disabled: no stored session"),
                }
            }
            ChatEvent::SessionExpired { .. } => {
                // Dropping the sender closes the channel: the live task
                // observes the closure and stops reconnecting.
                live = None;
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
async fn fetch_image_preview<T: Transport>(
    client: &mut RcClient<T>,
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

/// The command name when `text` is a slash command (`/cmd params`),
/// without the leading slash. A bare `/`, a `//`-escaped literal or
/// plain text is not a command.
fn slash_command_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    if rest.is_empty() || rest.starts_with('/') {
        return None;
    }
    rest.split_whitespace().next()
}

/// Maps a 401 from any client call to a session-invalidating event:
/// the token is dead, so the process must stop using it (the loop drops
/// the live layer on [`ChatEvent::SessionExpired`]).
fn session_expired<T: Transport>(
    store: Option<&Store>,
    client: &mut RcClient<T>,
    error: &RcError,
) -> Option<ChatEvent> {
    if let RcError::Http { status: 401, body } = error {
        client.clear_session();
        if let Some(store) = store {
            let _ = store.clear_session();
        }
        return Some(ChatEvent::SessionExpired {
            reason: format!("session rejected (401): {body} — log in again"),
        });
    }
    None
}

/// One command → one event; failures always surface as `Failed`. `store`
/// persists the session and room cache; a 401 clears the stored session.
async fn execute<T: Transport>(
    client: &mut RcClient<T>,
    store: Option<&Store>,
    oldest: &mut std::collections::HashMap<String, String>,
    command: ChatCommand,
) -> ChatEvent {
    match command {
        ChatCommand::Login { user, password } => match client.login(&user, &password).await {
            Ok(session) => {
                if let Some(store) = store
                    && let Err(error) = store.save_session(&session)
                {
                    eprintln!("blazer-chat: session not stored: {error}");
                }
                ChatEvent::LoggedIn {
                    username: session.username,
                    origin: client.origin().to_owned(),
                }
            }
            Err(error) => session_expired(store, client, &error)
                .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
        },
        ChatCommand::RefreshRooms => match client.rooms().await {
            Ok(rooms) => {
                if let Some(store) = store
                    && let Err(error) = store.save_rooms(&rooms)
                {
                    eprintln!("blazer-chat: rooms not cached: {error}");
                }
                ChatEvent::Rooms { rooms }
            }
            Err(error) => session_expired(store, client, &error)
                .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
        },
        ChatCommand::OpenRoom { room_id } => {
            // Instant open: the cached slice paints now; the network
            // refresh follows as its own command.
            let messages = store
                .and_then(|store| store.load_messages(&room_id))
                .unwrap_or_default();
            ChatEvent::CachedMessages { room_id, messages }
        }
        ChatCommand::RefreshRoom { room_id } => match client.history(&room_id).await {
            Ok(mut messages) => {
                // Server-side read receipt: the unread stream stops
                // counting this room. Cosmetic — a refusal is logged, not
                // banner-worthy.
                if let Err(error) = client.mark_read(&room_id).await {
                    eprintln!("blazer-chat: mark read: {error}");
                }
                if let Some(store) = store
                    && let Err(error) = store.save_messages(&room_id, &messages)
                {
                    eprintln!("blazer-chat: messages not cached: {error}");
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
                                    eprintln!("blazer-chat: preview {}: {error}", file.name)
                                }
                            }
                        }
                    }
                }
                ChatEvent::Messages { room_id, messages }
            }
            Err(error) => session_expired(store, client, &error)
                .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
        },
        ChatCommand::FetchOlder { room_id } => {
            let cursor = oldest.get(&room_id).cloned().or_else(|| {
                store
                    .and_then(|store| store.load_messages(&room_id))
                    .and_then(|messages| messages.first().map(|message| message.ts.clone()))
            });
            match client.history_page(&room_id, cursor.as_deref()).await {
                Ok(messages) => ChatEvent::OlderMessages { room_id, messages },
                Err(error) => {
                    // A failed page must not wedge the strip: an empty
                    // OlderMessages clears loading_older for a later
                    // retry (a 401 here still expires the session).
                    if matches!(error, RcError::Http { status: 401, .. }) {
                        session_expired(store, client, &error);
                    }
                    eprintln!("blazer-chat: older history: {error}");
                    ChatEvent::OlderMessages {
                        room_id,
                        messages: Vec::new(),
                    }
                }
            }
        }
        ChatCommand::Send {
            room_id,
            text,
            quote_msg_id,
        } => {
            // Slash commands are server-side methods, never chat text:
            // posting one raw would drop `/msg bob look here` into the
            // channel (m12 D1). No command registry exists yet, so
            // every command is refused locally — nothing hits the wire.
            if let Some(name) = slash_command_name(&text) {
                return ChatEvent::Failed(format!(
                    "slash command /{name} is not supported here yet — nothing was sent"
                ));
            }
            let sent = match quote_msg_id {
                // A quoted send rides the quote attachment (m12 F6).
                Some(quote) => client.send_quoted(&room_id, &text, &quote).await,
                None => client.send(&room_id, &text).await,
            };
            match sent {
                Ok(message) => ChatEvent::Sent { message },
                Err(error) => session_expired(store, client, &error)
                    .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
            }
        }
        ChatCommand::EditMessage {
            room_id,
            msg_id,
            text,
        } => {
            // An edited body starting with `/` is the same defect: it
            // would land as a slash command on the server (m12 D1).
            if let Some(name) = slash_command_name(&text) {
                return ChatEvent::Failed(format!(
                    "slash command /{name} is not supported here yet — nothing was sent"
                ));
            }
            match client.update_message(&room_id, &msg_id, &text).await {
                Ok(message) => ChatEvent::MessageUpdated { message },
                Err(error) => session_expired(store, client, &error)
                    .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
            }
        }
        ChatCommand::DeleteMessage { room_id, msg_id } => {
            match client.delete_message(&room_id, &msg_id).await {
                Ok(()) => ChatEvent::MessageDeleted { room_id, msg_id },
                Err(error) => session_expired(store, client, &error)
                    .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
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
            Err(error) => session_expired(store, client, &error)
                .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
        },
        ChatCommand::StarMessage { msg_id, star } => {
            match client.star_message(&msg_id, star).await {
                Ok(()) => ChatEvent::Starred { msg_id, star },
                Err(error) => session_expired(store, client, &error)
                    .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
            }
        }
        ChatCommand::SetStatus { status } => match client.set_status(&status).await {
            Ok(()) => ChatEvent::PresenceSet { status },
            Err(error) => session_expired(store, client, &error)
                .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
        },
        ChatCommand::Logout => {
            // The session is dead either way: local reset never waits
            // on the server call.
            let _ = client.logout().await;
            if let Some(store) = store
                && let Err(error) = store.clear_session()
            {
                eprintln!("blazer-chat: stored session not cleared: {error}");
            }
            ChatEvent::LoggedOut
        }
        ChatCommand::UploadFile { room_id, path, msg } => {
            match client.upload_file(&room_id, &path, &msg).await {
                Ok(message) => ChatEvent::Sent { message },
                Err(error) => session_expired(store, client, &error)
                    .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
            }
        }
        // Local search needs no I/O: the state owns the cache.
        ChatCommand::Search(query) => ChatEvent::SearchQuery(query),
        ChatCommand::OpenThread { tmid } => {
            let tmid_owned = tmid.clone();
            match client.thread_messages(&tmid).await {
                Ok(mut messages) => {
                    // Inline image previews, same as room history: the
                    // chip remains the fallback for anything that fails.
                    if let Some(store) = store
                        && let Some(room_id) =
                            messages.first().map(|message| message.room_id.clone())
                    {
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
                                        eprintln!("blazer-chat: preview {}: {error}", file.name)
                                    }
                                }
                            }
                        }
                    }
                    match messages.first().map(|message| message.room_id.clone()) {
                        Some(room_id) => ChatEvent::ThreadMessages {
                            room_id,
                            tmid,
                            messages,
                        },
                        None => ChatEvent::Failed(format!(
                            "thread {tmid_owned}: no messages (root not visible)"
                        )),
                    }
                }
                Err(error) => session_expired(store, client, &error)
                    .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
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
                        eprintln!("blazer-chat: avatar cache: {error}");
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
            Err(error) => session_expired(store, client, &error)
                .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
        },
        ChatCommand::MarkRoomUnread { room_id } => match client.mark_room_unread(&room_id).await {
            Ok(()) => ChatEvent::RoomMarkedUnread { room_id },
            Err(error) => session_expired(store, client, &error)
                .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
        },
        ChatCommand::HideRoom { room_id } => match client.hide_room(&room_id).await {
            Ok(()) => ChatEvent::RoomHidden { room_id },
            Err(error) => session_expired(store, client, &error)
                .unwrap_or_else(|| ChatEvent::Failed(error.to_string())),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;
    use crate::rc::client_tests::{FakeTransport, client};

    fn room(id: &str, name: &str) -> Room {
        Room {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: 'c',
            unread: 0,
            lm: None,
            preview: None,
            topic: None,
            mentions: 0,
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
            starred_by_me: false,
            system: false,
        }
    }

    #[test]
    fn login_requests_a_rooms_refresh() {
        let mut state = ChatState::default();
        let follow_up = state.apply(ChatEvent::LoggedIn {
            username: "tester".to_owned(),
            origin: "https://chat.example".to_owned(),
        });
        assert_eq!(state.username.as_deref(), Some("tester"));
        assert!(matches!(follow_up, Some(ChatCommand::RefreshRooms)));
    }

    #[test]
    fn session_expired_resets_state_and_surfaces_the_reason() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::LoggedIn {
            username: "tester".to_owned(),
            origin: "https://chat.example".to_owned(),
        });
        state.apply(ChatEvent::Rooms {
            rooms: vec![room("GENERAL", "general")],
        });
        state.active_room = Some("GENERAL".to_owned());
        state.apply(ChatEvent::SessionExpired {
            reason: "session rejected (401): stale — log in again".to_owned(),
        });
        assert_eq!(
            state.error.as_deref(),
            Some("session rejected (401): stale — log in again")
        );
        assert_eq!(
            state.username.as_deref(),
            None,
            "the dead account must not leak into the next session"
        );
        assert!(state.rooms.is_empty(), "rooms reset");
        assert!(state.messages.is_empty(), "messages reset");
        assert_eq!(state.active_room, None, "active room reset");
        assert_eq!(
            state.messages_version, 1,
            "the version bump survives so views repaint once"
        );
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
        edited.reactions.insert(
            "tada".to_owned(),
            crate::rc::Reaction {
                usernames: vec!["a".to_owned(), "b".to_owned()],
            },
        );
        state.apply(ChatEvent::MessageUpdated {
            message: edited.clone(),
        });
        let messages = state.active_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text, "new text");
        assert_eq!(
            messages[1]
                .reactions
                .get("tada")
                .map(crate::rc::Reaction::count),
            Some(2)
        );

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
    fn room_open_clears_the_open_thread() {
        let mut state = ChatState::default();

        state.apply(ChatEvent::ThreadMessages {
            room_id: "dm1".to_owned(),
            tmid: "t1".to_owned(),
            messages: vec![message("m1", "alice", "reply")],
        });
        assert!(state.open_thread.is_some(), "thread opened");

        // Both room-open paths navigate out of the thread view: the
        // network refresh fold and the cached fold (instant open). The
        // Rooms event is a background list refresh and must NOT close
        // an open thread.
        state.apply(ChatEvent::Messages {
            room_id: "dm1".to_owned(),
            messages: vec![message("m0", "alice", "history")],
        });
        assert!(
            state.open_thread.is_none(),
            "messages fold clears the thread"
        );

        state.apply(ChatEvent::ThreadMessages {
            room_id: "dm1".to_owned(),
            tmid: "t1".to_owned(),
            messages: vec![message("m1", "alice", "reply")],
        });
        state.apply(ChatEvent::CachedMessages {
            room_id: "dm1".to_owned(),
            messages: vec![],
        });
        assert!(state.open_thread.is_none(), "cached fold clears the thread");
    }

    #[test]
    fn cached_messages_paint_instantly_and_refresh_follows() {
        let mut state = ChatState::default();
        let follow_up = state.apply(ChatEvent::CachedMessages {
            room_id: "dm".to_owned(),
            messages: vec![message("m1", "tester", "cached slice")],
        });
        assert_eq!(state.active_room.as_deref(), Some("dm"));
        assert_eq!(state.active_messages().len(), 1);
        match follow_up {
            Some(ChatCommand::RefreshRoom { room_id }) => {
                assert_eq!(room_id, "dm");
            }
            other => panic!("expected RefreshRoom, got {other:?}"),
        }
    }

    #[test]
    fn older_slices_prepend_dedup_and_exhaust() {
        let mut state = ChatState {
            active_room: Some("dm".to_owned()),
            ..ChatState::default()
        };
        state.apply(ChatEvent::Messages {
            room_id: "dm".to_owned(),
            messages: vec![message("m1", "tester", "newest")],
        });
        // A slice with a duplicate and a fresh message: dedup keeps one.
        let mut duplicate = message("m1", "tester", "newest");
        duplicate.id = "m1".to_owned();
        state.apply(ChatEvent::OlderMessages {
            room_id: "dm".to_owned(),
            messages: vec![message("m0", "tester", "older"), duplicate],
        });
        let messages = state.active_messages();
        assert_eq!(messages.len(), 2, "prepended, deduplicated");
        assert_eq!(messages[0].id, "m0");
        assert_eq!(messages[1].id, "m1");

        // An empty slice means the room ran out of history.
        state.apply(ChatEvent::OlderMessages {
            room_id: "dm".to_owned(),
            messages: Vec::new(),
        });
        assert!(state.older_exhausted.contains("dm"));
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
        let mut state = ChatState {
            username: Some("tester".to_owned()),
            ..ChatState::default()
        };
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
            origin: "https://chat.example".to_owned(),
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

    #[test]
    fn first_unread_marks_the_boundary_and_live_clears_it() {
        let mut state = ChatState::default();
        state.apply(ChatEvent::Rooms {
            rooms: vec![room("GENERAL", "general")],
        });
        if let Some(room) = state.rooms.iter_mut().find(|room| room.id == "GENERAL") {
            room.unread = 2;
        }
        let messages: Vec<Msg> = (0..5)
            .map(|index| Msg {
                id: format!("m{index}"),
                room_id: "GENERAL".to_owned(),
                sender: format!("u{index}"),
                text: format!("text {index}"),
                ts: format!("2026-11-08T0{index}:00:00Z"),
                tmid: None,
                tcount: None,
                tlm: None,
                file: None,
                reactions: Default::default(),
                starred_by_me: false,
                system: false,
            })
            .collect();
        state.apply(ChatEvent::Messages {
            room_id: "GENERAL".to_owned(),
            messages,
        });
        assert_eq!(
            state.first_unread.get("GENERAL").map(String::as_str),
            Some("m3"),
            "the newest two messages were unread — the boundary is m3"
        );
        state.active_room = Some("GENERAL".to_owned());
        state.apply(ChatEvent::LiveMessage(Msg {
            id: "m5".to_owned(),
            room_id: "GENERAL".to_owned(),
            sender: "u9".to_owned(),
            text: "live".to_owned(),
            ts: "2026-11-08T05:00:00Z".to_owned(),
            tmid: None,
            tcount: None,
            tlm: None,
            file: None,
            reactions: Default::default(),
            starred_by_me: false,
            system: false,
        }));
        assert!(
            !state.first_unread.contains_key("GENERAL"),
            "reading past the boundary clears the divider"
        );
    }

    #[test]
    fn slash_command_name_ignores_literals() {
        assert_eq!(slash_command_name("/leave now"), Some("leave"));
        assert_eq!(slash_command_name("/"), None);
        assert_eq!(slash_command_name("//not-a-command"), None);
        assert_eq!(slash_command_name("plain text"), None);
    }

    #[test]
    fn slash_commands_are_refused_and_never_reach_the_wire() {
        let transport = FakeTransport::answering(&[("sendMessage", 200, "{}")]);
        let probe = transport.clone();
        let mut rc = client(transport);
        let mut oldest = std::collections::HashMap::new();
        let event = futures_lite::future::block_on(execute(
            &mut rc,
            None,
            &mut oldest,
            ChatCommand::Send {
                room_id: "GENERAL".to_owned(),
                text: "/leave now".to_owned(),
                quote_msg_id: None,
            },
        ));
        match &event {
            ChatEvent::Failed(message) => {
                assert!(
                    message.contains("/leave"),
                    "the reason names the command: {message}"
                );
                assert!(message.contains("nothing was sent"));
            }
            ChatEvent::Sent { .. } => panic!("a slash command was posted as chat text"),
            _ => panic!("expected a local refusal, got a different event"),
        }
        assert!(
            !probe.made_request(),
            "a slash command must never hit the wire as chat text"
        );
    }

    #[test]
    fn quoted_send_rides_the_quote_attachment() {
        let body = r#"{"message":{"_id":"m3","rid":"GENERAL","msg":"see this","ts":"2026-11-08T05:00:00Z","u":{"username":"tester"}},"success":true}"#;
        let login = r#"{"status":"success","data":{"userId":"u1","authToken":"tok","me":{"username":"tester","name":"Tester","emails":null}}}"#;
        let transport =
            FakeTransport::answering(&[("login", 200, login), ("sendMessage", 200, body)]);
        let probe = transport.clone();
        let mut rc = client(transport);
        futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
        let event = futures_lite::future::block_on(execute(
            &mut rc,
            None,
            &mut std::collections::HashMap::new(),
            ChatCommand::Send {
                room_id: "GENERAL".to_owned(),
                text: "see this".to_owned(),
                quote_msg_id: Some("root-msg".to_owned()),
            },
        ));
        match &event {
            ChatEvent::Sent { .. } => {}
            ChatEvent::Failed(reason) => panic!("the send failed: {reason}"),
            _ => panic!("expected a Sent event"),
        }
        let request = probe.last_request();
        assert!(
            request.contains(r#""type":"quote""#),
            "the quote attachment rides the body: {request}"
        );
        assert!(request.contains("root-msg"), "the quoted id rides the body");
    }

    #[test]
    fn mark_unread_folds_a_visible_badge() {
        let mut state = ChatState {
            rooms: vec![room("GENERAL", "general")],
            ..ChatState::default()
        };
        state.apply(ChatEvent::RoomMarkedUnread {
            room_id: "GENERAL".to_owned(),
        });
        assert_eq!(state.rooms[0].unread, 1, "a flagged room shows a badge");
        state.rooms[0].unread = 0;
        state.apply(ChatEvent::RoomMarkedUnread {
            room_id: "GENERAL".to_owned(),
        });
        assert_eq!(state.rooms[0].unread, 1, "the badge never stays hidden");
    }

    #[test]
    fn hidden_room_leaves_the_sidebar_and_closes_the_pane() {
        let mut state = ChatState {
            rooms: vec![room("GENERAL", "general"), room("priv1", "secret")],
            active_room: Some("GENERAL".to_owned()),
            ..ChatState::default()
        };
        state.apply(ChatEvent::RoomHidden {
            room_id: "GENERAL".to_owned(),
        });
        assert!(
            state.rooms.iter().all(|room| room.id != "GENERAL"),
            "the hidden room leaves the list"
        );
        assert_eq!(state.active_room, None, "hiding the open room closes it");
        assert!(
            state.rooms.iter().any(|room| room.id == "priv1"),
            "other rooms stay"
        );
    }
}
