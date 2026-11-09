//! The chat view entity: room list, message pane, composer. State lives in
//! the pure [`ChatState`]; events fold in through the compio pump, clicks
//! push commands back.

use gpui::{
    Anchor, AnyElement, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, FontWeight,
    MouseButton, Pixels, Role, ScrollHandle, SharedString, Window, anchored, deferred, div, img,
    prelude::*, px, rems, rgb, uniform_list,
};
use std::sync::{Arc, Mutex};

use async_channel::Sender;

use super::markdown;
use super::message_pane::{
    MessagePane, MessageView, PaneEvent, PaneInputs, adjacent_grouped, avatar, pretty_time,
};
use super::theme::{Palette, palette};
use super::{
    ZoomIn, ZoomOut, ZoomReset,
    input::{Suggest, TextInput, TextInputEvent},
};
use crate::app::{ChatCommand, ChatEvent, ChatService, ChatState, UiSnapshot};
use crate::rc::{Msg, Room};

/// The one window's root view.
pub(crate) struct ChatRoot {
    pub(crate) state: ChatState,
    commands: Sender<ChatCommand>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
    pub(crate) composer: Entity<TextInput>,
    /// Room matches for the current search query: id + display name,
    /// rendered above the message hits (m12 F3 room jump).
    search_rooms: Vec<(String, String)>,
    focus: FocusHandle,
    /// Local search box (the pane shows results while a query is set).
    search: Entity<TextInput>,
    /// `(room_id, msg_id)` while the composer edits an existing message.
    pub(crate) editing: Option<(String, String)>,
    thread_scroll: ScrollHandle,
    results_scroll: ScrollHandle,
    /// Picked file paths from the attach dialog's worker thread.
    pick_tx: async_channel::Sender<Option<std::path::PathBuf>>,
    /// The message pane entity: rows, list state and parse cache.
    pane: Entity<MessagePane>,
    /// The row whose labeled action menu is open `(room_id, msg_id)`.
    pub(crate) open_menu: Option<(String, String)>,
    /// The room whose right-click context menu is open (m12 F5).
    room_menu: Option<String>,
    /// The room-info panel toggle (header ℹ️, m12 F7).
    show_info: bool,
    /// The username whose user card is open (m12 F8).
    pub(crate) user_card: Option<String>,
    /// The emoji picker popover is open (m12 F9).
    emoji_picker_open: bool,
    /// The top-bar avatar menu is open.
    user_menu_open: bool,
    /// Colors for the window's current appearance; the m9 appearance
    /// observer swaps it and re-renders on a live switch.
    palette: Palette,
    /// Frame-interval telemetry (m13): the previous render's start
    /// plus the last 10 frame intervals in ms. An off-view window
    /// produces no frames and no samples — throttled intervals do
    /// not represent render cost; the in-view tile stays the m10
    /// gold standard.
    last_frame_start: Option<std::time::Instant>,
    frame_ms: std::collections::VecDeque<f64>,
}

/// Frame-interval sampling (m13 telemetry): the interval between
/// consecutive render starts enters a 10-slot ring; the very first
/// frame has no predecessor and samples nothing.
fn record_frame(
    last: &mut Option<std::time::Instant>,
    ring: &mut std::collections::VecDeque<f64>,
    now: std::time::Instant,
) {
    if let Some(start) = *last {
        ring.push_back(now.duration_since(start).as_secs_f64() * 1000.0);
        while ring.len() > 10 {
            ring.pop_front();
        }
    }
    *last = Some(now);
}

impl ChatRoot {
    pub(crate) fn new(service: ChatService, window: &Window, cx: &mut Context<Self>) -> Self {
        let ChatService {
            events,
            commands,
            snapshot,
        } = service;

        // compio → UI: fold events into the pure state; a failed upgrade
        // means the window closed and the pump stops.
        cx.spawn(async move |root, cx| {
            while let Ok(event) = events.recv().await {
                if let ChatEvent::Quit = event {
                    cx.update(|app| app.quit());
                    break;
                }
                if root
                    .update(cx, |chat, cx| chat.apply_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        // The file dialog runs on its own thread (GTK wants its own
        // iteration); a picked path returns over the channel and is
        // uploaded into the room active at that moment.
        let (pick_tx, pick_rx) = async_channel::unbounded::<Option<std::path::PathBuf>>();
        cx.spawn(async move |root, cx| {
            while let Ok(picked) = pick_rx.recv().await {
                let Some(path) = picked else { continue };
                if root
                    .update(cx, |chat, cx| {
                        if let Some(room_id) = chat.state.active_room.clone() {
                            chat.send_command(ChatCommand::UploadFile {
                                room_id,
                                path: path.display().to_string(),
                                msg: String::new(),
                            });
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let composer = cx.new(|cx| TextInput::new("Message…", cx));
        let search_rooms = Vec::new();
        // Edit walking needs the window (begin_edit focuses the
        // composer): subscribe_in, same as the pane (m13 S2b).
        cx.subscribe_in(
            &composer,
            window,
            |chat, _, event, window, cx| match event {
                TextInputEvent::Submitted => chat.send_message(cx),
                TextInputEvent::Cancelled => chat.cancel_editing(cx),
                TextInputEvent::ImagePasted { bytes, ext } => {
                    chat.upload_clipboard_image(bytes, ext, cx);
                }
                TextInputEvent::Changed { text, caret } => {
                    chat.update_suggestions(text, *caret, cx);
                }
                TextInputEvent::SuggestMoved => cx.notify(),
                TextInputEvent::EditPrevious => chat.edit_walk(true, window, cx),
                TextInputEvent::EditNext => chat.edit_walk(false, window, cx),
            },
        )
        .detach();
        let search = cx.new(|cx| TextInput::new("Search…", cx));
        let pane = cx.new(MessagePane::new);
        // The window routes BeginEdit's composer focus; plain
        // subscribe has no Window (m13 S2b).
        cx.subscribe_in(&pane, window, |chat, _, event, window, cx| {
            chat.on_pane_event(event, window, cx)
        })
        .detach();
        cx.subscribe(&search, |chat, _, event, cx| match event {
            TextInputEvent::Submitted => chat.run_search(cx),
            TextInputEvent::Cancelled
            | TextInputEvent::ImagePasted { .. }
            | TextInputEvent::Changed { .. }
            | TextInputEvent::SuggestMoved
            | TextInputEvent::EditPrevious
            | TextInputEvent::EditNext => {}
        })
        .detach();
        // A live appearance switch swaps the palette and re-renders the
        // window (m9): the pane rides PaneInputs, the composer reads
        // `window.appearance()` per frame.
        let this = cx.entity();
        window
            .observe_window_appearance(move |window, cx| {
                let p = palette(window.appearance());
                this.update(cx, |chat, cx| {
                    chat.palette = p;
                    cx.notify();
                });
            })
            .detach();
        Self {
            state: ChatState::default(),
            commands,
            snapshot,
            composer,
            last_frame_start: None,
            frame_ms: std::collections::VecDeque::new(),
            search_rooms,
            focus: cx.focus_handle(),
            search,
            editing: None,
            thread_scroll: ScrollHandle::new(),
            results_scroll: ScrollHandle::new(),
            pick_tx,
            pane,
            open_menu: None,
            room_menu: None,
            show_info: false,
            user_card: None,
            emoji_picker_open: false,
            user_menu_open: false,
            palette: palette(window.appearance()),
        }
    }

    fn apply_event(&mut self, event: ChatEvent, cx: &mut Context<Self>) {
        if matches!(event, ChatEvent::OlderMessages { .. }) {
            self.pane.update(cx, |pane, _| pane.clear_loading_older());
        }
        let follow_up = self.state.apply(event);
        if let Some(command) = follow_up {
            self.send_command(command);
        }
        // Anchor policy lives only in the pane's sync: splices keep
        // the viewport natively and reshapes restore by message id —
        // a second anchor here would fight the first one (deepseek S5
        // T3: the later scroll_to_reveal_item rewrote the index).
        let state = &self.state;
        self.pane.update(cx, |pane, _| pane.sync(state));
        self.mirror(cx);
        cx.notify();
    }

    /// Maps pane events back onto the existing methods — the pane
    /// edge cut (m13 S2b); no ChatRoot method changed its body.
    fn on_pane_event(&mut self, event: &PaneEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PaneEvent::ToggleReaction {
                room_id,
                msg_id,
                emoji,
            } => self.toggle_reaction(room_id, msg_id, emoji, cx),
            PaneEvent::BeginEdit {
                room_id,
                msg_id,
                text,
            } => self.begin_edit(room_id.clone(), msg_id.clone(), text.clone(), window, cx),
            PaneEvent::CancelEdit => {
                self.editing = None;
                cx.notify();
            }
            PaneEvent::DeleteMessage { room_id, msg_id } => {
                self.delete_message(room_id, msg_id, cx)
            }
            PaneEvent::OpenThread { tmid } => {
                self.send_command(ChatCommand::OpenThread { tmid: tmid.clone() });
                cx.notify();
            }
            PaneEvent::QuoteMessage { room_id, msg_id } => {
                self.state.quote = Some((room_id.clone(), msg_id.clone()));
                cx.notify();
            }
            PaneEvent::ToggleStar { msg_id, starred } => {
                self.toggle_star(msg_id.clone(), *starred, cx);
            }
            PaneEvent::CopyPermalink { room_id, msg_id } => {
                if let Some(link) = self.permalink(room_id, msg_id) {
                    cx.write_to_clipboard(ClipboardItem::new_string(link));
                }
                cx.notify();
            }
            PaneEvent::CopyText { text } => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                cx.notify();
            }
            PaneEvent::ToggleMenu { room_id, msg_id } => {
                self.toggle_menu(room_id.clone(), msg_id.clone(), cx);
            }
            PaneEvent::CloseMenu => {
                self.open_menu = None;
                cx.notify();
            }
            PaneEvent::OpenUserCard { username } => {
                self.user_card = Some(username.clone());
                if !self.state.user_info.contains_key(username) {
                    self.send_command(ChatCommand::UserInfo {
                        username: username.clone(),
                    });
                }
                cx.notify();
            }
            PaneEvent::CloseUserCard => {
                self.user_card = None;
                cx.notify();
            }
            PaneEvent::OpenDm { username } => {
                self.user_card = None;
                self.send_command(ChatCommand::OpenDm {
                    username: username.clone(),
                });
                cx.notify();
            }
        }
    }

    /// Mirrors the entity into the control-socket snapshot. The UI thread
    /// holds the lock only for the store, never across a frame.
    fn mirror(&self, cx: &App) {
        let mut snapshot = UiSnapshot::from_state(&self.state);
        snapshot.pane_rebuilds = self.pane.read(cx).rebuilds();
        if !self.frame_ms.is_empty() {
            let count = self.frame_ms.len() as f64;
            snapshot.frame_ms_avg = Some(self.frame_ms.iter().sum::<f64>() / count);
            snapshot.frame_ms_max = Some(self.frame_ms.iter().cloned().fold(0.0, f64::max));
        }
        snapshot.build_profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot);
    }

    fn open_room(&mut self, room_id: String, cx: &mut Context<Self>) {
        // Clicking a room is a navigation out of any open thread —
        // clear immediately (the folds clear again authoritatively).
        self.state.open_thread = None;
        // The draft and the edit target belong to the room they were
        // started in: save the text, drop the edit, restore the draft.
        if let Some(previous) = self.state.active_room.clone() {
            let draft = self.composer.read(cx).content().to_owned();
            self.state.drafts.insert(previous.clone(), draft.clone());
            self.send_command(ChatCommand::SaveDraft {
                room_id: previous,
                text: draft,
            });
        }
        self.editing = None;
        // Panels and cards belong to the room they opened in.
        self.show_info = false;
        self.user_card = None;
        self.emoji_picker_open = false;
        let restored = self.state.drafts.get(&room_id).cloned().unwrap_or_default();
        self.composer
            .update(cx, |composer, cx| composer.set_content(&restored, cx));
        // The placeholder names the room (m12 F12).
        let name = self
            .state
            .rooms
            .iter()
            .find(|room| room.id == room_id)
            .map(|room| room.name.clone());
        let placeholder = match name {
            Some(name) => format!("Message #{name}"),
            None => "Message…".to_owned(),
        };
        self.composer.update(cx, |composer, cx| {
            composer.set_placeholder(&placeholder, cx)
        });
        self.send_command(ChatCommand::OpenRoom {
            room_id: room_id.clone(),
        });
        cx.notify();
    }

    /// Queues a command; a full queue is a bug-scale backlog, so the
    /// drop is loud (m10 bounded-channel policy).
    pub(crate) fn send_command(&self, command: ChatCommand) {
        if let Err(error) = self.commands.try_send(command) {
            eprintln!("blazer-chat: command queue full, dropped: {error}");
        }
    }

    /// Escape: an edit backs out to the room's draft; with no edit an
    /// active search clears (m12 F12).
    fn cancel_editing(&mut self, cx: &mut Context<Self>) {
        if let Some((edit_room, _)) = self.editing.take() {
            let restored = self
                .state
                .drafts
                .get(&edit_room)
                .cloned()
                .unwrap_or_default();
            self.composer
                .update(cx, |composer, cx| composer.set_content(&restored, cx));
        } else if self
            .state
            .search_query
            .as_deref()
            .is_some_and(|query| !query.is_empty())
        {
            self.search.update(cx, |box_, cx| box_.set_content("", cx));
            self.send_command(ChatCommand::Search(String::new()));
        }
        cx.notify();
    }

    /// ArrowUp/ArrowDown edit walk (m14 P0, RC ChatMessages
    /// contract subset): previous walks to the older own editable
    /// message (caret at end; from a fresh composer, to the newest
    /// own); next walks forward (caret at start) and exits the edit
    /// at the newest, restoring the room draft. The in-progress text
    /// is saved to the room draft before every hop.
    fn edit_walk(&mut self, previous: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(room) = self.state.active_room.clone() else {
            return;
        };
        let Some(me) = self.state.username.as_deref() else {
            return;
        };
        if !previous && self.editing.is_none() {
            return; // ArrowDown outside an edit does nothing (RC).
        }
        // Own, non-system messages of the room, oldest first.
        let own: Vec<(String, String)> = self
            .state
            .messages
            .get(&room)
            .map(|messages| {
                messages
                    .iter()
                    .filter(|message| message.sender == me && !message.system)
                    .map(|message| (message.id.clone(), message.text.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if own.is_empty() {
            return;
        }
        // The in-progress text becomes the room draft before the hop.
        let draft = self.composer.read(cx).content().to_owned();
        self.state.drafts.insert(room.clone(), draft);
        let position = self
            .editing
            .as_ref()
            .and_then(|(_, current)| own.iter().position(|(id, _)| id == current));
        let target = match (previous, position) {
            (true, None) => Some(own.len() - 1),
            (true, Some(index)) if index > 0 => Some(index - 1),
            (true, Some(_)) => return, // oldest own reached: stay
            (false, Some(index)) if index + 1 < own.len() => Some(index + 1),
            (false, Some(_)) => {
                // Newest own reached: exit the edit, draft restored.
                self.cancel_editing(cx);
                return;
            }
            (false, None) => return,
        };
        let Some((msg_id, text)) = target.map(|index| own[index].clone()) else {
            return;
        };
        self.begin_edit(room, msg_id, text, window, cx);
        if !previous {
            self.composer
                .update(cx, |composer, cx| composer.caret_to_start(cx));
        }
    }

    /// Recomputes the completion popup from the text around the caret
    /// (m12 F10): @ over senders seen in the room plus the own name,
    /// : over the unicode shortcodes.
    fn update_suggestions(&mut self, text: &str, caret: usize, cx: &mut Context<Self>) {
        let suggest = completion_token(text, caret).and_then(|(token_start, sigil, query)| {
            let mut candidates: Vec<String> = match sigil {
                '@' => {
                    let mut names: std::collections::BTreeSet<String> = self
                        .state
                        .messages
                        .get(self.state.active_room.as_deref()?)
                        .map(|messages| {
                            messages
                                .iter()
                                .map(|message| message.sender.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(me) = self.state.username.clone() {
                        names.insert(me);
                    }
                    let query = query.to_lowercase();
                    names
                        .into_iter()
                        .filter(|name| name.to_lowercase().starts_with(&query))
                        .collect()
                }
                _ => {
                    let query = query.to_lowercase();
                    markdown::SHORTCODES
                        .iter()
                        .map(|(name, _)| (*name).to_owned())
                        .filter(|name| name.starts_with(&query))
                        .collect()
                }
            };
            candidates.sort();
            candidates.truncate(5);
            (!candidates.is_empty()).then_some(Suggest {
                token_start,
                caret,
                items: candidates,
                selected: 0,
                emoji: sigil == ':',
            })
        });
        self.composer
            .update(cx, |composer, cx| composer.set_suggest(suggest, cx));
        cx.notify();
    }

    /// An image pasted into the composer rides the upload path (m12
    /// F14): bytes land in a temp file, the attach flow uploads.
    fn upload_clipboard_image(&mut self, bytes: &[u8], ext: &str, cx: &mut Context<Self>) {
        let Some(room_id) = self.state.active_room.clone() else {
            self.state.error = Some("open a room first".to_owned());
            cx.notify();
            return;
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!("blazer-chat-paste-{stamp}.{ext}"));
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.send_command(ChatCommand::UploadFile {
                    room_id,
                    path: path.display().to_string(),
                    msg: String::new(),
                });
            }
            Err(error) => self.state.error = Some(format!("paste: {error}")),
        }
        cx.notify();
    }

    fn send_message(&mut self, cx: &mut Context<Self>) {
        let Some(room_id) = self.state.active_room.clone() else {
            self.state.error = Some("open a room first".to_owned());
            cx.notify();
            return;
        };
        let text = self.composer.read(cx).content().trim().to_owned();
        if text.is_empty() {
            return;
        }
        // Unicode shortcodes resolve on the wire (:smile: → 😄, m12
        // F9); unknown names stay literal for the server's custom
        // emoji.
        let text = markdown::resolve_shortcodes(&text);
        self.composer.update(cx, |composer, cx| composer.reset(cx));
        // A thread reply posts into the open thread; the main composer
        // posts to the room (SendThread was unreachable from the UI).
        let command = match self.editing.take() {
            Some((edit_room, msg_id)) => ChatCommand::EditMessage {
                room_id: edit_room,
                msg_id,
                text: text.clone(),
            },
            None => match self.state.open_thread.clone() {
                Some((thread_room, tmid)) => ChatCommand::SendThread {
                    room_id: thread_room,
                    tmid,
                    text: text.clone(),
                },
                None => {
                    // A pending quote card for this room rides the send
                    // (m12 F6); the Sent fold consumes it.
                    let quote = self
                        .state
                        .quote
                        .as_ref()
                        .filter(|(quote_room, _)| quote_room == &room_id)
                        .map(|(_, msg_id)| msg_id.clone());
                    ChatCommand::Send {
                        room_id: room_id.clone(),
                        text: text.clone(),
                        quote_msg_id: quote,
                    }
                }
            },
        };
        self.state.drafts.remove(&room_id);
        self.send_command(ChatCommand::SaveDraft {
            room_id: room_id.clone(),
            text: String::new(),
        });
        self.send_command(command);
        cx.notify();
    }
}

impl ChatRoot {
    /// Toggles a 👍 reaction on one message (chat.react toggles on
    /// repeat).
    /// Rocket.Chat reactions are emoji NAMES on the wire (`+1`, `tada`,
    /// …); the unicode glyph is display-only.
    /// Opens or closes one row's labeled action menu.
    pub(crate) fn toggle_menu(&mut self, room_id: String, msg_id: String, cx: &mut Context<Self>) {
        self.open_menu = match self.open_menu.as_ref() {
            Some(open) if open.0 == room_id && open.1 == msg_id => None,
            _ => Some((room_id, msg_id)),
        };
        cx.notify();
    }

    /// Stars or unstars a message through the server; the fold applies
    /// the flip locally.
    pub(crate) fn toggle_star(&mut self, msg_id: String, starred: bool, cx: &mut Context<Self>) {
        self.send_command(ChatCommand::StarMessage {
            msg_id,
            star: !starred,
        });
        self.open_menu = None;
        cx.notify();
    }

    /// The original client's permalink:
    /// `{origin}/{channel|group}/{room name}/msg/{id}` — channels and
    /// groups only (DM links address the peer, which rooms don't carry).
    pub(crate) fn permalink(&self, room_id: &str, msg_id: &str) -> Option<String> {
        let origin = self.state.origin.as_deref()?;
        let room = self.state.rooms.iter().find(|room| room.id == room_id)?;
        let path = match room.kind {
            'c' => "channel",
            'p' => "group",
            _ => return None,
        };
        Some(format!("{origin}/{path}/{}/msg/{msg_id}", room.name))
    }

    /// Logs out: the server session dies, the UI resets to logged-out.
    fn logout(&mut self, cx: &mut Context<Self>) {
        self.send_command(ChatCommand::Logout);
        self.user_menu_open = false;
        cx.notify();
    }

    /// Browser-style zoom: rem scaling resizes every styled element
    /// (gpui lays spacing and text out in rems).
    fn zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        window.set_rem_size((window.rem_size() * 1.1).min(px(32.0)));
        cx.notify();
    }

    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        window.set_rem_size((window.rem_size() / 1.1).max(px(10.0)));
        cx.notify();
    }

    fn zoom_reset(&mut self, _: &ZoomReset, window: &mut Window, cx: &mut Context<Self>) {
        window.set_rem_size(px(16.0));
        cx.notify();
    }

    pub(crate) fn toggle_reaction(
        &mut self,
        room_id: &str,
        msg_id: &str,
        emoji: &str,
        cx: &mut Context<Self>,
    ) {
        self.send_command(ChatCommand::React {
            room_id: room_id.to_owned(),
            msg_id: msg_id.to_owned(),
            emoji: emoji.to_owned(),
        });
        cx.notify();
    }

    /// Puts a message back into the composer for editing; the next
    /// submit sends `chat.update` instead of a new message.
    pub(crate) fn begin_edit(
        &mut self,
        room_id: String,
        msg_id: String,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing = Some((room_id, msg_id));
        self.composer
            .update(cx, |composer, cx| composer.set_content(&text, cx));
        window.focus(&self.composer.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Sends the current search box content as the active query; empty
    /// clears the results.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).content().trim().to_owned();
        self.search_rooms = room_hits(&query, &self.state.rooms);
        self.send_command(ChatCommand::Search(query.clone()));
        cx.notify();
    }

    /// Focuses the search strip: the Ctrl+K room-jump path (m12 F3).
    fn focus_room_jump(
        &mut self,
        _: &super::FocusSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = self.search.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    pub(crate) fn delete_message(&mut self, room_id: &str, msg_id: &str, cx: &mut Context<Self>) {
        self.send_command(ChatCommand::DeleteMessage {
            room_id: room_id.to_owned(),
            msg_id: msg_id.to_owned(),
        });
        cx.notify();
    }
}

/// Rooms whose names contain the query (case-insensitive). An empty
/// query matches nothing — the results pane stays a search surface.
fn room_hits(query: &str, rooms: &[Room]) -> Vec<(String, String)> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    rooms
        .iter()
        .filter(|room| fuzzy_subsequence(&query, &room.name))
        .map(|room| (room.id.clone(), room.name.clone()))
        .collect()
}

/// Subsequence match — every needle character appears in order in the
/// haystack (case-insensitive): the spotlight's fuzzy behavior
/// ("gnrl" finds "general", "cap" finds "capybaras"; m4 search).
fn fuzzy_subsequence(needle: &str, haystack: &str) -> bool {
    let needle = needle.to_lowercase();
    let haystack = haystack.to_lowercase();
    let mut rest = haystack.chars();
    needle.chars().all(|c| rest.any(|h| h == c))
}

impl Focusable for ChatRoot {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideEntry {
    Section(&'static str),
    Room(usize),
}

/// Sidebar entries in the original client's order: Unread first, then
/// Channels, Private Groups, Direct Messages. A room with a badge
/// belongs to Unread only; empty sections drop out; each section
/// keeps the newest-activity order of the source list.
fn side_entries(rooms: &[Room]) -> Vec<SideEntry> {
    let mut entries = Vec::new();
    let mut push = |label: &'static str, pick: fn(&Room) -> bool| {
        let picked: Vec<usize> = rooms
            .iter()
            .enumerate()
            .filter(|(_, room)| pick(room))
            .map(|(index, _)| index)
            .collect();
        if !picked.is_empty() {
            entries.push(SideEntry::Section(label));
            entries.extend(picked.into_iter().map(SideEntry::Room));
        }
    };
    push("UNREAD", |room| room.unread > 0);
    push("CHANNELS", |room| room.kind == 'c' && room.unread == 0);
    push("PRIVATE GROUPS", |room| {
        room.kind == 'p' && room.unread == 0
    });
    push("DIRECT MESSAGES", |room| {
        room.kind == 'd' && room.unread == 0
    });
    entries
}

/// One sidebar section header. uniform_list stretches every row to
/// the tallest, so the header rides the same 48 px row the rooms use
/// — the label sits at its bottom edge like the original's sticky
/// headers.
fn section_row(label: &'static str, p: Palette) -> AnyElement {
    div()
        .h(px(48.0))
        .flex()
        .flex_col()
        .justify_end()
        .px_2()
        .pb_1()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(p.muted))
                .child(label),
        )
        .into_any_element()
}

fn panel(
    title: &str,
    subtitle: Option<&str>,
    content: impl IntoElement,
    width: Option<Pixels>,
    p: Palette,
) -> impl IntoElement {
    // A fixed-width panel must not carry flex_1's zero basis: build the
    // sizing first, then the common chrome.
    let sized = match width {
        Some(width) => div().w(width).flex_none(),
        None => div().flex_1().min_w_0(),
    };
    sized
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .bg(rgb(p.panel))
        .border_1()
        .border_color(rgb(p.border))
        .rounded_md()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(p.text))
                        .child(title.to_owned()),
                )
                .children(subtitle.map(|subtitle| {
                    div()
                        .text_xs()
                        .text_color(rgb(p.muted))
                        .truncate()
                        .child(SharedString::from(subtitle.to_owned()))
                })),
        )
        .child(content)
}

impl gpui::Render for ChatRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        record_frame(
            &mut self.last_frame_start,
            &mut self.frame_ms,
            std::time::Instant::now(),
        );
        let p = self.palette;
        let who = self
            .state
            .username
            .clone()
            .unwrap_or_else(|| "not logged in".into());
        let error = div().when_some(self.state.error.clone(), |row, message| {
            row.child(
                div()
                    .text_sm()
                    .text_color(rgb(p.danger))
                    .child(format!("error: {message}")),
            )
        });

        // uniform_list materializes only the visible rows — the room list
        // runs into the hundreds on real servers. Sections read like the
        // original client (m12 F4).
        let this = cx.entity();
        let active = self.state.active_room.clone();
        let entries = side_entries(&self.state.rooms);
        let room_count = entries.len();
        let rooms = uniform_list("rooms", room_count, move |range, _window, cx| {
            let root = this.read(cx);
            let p = root.palette;

            range
                .map(|index| match entries[index] {
                    SideEntry::Section(label) => section_row(label, p),
                    SideEntry::Room(room_index) => {
                        let room = &root.state.rooms[room_index];
                        let room_id = room.id.clone();
                        let selected = active.as_deref() == Some(room.id.as_str());
                        let unread = room.unread;
                        let this = this.clone();
                        let menu_open = root.room_menu.as_deref() == Some(room.id.as_str());
                        let room_menu = menu_open.then(|| {
                            let item = |label: &'static str, suffix: &'static str| {
                                div()
                                    .id(SharedString::from(format!("{suffix}-{}", room.id)))
                                    .px_2()
                                    .py_1()
                                    .rounded_xs()
                                    .text_xs()
                                    .text_color(rgb(p.text))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(p.row_hover)))
                                    .child(label)
                            };
                            deferred(
                                anchored().anchor(Anchor::TopLeft).child(
                                    div()
                                        .occlude()
                                        .flex()
                                        .flex_col()
                                        .min_w(px(150.0))
                                        .p_1()
                                        .gap_0p5()
                                        .bg(rgb(p.panel))
                                        .border_1()
                                        .border_color(rgb(p.border))
                                        .rounded_md()
                                        .shadow_lg()
                                        .child(item("🚩 Mark Unread", "r-unread").on_click({
                                            let this = this.clone();
                                            let room_id = room_id.clone();
                                            move |_, _, cx| {
                                                this.update(cx, |chat, cx| {
                                                    chat.room_menu = None;
                                                    chat.send_command(
                                                        ChatCommand::MarkRoomUnread {
                                                            room_id: room_id.clone(),
                                                        },
                                                    );
                                                    cx.notify();
                                                });
                                            }
                                        }))
                                        .child(item("✂️ Hide", "r-hide").on_click({
                                            let this = this.clone();
                                            let room_id = room_id.clone();
                                            move |_, _, cx| {
                                                this.update(cx, |chat, cx| {
                                                    chat.room_menu = None;
                                                    chat.send_command(ChatCommand::HideRoom {
                                                        room_id: room_id.clone(),
                                                    });
                                                    cx.notify();
                                                });
                                            }
                                        })),
                                ),
                            )
                        });
                        div()
                            .id(SharedString::from(room.id.clone()))
                            .role(Role::ListItem)
                            .aria_label(match room.unread {
                                0 => room.name.clone(),
                                unread => format!("{} — {} unread", room.name, unread),
                            })
                            .px_2()
                            .py_1()
                            .h(px(48.0))
                            .rounded_sm()
                            .text_sm()
                            .overflow_hidden()
                            .cursor_pointer()
                            .when(selected, |row| row.bg(rgb(p.accent)))
                            .when(selected, |row| row.aria_selected(true))
                            .hover(|style| style.bg(rgb(p.border)))
                            .on_click({
                                let this = this.clone();
                                let room_id = room_id.clone();
                                move |_, _, cx| {
                                    this.update(cx, |chat, cx| {
                                        chat.room_menu = None;
                                        chat.open_room(room_id.clone(), cx);
                                    });
                                }
                            })
                            .on_mouse_down(MouseButton::Right, {
                                let this = this.clone();
                                let room_id = room_id.clone();
                                move |_, _, cx| {
                                    this.update(cx, |chat, cx| {
                                        chat.room_menu = Some(room_id.clone());
                                        cx.notify();
                                    });
                                }
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .flex()
                                            .gap_1()
                                            .items_baseline()
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .flex_1()
                                                    .overflow_hidden()
                                                    .child(SharedString::from(room.name.clone())),
                                            )
                                            .children(room.lm.as_ref().map(|ts| {
                                                div()
                                                    .flex_none()
                                                    .text_xs()
                                                    .text_color(rgb(p.muted))
                                                    .child(SharedString::from(pretty_time(ts)))
                                            }))
                                            .when(unread > 0, |line| {
                                                line.child(
                                                    div()
                                                        .flex_none()
                                                        .px_1()
                                                        .rounded_xs()
                                                        .bg(rgb(p.accent))
                                                        .text_xs()
                                                        .child(SharedString::from(
                                                            unread.to_string(),
                                                        )),
                                                )
                                            })
                                            .when(room.mentions > 0, |line| {
                                                line.child(
                                                    div()
                                                        .flex_none()
                                                        .px_1()
                                                        .rounded_xs()
                                                        .bg(rgb(p.danger))
                                                        .text_xs()
                                                        .text_color(rgb(p.accent_fg))
                                                        .child(SharedString::from(format!(
                                                            "@{}",
                                                            room.mentions
                                                        ))),
                                                )
                                            }),
                                    )
                                    .children(room.preview.as_ref().map(|(sender, text)| {
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .text_color(rgb(p.muted))
                                            .child(SharedString::from(format!("{sender}: {text}")))
                                    })),
                            )
                            .children(room_menu)
                            .into_any_element()
                    }
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0();

        // Virtualized pane (m10 S4): list() materializes only the
        // visible window, so per-frame cost no longer scales with the
        // room's cache. The old plain-children pane built every row
        // every frame — the large-room lag. The 81ace8a3 probe's
        // "negative" was a misapplied height (post-mortem: deepseek
        // S4), not a gpui defect.
        // Pane render inputs, snapshot per frame (m13 S2a 2.4(7)).
        let user_card_info = self.user_card.as_ref().and_then(|name| {
            self.state
                .user_info
                .get(name)
                .map(|info| (name.clone(), info.clone()))
        });
        let pane_inputs = PaneInputs {
            editing: self.editing.clone(),
            user_card: self.user_card.clone(),
            open_menu: self.open_menu.clone(),
            me: self.state.username.clone(),
            emoji: self.state.emoji.clone(),
            user_card_info,
            link_ready: self
                .state
                .active_room
                .as_deref()
                .and_then(|room| self.permalink(room, ""))
                .is_some(),
            palette: p,
        };
        let messages = self.pane.read(cx).element(&pane_inputs);

        // Popup toasts: the pending notification list, docked above the
        // pane (the original client's bottom-right toasts; in-window so
        // nothing floats over the operator's desktop). Click opens the
        // room; ✖ dismisses.
        let toasts = div().flex().flex_none().flex_col().gap_1().children(
            self.state
                .notifications
                .iter()
                .enumerate()
                .map(|(index, note)| {
                    let this = cx.entity();
                    let room_id = note.room_id.clone();
                    let body = note.message.clone().unwrap_or_else(|| note.text.clone());
                    div()
                        .id(SharedString::from(format!("toast-{index}")))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.accent))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(p.row_hover)))
                        .on_click({
                            let this = this.clone();
                            move |_, _, cx| {
                                this.update(cx, |chat, cx| {
                                    chat.open_room(room_id.clone(), cx);
                                    chat.state.notifications.remove(index);
                                    cx.notify();
                                });
                            }
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(p.text))
                                        .child(SharedString::from(note.title.clone())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(p.muted))
                                        .truncate()
                                        .child(SharedString::from(body)),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("toast-close-{index}")))
                                .flex_none()
                                .px_1()
                                .rounded_xs()
                                .text_xs()
                                .text_color(rgb(p.muted))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(p.border)).text_color(rgb(p.text)))
                                .child("✖")
                                .on_click({
                                    let this = this.clone();
                                    move |_, _, cx| {
                                        this.update(cx, |chat, cx| {
                                            chat.state.notifications.remove(index);
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                }),
        );

        // Search strip: a query switches the pane to local results.
        let search_this = cx.entity();
        let search_strip = {
            let search_this = search_this.clone();
            div()
                .flex()
                .flex_1()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .border_1()
                        .border_color(rgb(p.border))
                        .rounded_md()
                        .child(self.search.clone()),
                )
                .child(
                    div()
                        .id("search")
                        .role(Role::Button)
                        .aria_label("Search rooms")
                        .px_3()
                        .py_1()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.border))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(p.accent)))
                        .child("🔍")
                        .on_click(move |_, _, cx| {
                            search_this.update(cx, |chat, cx| chat.run_search(cx));
                        }),
                )
        };
        // Scroll-to-top paging: one older slice per visit to the top
        // edge (edge-triggered — a pinned top must not re-fire every
        // frame), stopping at the pane cap or when history runs out.
        if let Some(command) = self
            .pane
            .update(cx, |pane, _| pane.poll_paging(&self.state))
        {
            self.send_command(command);
        }

        let pane_title = self
            .state
            .rooms
            .iter()
            .find(|room| Some(room.id.as_str()) == self.state.active_room.as_deref())
            .map(|room| room.name.clone())
            .unwrap_or_else(|| "messages".to_owned());
        let pane = if let Some((room_id, tmid)) = self.state.open_thread.clone() {
            // Thread view: back button, then the thread's messages —
            // the same full rows as the main pane (avatars, times,
            // attachments with image previews, reaction pills).
            let this = search_this.clone();
            let thread_messages: Vec<Msg> = self.state.thread(&tmid).to_vec();
            let state = &self.state;
            let rows = self.pane.update(cx, |pane, _| {
                let views: Vec<MessageView> = thread_messages
                    .iter()
                    .map(|message| pane.message_view(state, message))
                    .collect();
                views
                    .iter()
                    .enumerate()
                    .map(|(index, view)| {
                        let grouped = index > 0 && adjacent_grouped(&views[index - 1], view);
                        pane.message_row_view(view, grouped, &pane_inputs)
                    })
                    .collect::<Vec<_>>()
            });
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap_1()
                .child(
                    div()
                        .id("thread-back")
                        .flex_none()
                        .px_2()
                        .py_0p5()
                        .rounded_xs()
                        .bg(rgb(p.panel))
                        .text_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(p.accent)))
                        .child("← thread")
                        .on_click(move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.send_command(ChatCommand::CloseThread);
                                let _room_id = room_id.clone();
                                cx.notify();
                            });
                        }),
                )
                .child(
                    div()
                        .id("thread-scroll")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .gap_1()
                        .overflow_y_scroll()
                        .track_scroll(&self.thread_scroll)
                        .children(rows),
                )
                .into_any_element()
        } else if let Some(query) = self.state.search_query.as_deref()
            && !query.trim().is_empty()
        {
            let hits = self.state.search(query);
            let mut results: Vec<gpui::AnyElement> = self
                .search_rooms
                .iter()
                .map(|(room_id, name)| {
                    let this = search_this.clone();
                    let room_id = room_id.clone();
                    div()
                        .id(SharedString::from(format!("jump-{room_id}")))
                        .role(Role::Button)
                        .aria_label(format!("Jump to room {name}"))
                        .flex()
                        .gap_2()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(p.accent)))
                        .on_click(move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.open_room(room_id.clone(), cx);
                                chat.search.update(cx, |box_, cx| {
                                    box_.set_content("", cx);
                                });
                                chat.send_command(ChatCommand::Search(String::new()));
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(p.text))
                                .child(SharedString::from(format!("# {name}"))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(rgb(p.muted))
                                .child("Jump to room"),
                        )
                        .into_any_element()
                })
                .collect::<Vec<_>>();
            let message_hits = hits
                .iter()
                .map(|hit| {
                    let this = search_this.clone();
                    let room_id = hit.room_id.clone();
                    div()
                        .id(SharedString::from(format!("hit-{}", hit.room_id)))
                        .flex()
                        .gap_2()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(p.border)))
                        .on_click(move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.open_room(room_id.clone(), cx);
                                chat.search.update(cx, |box_, cx| {
                                    box_.set_content("", cx);
                                });
                                // Clearing the box alone leaves the query
                                // in state: the results pane would keep
                                // covering the room that was just opened.
                                chat.send_command(ChatCommand::Search(String::new()));
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(p.accent))
                                .child(SharedString::from(hit.room.clone())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(p.muted))
                                .child(SharedString::from(hit.sender.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .child(SharedString::from(hit.text.clone())),
                        )
                })
                .map(|element| element.into_any_element())
                .collect::<Vec<_>>();
            results.extend(message_hits);
            div()
                .id("results-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap_1()
                .overflow_y_scroll()
                .track_scroll(&self.results_scroll)
                .children(results)
                .into_any_element()
        } else if let Some((_room_id, pinned)) = self.state.pinned.clone() {
            // Read-only pinned tab (m12 F7): the same full rows as the
            // main pane.
            let this = search_this.clone();
            let state = &self.state;
            let mut rows: Vec<gpui::AnyElement> = self.pane.update(cx, |pane, _| {
                let views: Vec<MessageView> = pinned
                    .iter()
                    .map(|message| pane.message_view(state, message))
                    .collect();
                views
                    .iter()
                    .enumerate()
                    .map(|(index, view)| {
                        let grouped = index > 0 && adjacent_grouped(&views[index - 1], view);
                        pane.message_row_view(view, grouped, &pane_inputs)
                    })
                    .collect::<Vec<_>>()
            });
            if rows.is_empty() {
                rows.push(
                    div()
                        .text_sm()
                        .text_color(rgb(p.muted))
                        .child("Nothing pinned in this room yet.")
                        .into_any_element(),
                );
            }
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap_1()
                .child(
                    div()
                        .id("pinned-back")
                        .flex_none()
                        .px_2()
                        .py_0p5()
                        .rounded_xs()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(p.border)))
                        .child("← 📌 Pinned messages")
                        .on_click({
                            let this = this.clone();
                            move |_, _, cx| {
                                this.update(cx, |chat, cx| {
                                    chat.state.pinned = None;
                                    cx.notify();
                                });
                            }
                        }),
                )
                .child(
                    div()
                        .id("pinned-scroll")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .gap_1()
                        .overflow_y_scroll()
                        .track_scroll(&self.thread_scroll)
                        .children(rows),
                )
                .into_any_element()
        } else if self.show_info {
            // Room info panel (m12 F7), reachable from the header.
            let room = self
                .state
                .rooms
                .iter()
                .find(|room| Some(room.id.as_str()) == self.state.active_room.as_deref());
            let kind_label = match room.map(|room| room.kind) {
                Some('p') => "private group",
                Some('d') => "direct message",
                _ => "channel",
            };
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap_1()
                .child(
                    div()
                        .id("info-back")
                        .flex_none()
                        .px_2()
                        .py_0p5()
                        .rounded_xs()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(p.border)))
                        .child("← ℹ️ Room info")
                        .on_click({
                            let this = search_this.clone();
                            move |_, _, cx| {
                                this.update(cx, |chat, cx| {
                                    chat.show_info = false;
                                    cx.notify();
                                });
                            }
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_3()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.border))
                        .rounded_md()
                        .child(
                            div().text_sm().font_weight(FontWeight::MEDIUM).child(
                                SharedString::from(
                                    room.map(|room| room.name.clone())
                                        .unwrap_or_else(|| "no room".to_owned()),
                                ),
                            ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(p.muted))
                                .child(SharedString::from(kind_label)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(p.text))
                                .child(SharedString::from(
                                    room.and_then(|room| room.topic.clone())
                                        .unwrap_or_else(|| "No topic set.".to_owned()),
                                )),
                        ),
                )
                .into_any_element()
        } else {
            messages.into_any_element()
        };

        // The top bar, the original's shape: brand, centered room
        // search, account avatar whose click drops the account menu
        // (presence + logout). gpui 0.2.2 has no anchored popover, so
        // the menu card flows below the bar, right-aligned.
        let this_bar = cx.entity();
        let me = self.state.username.clone().unwrap_or_else(|| who.clone());
        let me_avatar = match self
            .state
            .avatars
            .get(self.state.username.as_deref().unwrap_or_default())
        {
            Some(path) => img(std::path::PathBuf::from(path))
                .size(rems(2.0))
                .rounded_full()
                .into_any_element(),
            None => avatar(&me, p).into_any_element(),
        };
        let status_item =
            |dot: u32, label: &'static str, key: &'static str, taken: Entity<ChatRoot>| {
                div()
                    .id(SharedString::from(format!("status-{key}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_xs()
                    .text_sm()
                    .text_color(rgb(p.text))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(p.row_hover)))
                    .child(div().size(px(10.0)).rounded_full().bg(rgb(dot)))
                    .child(label)
                    .on_click(move |_, _, cx| {
                        taken.update(cx, |chat, cx| {
                            chat.send_command(ChatCommand::SetStatus {
                                status: key.to_owned(),
                            });
                            cx.notify();
                        });
                    })
            };
        let presence = self.state.presence.clone();
        // The account menu floats anchored to the bar (SwitchAnchor
        // keeps it on screen); nothing renders while closed.
        let user_menu = self.user_menu_open.then(|| {
            deferred(
                anchored().anchor(Anchor::TopRight).child(
                    div()
                        .occlude()
                        .flex()
                        .flex_col()
                        .min_w(px(220.0))
                        .p_1()
                        .gap_0p5()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.border))
                        .rounded_md()
                        .shadow_lg()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .p_2()
                                .child(match self.state.avatars.get(me.as_str()) {
                                    Some(path) => img(std::path::PathBuf::from(path))
                                        .size(rems(2.5))
                                        .rounded_full()
                                        .into_any_element(),
                                    None => div()
                                        .size(rems(2.5))
                                        .child(avatar(&me, p))
                                        .into_any_element(),
                                })
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(rgb(p.text))
                                                .child(SharedString::from(me.clone())),
                                        )
                                        .child(div().text_xs().text_color(rgb(p.muted)).child(
                                            SharedString::from(match presence.as_deref() {
                                                Some(status) => status.to_owned(),
                                                None => "online".to_owned(),
                                            }),
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .px_2()
                                .pt_1()
                                .pb_0p5()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(p.muted))
                                .child("Status"),
                        )
                        .child(status_item(p.online, "online", "online", this_bar.clone()))
                        .child(status_item(p.away, "away", "away", this_bar.clone()))
                        .child(status_item(p.busy, "busy", "busy", this_bar.clone()))
                        .child(
                            div()
                                .px_2()
                                .pt_1()
                                .pb_0p5()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(p.muted))
                                .child("Account"),
                        )
                        .child(
                            div()
                                .id("menu-logout")
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded_xs()
                                .text_sm()
                                .text_color(rgb(p.danger))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(p.row_hover)))
                                .child("⎋ Log out")
                                .on_click({
                                    let this = this_bar.clone();
                                    move |_, _, cx| {
                                        this.update(cx, |chat, cx| chat.logout(cx));
                                    }
                                }),
                        ),
                ),
            )
            .into_any_element()
        });
        let top_bar = div()
            .flex_none()
            .flex()
            .flex_col()
            .items_end()
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .bg(rgb(p.panel))
                    .border_b_1()
                    .border_color(rgb(p.border))
                    .rounded_md()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(p.text))
                            .child("blazer-chat"),
                    )
                    .child(div().flex_1())
                    .child(div().flex_none().flex().w(px(480.0)).child(search_strip))
                    .child(div().flex_1())
                    .when_some(self.state.active_room.clone(), |bar, room_id| {
                        bar.child(
                            div()
                                .id("pins-tab")
                                .role(Role::Button)
                                .aria_label("Pinned messages")
                                .flex_none()
                                .px_2()
                                .py_1()
                                .rounded_xs()
                                .text_sm()
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(p.border)))
                                .child("📌")
                                .on_click({
                                    let this = this_bar.clone();
                                    let room_id = room_id.clone();
                                    move |_, _, cx| {
                                        this.update(cx, |chat, cx| {
                                            chat.show_info = false;
                                            let open = match chat.state.pinned.take() {
                                                // Same room's tab open → close it.
                                                Some((rid, _)) if rid == room_id => false,
                                                // No tab or another room's tab → open here.
                                                _ => {
                                                    chat.send_command(
                                                        ChatCommand::PinnedMessages {
                                                            room_id: room_id.clone(),
                                                        },
                                                    );
                                                    true
                                                }
                                            };
                                            chat.state.pinned =
                                                open.then(|| (room_id.clone(), Vec::new()));
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                        .child(
                            div()
                                .id("room-info")
                                .role(Role::Button)
                                .aria_label("Room info")
                                .flex_none()
                                .px_2()
                                .py_1()
                                .rounded_xs()
                                .text_sm()
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(p.border)))
                                .child("ℹ️")
                                .on_click({
                                    let this = this_bar.clone();
                                    move |_, _, cx| {
                                        this.update(cx, |chat, cx| {
                                            chat.state.pinned = None;
                                            chat.show_info = !chat.show_info;
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(p.muted))
                            .pr_1()
                            .child(SharedString::from(me.clone())),
                    )
                    .child(
                        div()
                            .id("user-avatar")
                            .role(Role::Button)
                            .aria_label("Account menu")
                            .flex_none()
                            .cursor_pointer()
                            .rounded_full()
                            .hover(|style| style.opacity(0.85))
                            .on_click(cx.listener(|chat, _, _, cx| {
                                chat.user_menu_open = !chat.user_menu_open;
                                cx.notify();
                            }))
                            .child(me_avatar),
                    ),
            )
            .children(user_menu);

        // The completion popup (m12 F10): Tab/Enter accept, arrows
        // move, a chip click accepts directly.
        let suggest_popup = self.composer.read(cx).suggest().cloned().map(|suggest| {
            let composer = self.composer.clone();
            let chips: Vec<gpui::AnyElement> = suggest
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let composer = composer.clone();
                    let label = match suggest.emoji {
                        true => format!(
                            ":{item}: {}",
                            markdown::shortcode_glyph(item).unwrap_or("?")
                        ),
                        false => format!("@{item}"),
                    };
                    div()
                        .id(SharedString::from(format!("sug-{index}-{item}")))
                        .px_2()
                        .py_0p5()
                        .rounded_xs()
                        .text_xs()
                        .cursor_pointer()
                        .when(index == suggest.selected, |chip| {
                            chip.bg(rgb(p.accent)).text_color(rgb(p.accent_fg))
                        })
                        .when(index != suggest.selected, |chip| {
                            chip.text_color(rgb(p.text))
                                .hover(|style| style.bg(rgb(p.row_hover)))
                        })
                        .child(SharedString::from(label))
                        .on_click(
                            move |_: &gpui::ClickEvent,
                                  _: &mut gpui::Window,
                                  cx: &mut gpui::App| {
                                composer
                                    .update(cx, |composer, cx| composer.pick_suggestion(index, cx));
                            },
                        )
                        .into_any_element()
                })
                .collect();
            div()
                .flex()
                .flex_col()
                .flex_none()
                .p_1()
                .gap_0p5()
                .bg(rgb(p.panel))
                .border_1()
                .border_color(rgb(p.border))
                .rounded_md()
                .shadow_lg()
                .children(chips)
        });

        // The formatting toolbar: wrap-on-type over the selection
        // (m12 F11); Ctrl+B/I drive the same paths.
        let fmt_button = |label: &'static str, id: &'static str, marker: &'static str| {
            let composer = self.composer.clone();
            div()
                .id(SharedString::from(id))
                .role(Role::Button)
                .aria_label(label)
                .px_1p5()
                .rounded_xs()
                .text_xs()
                .font_weight(if marker == "**" {
                    FontWeight::BOLD
                } else {
                    FontWeight::MEDIUM
                })
                .text_color(rgb(p.muted))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(p.border)))
                .child(label)
                .on_click({
                    let composer = composer.clone();
                    move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
                        composer.update(cx, |composer, cx| composer.apply_wrap(marker, cx));
                    }
                })
        };
        let emoji_button = div()
            .id("emoji-picker")
            .role(Role::Button)
            .aria_label("Emoji")
            .px_1p5()
            .rounded_xs()
            .text_xs()
            .text_color(rgb(p.muted))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(p.border)))
            .child("😊")
            .on_click({
                let this = cx.entity();
                move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
                    this.update(cx, |chat, cx| {
                        chat.emoji_picker_open = !chat.emoji_picker_open;
                        cx.notify();
                    });
                }
            });
        let picker_rows: Vec<gpui::AnyElement> = markdown::SHORTCODES
            .iter()
            .map(|(_, glyph)| {
                let composer = self.composer.clone();
                let this = cx.entity();
                div()
                    .id(SharedString::from(format!("pick-{glyph}")))
                    .px_1()
                    .py_0p5()
                    .text_lg()
                    .cursor_pointer()
                    .rounded_xs()
                    .hover(|style| style.bg(rgb(p.row_hover)))
                    .child(*glyph)
                    .on_click(
                        move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
                            composer.update(cx, |composer, cx| composer.insert_at_caret(glyph, cx));
                            this.update(cx, |chat, cx| {
                                chat.emoji_picker_open = false;
                                cx.notify();
                            });
                        },
                    )
                    .into_any_element()
            })
            .collect();
        let emoji_picker = self.emoji_picker_open.then(|| {
            deferred(
                anchored().anchor(Anchor::TopLeft).child(
                    div()
                        .occlude()
                        .flex()
                        .flex_wrap()
                        .w(px(230.0))
                        .p_1()
                        .gap_0p5()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.border))
                        .rounded_md()
                        .shadow_lg()
                        .children(picker_rows),
                ),
            )
        });
        let toolbar = div()
            .flex()
            .flex_none()
            .gap_1()
            .child(fmt_button("B", "fmt-bold", "**"))
            .child(fmt_button("I", "fmt-italic", "_"))
            .child(fmt_button("S", "fmt-strike", "~~"))
            .child(fmt_button("</>", "fmt-code", "`"))
            .child(emoji_button)
            .children(emoji_picker);

        // The pending quote card rides above the composer; a foreign
        // room's quote never shows (m12 F6).
        let quote_card = self
            .state
            .quote
            .as_ref()
            .and_then(|(quote_room, quote_msg)| {
                let visible = self.state.active_room.as_deref() == Some(quote_room.as_str());
                visible.then(|| {
                    let known = self
                        .pane
                        .read(cx)
                        .view(quote_msg)
                        .map(|view| (view.sender.clone(), view.text.clone()))
                        .or_else(|| {
                            self.state.messages.get(quote_room).and_then(|messages| {
                                messages
                                    .iter()
                                    .find(|message| message.id == *quote_msg)
                                    .map(|message| (message.sender.clone(), message.text.clone()))
                            })
                        });
                    let (sender, snippet) = known
                        .map(|(sender, text)| {
                            let line = text.lines().next().unwrap_or_default();
                            let short: String = line.chars().take(80).collect();
                            (sender, short)
                        })
                        .unwrap_or_else(|| ("message".to_owned(), String::new()));
                    let label = match snippet.is_empty() {
                        true => format!("Quoting {sender}"),
                        false => format!("Quoting {sender}: {snippet}"),
                    };
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.border))
                        .rounded_md()
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(p.accent))
                                .child("↩"),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_xs()
                                .text_color(rgb(p.muted))
                                .child(SharedString::from(label)),
                        )
                        .child(
                            div()
                                .id("quote-clear")
                                .flex_none()
                                .px_1()
                                .text_xs()
                                .text_color(rgb(p.muted))
                                .cursor_pointer()
                                .hover(|style| style.text_color(rgb(p.text)))
                                .child("✕")
                                .on_click({
                                    let this = cx.entity();
                                    move |_, _, cx| {
                                        this.update(cx, |chat, cx| {
                                            chat.state.quote = None;
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                })
            });

        div()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::focus_room_jump))
            .size_full()
            .p_4()
            .gap_3()
            .bg(rgb(p.bg))
            .text_color(rgb(p.text))
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_reset))
            .child(top_bar)
            .child(error)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(panel("rooms", None, rooms, Some(px(240.0)), p))
                    .child(panel(
                        &pane_title,
                        self.state
                            .rooms
                            .iter()
                            .find(|room| {
                                Some(room.id.as_str()) == self.state.active_room.as_deref()
                            })
                            .and_then(|room| room.topic.as_deref()),
                        div().flex().flex_1().min_h_0().child(toasts).child(pane),
                        None,
                        p,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .gap_1()
                    .children(quote_card)
                    .children(suggest_popup)
                    .child(toolbar)
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .gap_2()
                            .child(
                                div()
                                    .id("attach")
                                    .role(Role::Button)
                                    .aria_label("Attach a file")
                                    .flex_none()
                                    .px_2()
                                    .py_1()
                                    .bg(rgb(p.panel))
                                    .border_1()
                                    .border_color(rgb(p.border))
                                    .rounded_md()
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(p.accent)))
                                    .child("📎")
                                    .on_click(cx.listener(|chat, _, _, _cx| {
                                        let tx = chat.pick_tx.clone();
                                        std::thread::spawn(move || {
                                            let picked = rfd::FileDialog::new()
                                                .set_title("Attach a file")
                                                .pick_file();
                                            let _ = tx.send_blocking(picked);
                                        });
                                    })),
                            )
                            .child(div().flex_1().child(self.composer.clone()))
                            .child(
                                div()
                                    .id("send")
                                    .role(Role::Button)
                                    .aria_label(match self.editing.is_some() {
                                        true => "Save edited message",
                                        false => "Send message",
                                    })
                                    .px_3()
                                    .py_1()
                                    .bg(rgb(p.panel))
                                    .border_1()
                                    .border_color(rgb(p.border))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(p.accent)))
                                    .active(|style| style.opacity(0.85))
                                    .on_click(cx.listener(|chat, _, _, cx| chat.send_message(cx)))
                                    .child(match self.editing.is_some() {
                                        true => "save",
                                        false => "send",
                                    }),
                            ),
                    ),
            )
    }
}

/// The word ending at `caret`, if it starts with `@` or `:` — the
/// completion token (m12 F10). Returns the token's start (the sigil
/// included), the sigil and the query after it.
fn completion_token(text: &str, caret: usize) -> Option<(usize, char, String)> {
    let caret = caret.min(text.len());
    let mut start = caret;
    for (index, character) in text[..caret].char_indices().rev() {
        if character.is_whitespace() {
            start = index + character.len_utf8();
            break;
        }
        start = index;
    }
    let token = &text[start..caret];
    let sigil = token.chars().next()?;
    (sigil == '@' || sigil == ':').then(|| (start, sigil, token[1..].to_owned()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn frame_ring_samples_real_frames_only() {
        let mut last = None;
        let mut ring = std::collections::VecDeque::new();
        let start = std::time::Instant::now();
        record_frame(&mut last, &mut ring, start);
        assert!(ring.is_empty(), "the first frame has no predecessor");
        for step in 1..=15 {
            record_frame(
                &mut last,
                &mut ring,
                start + Duration::from_millis(step * 8),
            );
        }
        assert_eq!(ring.len(), 10, "the ring keeps the last 10 intervals");
        assert!(ring.iter().all(|ms| (8.0..16.6).contains(ms)));
    }

    #[test]
    fn room_hits_match_names_case_insensitively() {
        let rooms = vec![
            Room {
                id: "g1".to_owned(),
                name: "general".to_owned(),
                kind: 'c',
                unread: 0,
                lm: None,
                preview: None,
                topic: None,
                mentions: 0,
            },
            Room {
                id: "d1".to_owned(),
                name: "Dev Talk".to_owned(),
                kind: 'c',
                unread: 0,
                lm: None,
                preview: None,
                topic: None,
                mentions: 0,
            },
        ];
        assert!(
            room_hits("", &rooms).is_empty(),
            "empty query matches nothing"
        );
        assert_eq!(
            room_hits("gen", &rooms)
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["g1"]
        );
        assert_eq!(
            room_hits("DEV", &rooms)
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["d1"],
            "matching is case-insensitive"
        );
        assert_eq!(
            room_hits("gnrl", &rooms)
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["g1"],
            "fuzzy: subsequence characters in order"
        );
        assert!(
            room_hits("lz", &rooms).is_empty(),
            "characters must appear in order, not scattered anywhere"
        );
    }

    #[test]
    fn completion_token_finds_the_sigil_word() {
        assert_eq!(
            completion_token("hi @bo", 6),
            Some((3, '@', "bo".to_owned()))
        );
        assert_eq!(
            completion_token(":smi", 4),
            Some((0, ':', "smi".to_owned()))
        );
        assert_eq!(completion_token("plain", 5), None, "no sigil, no popup");
        assert_eq!(
            completion_token("a @b c", 6),
            None,
            "the query never crosses whitespace"
        );
    }

    fn sroom(id: &str, kind: char, unread: u32) -> Room {
        Room {
            id: id.to_owned(),
            name: id.to_owned(),
            kind,
            unread,
            lm: None,
            preview: None,
            topic: None,
            mentions: 0,
        }
    }

    #[test]
    fn side_entries_group_the_original_sections() {
        let mut hot = sroom("hot", 'c', 2);
        hot.mentions = 1;
        let rooms = vec![
            sroom("d1", 'd', 0),
            hot,
            sroom("p1", 'p', 0),
            sroom("g1", 'c', 0),
            sroom("quiet-dm", 'd', 0),
        ];
        assert_eq!(
            side_entries(&rooms),
            vec![
                SideEntry::Section("UNREAD"),
                SideEntry::Room(1),
                SideEntry::Section("CHANNELS"),
                SideEntry::Room(3),
                SideEntry::Section("PRIVATE GROUPS"),
                SideEntry::Room(2),
                SideEntry::Section("DIRECT MESSAGES"),
                SideEntry::Room(0),
                SideEntry::Room(4),
            ],
            "unread leads; sections regroup by kind in source order"
        );
        assert!(
            side_entries(&[sroom("g1", 'c', 0)]).len() == 2,
            "empty sections drop out"
        );
    }
}
