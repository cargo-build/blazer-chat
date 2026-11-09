//! The chat view entity: room list, message pane, composer. State lives in
//! the pure [`ChatState`]; events fold in through the compio pump, clicks
//! push commands back.

use gpui::{
    Anchor, AnyElement, ClipboardItem, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla,
    MouseButton, Pixels, Role, ScrollHandle, SharedString, Window, anchored, deferred, div, hsla,
    img, prelude::*, px, rems, rgb, uniform_list, white,
};
use std::sync::{Arc, Mutex};

use async_channel::Sender;

use super::markdown::{self, Span};
use super::{
    ZoomIn, ZoomOut, ZoomReset,
    input::{MUTED, PANEL, Suggest, TextInput, TextInputEvent},
};
use crate::app::{ChatCommand, ChatEvent, ChatService, ChatState, UiSnapshot};
use crate::rc::{Msg, Room};

const BG: u32 = 0x11151c;
const BORDER: u32 = 0x394354;
const TEXT: u32 = 0xe8eaed;
const ACCENT: u32 = 0x4f8cff;
/// One step above the pane background: the hovered message row.
const ROW_HOVER: u32 = 0x1a2230;

/// The one window's root view.
pub(crate) struct ChatRoot {
    state: ChatState,
    commands: Sender<ChatCommand>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
    pub(crate) composer: Entity<TextInput>,
    /// Room matches for the current search query: id + display name,
    /// rendered above the message hits (m12 F3 room jump).
    search_rooms: Vec<(String, String)>,
    focus: FocusHandle,
    /// Render views of the active room's messages, rebuilt on events.
    views: Vec<MessageView>,
    /// Local search box (the pane shows results while a query is set).
    search: Entity<TextInput>,
    /// `(room_id, msg_id)` while the composer edits an existing message.
    editing: Option<(String, String)>,
    thread_scroll: ScrollHandle,
    results_scroll: ScrollHandle,
    /// The pane's rows: messages interleaved with day separators,
    /// rendered as plain scrollable children.
    rows: Vec<PaneRow>,
    /// The pane's scroll; pins to the newest message on room switches.
    scroll: ScrollHandle,
    /// Picked file paths from the attach dialog's worker thread.
    pick_tx: async_channel::Sender<Option<std::path::PathBuf>>,
    /// This entity's own handle for row-closure callbacks.
    handle: Entity<ChatRoot>,
    /// The row whose labeled action menu is open `(room_id, msg_id)`.
    open_menu: Option<(String, String)>,
    /// The room whose right-click context menu is open (m12 F5).
    room_menu: Option<String>,
    /// The room-info panel toggle (header ℹ️, m12 F7).
    show_info: bool,
    /// The username whose user card is open (m12 F8).
    user_card: Option<String>,
    /// The emoji picker popover is open (m12 F9).
    emoji_picker_open: bool,
    /// The top-bar avatar menu is open.
    user_menu_open: bool,
    /// An older-slice fetch is in flight (one at a time per pane).
    loading_older: bool,
    /// Parsed markdown per (message id, text): rebuilds reuse it instead
    /// of re-parsing every message on every event.
    parsed: std::collections::HashMap<(String, String), std::sync::Arc<Vec<Vec<markdown::Span>>>>,
    /// The state versions the views were built from (skip no-op
    /// rebuilds).
    views_version: u64,
    views_emoji_version: u64,
    views_avatars_version: u64,
    /// One older-slice fetch per visit to the pane's top edge.
    paged_since_top: bool,
    /// Room whose messages the scroll handle last re-pinned.
    pinned_room: Option<String>,
}

/// One precomputed message row of the active room: the pane renders
/// these without re-parsing markdown per frame.
struct MessageView {
    id: String,
    system: bool,
    room_id: String,
    ts: String,
    sender: String,
    /// Raw text — the edit action puts it back into the composer.
    text: String,
    lines: std::sync::Arc<Vec<Vec<Span>>>,
    reactions: Vec<(String, crate::rc::Reaction)>,
    /// `(name, url, size)` when the message carries an uploaded file.
    file: Option<(String, String, u64)>,
    /// Cached preview path for image attachments.
    image_path: Option<String>,
    /// Cached avatar path for the sender; initials render as fallback.
    avatar: Option<String>,
    /// `(reply count, formatted last reply)` when this message starts a
    /// thread.
    thread: Option<(u32, String)>,
    /// Starred by the logged-in user (menu shows "Remove Star").
    starred: bool,
    /// UTC date (YYYY-MM-DD); the pane inserts day separators between
    /// different dates.
    day: String,
    /// Raw server timestamp for the grouping window.
    ts_raw: String,
}

impl ChatRoot {
    pub(crate) fn new(service: ChatService, cx: &mut Context<Self>) -> Self {
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
                            let _ = chat.commands.try_send(ChatCommand::UploadFile {
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

        let handle = cx.entity();
        let composer = cx.new(|cx| TextInput::new("Message…", cx));
        let search_rooms = Vec::new();
        cx.subscribe(&composer, |chat, _, event, cx| match event {
            TextInputEvent::Submitted => chat.send_message(cx),
            TextInputEvent::Cancelled => chat.cancel_editing(cx),
            TextInputEvent::ImagePasted { bytes, ext } => {
                chat.upload_clipboard_image(bytes, ext, cx);
            }
            TextInputEvent::Changed { text, caret } => {
                chat.update_suggestions(text, *caret, cx);
            }
            TextInputEvent::SuggestMoved => cx.notify(),
        })
        .detach();
        let search = cx.new(|cx| TextInput::new("Search…", cx));
        cx.subscribe(&search, |chat, _, event, cx| match event {
            TextInputEvent::Submitted => chat.run_search(cx),
            TextInputEvent::Cancelled
            | TextInputEvent::ImagePasted { .. }
            | TextInputEvent::Changed { .. }
            | TextInputEvent::SuggestMoved => {}
        })
        .detach();
        Self {
            handle,
            state: ChatState::default(),
            commands,
            snapshot,
            composer,
            search_rooms,
            focus: cx.focus_handle(),
            views: Vec::new(),
            search,
            editing: None,
            thread_scroll: ScrollHandle::new(),
            results_scroll: ScrollHandle::new(),
            rows: Vec::new(),
            scroll: ScrollHandle::new(),
            pinned_room: None,
            pick_tx,
            open_menu: None,
            room_menu: None,
            show_info: false,
            user_card: None,
            emoji_picker_open: false,
            user_menu_open: false,
            loading_older: false,
            parsed: Default::default(),
            views_version: u64::MAX,
            views_emoji_version: u64::MAX,
            views_avatars_version: u64::MAX,
            paged_since_top: false,
        }
    }

    fn apply_event(&mut self, event: ChatEvent, cx: &mut Context<Self>) {
        if matches!(event, ChatEvent::OlderMessages { .. }) {
            self.loading_older = false;
        }
        // Prepending shifts rows: pin the previously-top message.
        let anchor = match &event {
            ChatEvent::OlderMessages { messages, .. } if !messages.is_empty() => self
                .state
                .active_messages()
                .first()
                .map(|message| message.id.clone()),
            _ => None,
        };
        let follow_up = self.state.apply(event);
        if let Some(command) = follow_up {
            let _ = self.commands.try_send(command);
        }
        // Render data derives in the entity, never in render: the pane only
        // maps the precomputed spans to elements.
        self.rebuild_views();
        if let Some(anchor_id) = anchor
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| {
                    matches!(row, PaneRow::Message { view_index } if self.views[*view_index].id == anchor_id)
                })
        {
            self.scroll.scroll_to_item(index);
        }
        self.mirror();
        cx.notify();
    }

    /// Re-parses the active room's messages into render views (markdown
    /// spans with emoji paths resolved).
    fn rebuild_views(&mut self) {
        let switched = self.pinned_room.as_deref() != self.state.active_room.as_deref();
        // Nothing visible changed (badge/event churn): keep the views.
        if !switched
            && self.views_version == self.state.messages_version
            && self.views_emoji_version == self.state.emoji_version
            && self.views_avatars_version == self.state.avatars_version
        {
            return;
        }
        let at_bottom = self.scroll.offset().y >= -self.scroll.max_offset().y;
        let messages = self.state.active_messages().to_vec();
        self.views = messages
            .iter()
            .map(|message| self.message_view(message))
            .collect();
        // Interleave day separators; the pane renders them between
        // messages. The first-unread boundary draws a divider before
        // its message (m12 F1).
        let boundary = self
            .state
            .first_unread
            .get(self.state.active_room.as_deref().unwrap_or_default())
            .cloned();
        self.rows = Vec::with_capacity(self.views.len() + 8);
        let mut previous_day: Option<String> = None;
        for (view_index, view) in self.views.iter().enumerate() {
            if previous_day.as_deref() != Some(view.day.as_str()) {
                self.rows.push(PaneRow::Separator {
                    day_ts: view.ts_raw.clone(),
                });
                previous_day = Some(view.day.clone());
            }
            if boundary.as_deref() == Some(view.id.as_str()) {
                self.rows.push(PaneRow::UnreadMarker { view_index });
            }
            self.rows.push(PaneRow::Message { view_index });
        }
        if switched || at_bottom {
            // Jump to the unread divider when the room opened with
            // unreads; otherwise land at the newest message.
            match self
                .rows
                .iter()
                .position(|row| matches!(row, PaneRow::UnreadMarker { .. }))
            {
                Some(index) if switched => self.scroll.scroll_to_item(index),
                _ => self.scroll.scroll_to_bottom(),
            }
            self.pinned_room = self.state.active_room.clone();
        }
        self.views_version = self.state.messages_version;
        self.views_emoji_version = self.state.emoji_version;
        self.views_avatars_version = self.state.avatars_version;
    }

    /// Builds the render view for one message: markdown parsed through
    /// the shared cache, emoji/avatars resolved, attachments carried.
    fn message_view(&mut self, message: &Msg) -> MessageView {
        MessageView {
            id: message.id.clone(),
            system: message.system,
            room_id: message.room_id.clone(),
            starred: message.starred_by_me,
            ts: pretty_time(&message.ts),
            sender: message.sender.clone(),
            text: message.text.clone(),
            lines: {
                let key = (message.id.clone(), message.text.clone());
                match self.parsed.get(&key) {
                    Some(lines) => lines.clone(),
                    None => {
                        if self.parsed.len() > 4000 {
                            self.parsed.clear();
                        }
                        let parsed = std::sync::Arc::new(
                            markdown::split_fences(&message.text)
                                .iter()
                                .flat_map(|segment| match segment {
                                    markdown::Segment::Prose(line) => {
                                        vec![markdown::parse(line, &|name| {
                                            self.state.emoji.get(name).cloned()
                                        })]
                                    }
                                    markdown::Segment::Fence { body } => {
                                        vec![vec![markdown::Span::CodeBlock { body: body.clone() }]]
                                    }
                                })
                                .collect::<Vec<_>>(),
                        );
                        self.parsed.insert(key, parsed.clone());
                        parsed
                    }
                }
            },
            day: local_day(&message.ts),
            ts_raw: message.ts.clone(),
            reactions: message
                .reactions
                .iter()
                .map(|(name, reaction)| (name.clone(), reaction.clone()))
                .collect(),
            file: message
                .file
                .as_ref()
                .map(|file| (file.name.clone(), file.url.clone(), file.size)),
            image_path: message
                .file
                .as_ref()
                .and_then(|file| file.image_path.clone()),
            avatar: self.state.avatars.get(&message.sender).cloned(),
            thread: message
                .tcount
                .map(|count| (count, pretty_time(message.tlm.as_deref().unwrap_or("")))),
        }
    }

    /// Mirrors the entity into the control-socket snapshot. The UI thread
    /// holds the lock only for the store, never across a frame.
    fn mirror(&self) {
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(UiSnapshot::from_state(&self.state));
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
            let _ = self.commands.try_send(ChatCommand::SaveDraft {
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
        let _ = self.commands.try_send(ChatCommand::OpenRoom {
            room_id: room_id.clone(),
        });
        cx.notify();
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
            let _ = self.commands.try_send(ChatCommand::Search(String::new()));
        }
        cx.notify();
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
                let _ = self.commands.try_send(ChatCommand::UploadFile {
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
        let _ = self.commands.try_send(ChatCommand::SaveDraft {
            room_id: room_id.clone(),
            text: String::new(),
        });
        let _ = self.commands.try_send(command);
        cx.notify();
    }
}

impl ChatRoot {
    /// Toggles a 👍 reaction on one message (chat.react toggles on
    /// repeat).
    /// Rocket.Chat reactions are emoji NAMES on the wire (`+1`, `tada`,
    /// …); the unicode glyph is display-only.
    /// Opens or closes one row's labeled action menu.
    fn toggle_menu(&mut self, room_id: String, msg_id: String, cx: &mut Context<Self>) {
        self.open_menu = match self.open_menu.as_ref() {
            Some(open) if open.0 == room_id && open.1 == msg_id => None,
            _ => Some((room_id, msg_id)),
        };
        cx.notify();
    }

    /// Stars or unstars a message through the server; the fold applies
    /// the flip locally.
    fn toggle_star(&mut self, msg_id: String, starred: bool, cx: &mut Context<Self>) {
        let _ = self.commands.try_send(ChatCommand::StarMessage {
            msg_id,
            star: !starred,
        });
        self.open_menu = None;
        cx.notify();
    }

    /// The original client's permalink:
    /// `{origin}/{channel|group}/{room name}/msg/{id}` — channels and
    /// groups only (DM links address the peer, which rooms don't carry).
    fn permalink(&self, room_id: &str, msg_id: &str) -> Option<String> {
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
        let _ = self.commands.try_send(ChatCommand::Logout);
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

    fn toggle_reaction(
        &mut self,
        room_id: &str,
        msg_id: &str,
        emoji: &str,
        cx: &mut Context<Self>,
    ) {
        let _ = self.commands.try_send(ChatCommand::React {
            room_id: room_id.to_owned(),
            msg_id: msg_id.to_owned(),
            emoji: emoji.to_owned(),
        });
        cx.notify();
    }

    /// Puts a message back into the composer for editing; the next
    /// submit sends `chat.update` instead of a new message.
    fn begin_edit(
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
        let _ = self.commands.try_send(ChatCommand::Search(query.clone()));
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

    fn delete_message(&mut self, room_id: &str, msg_id: &str, cx: &mut Context<Self>) {
        let _ = self.commands.try_send(ChatCommand::DeleteMessage {
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
        .filter(|room| room.name.to_lowercase().contains(&query))
        .map(|room| (room.id.clone(), room.name.clone()))
        .collect()
}

impl Focusable for ChatRoot {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ChatRoot {
    /// The accent divider marking the first unread message (m12 F1).
    fn unread_marker(&self, _view_index: usize) -> AnyElement {
        let label = "Unread messages";
        div()
            .id("unread-divider")
            .aria_label(label)
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .child(div().flex_1().h(px(1.0)).bg(rgb(ACCENT)))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(rgb(ACCENT))
                    .child(label),
            )
            .child(div().flex_1().h(px(1.0)).bg(rgb(ACCENT)))
            .into_any_element()
    }

    /// A centered day divider between messages of different dates.
    fn day_separator(&self, day_ts: &str) -> AnyElement {
        div()
            .id(SharedString::from(format!("day-{day_ts}")))
            .role(Role::Heading)
            .aria_label(format!("Messages from {}", pretty_day(day_ts)))
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(SharedString::from(pretty_day(day_ts))),
            )
            .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
            .into_any_element()
    }

    /// One message row; consecutive same-sender rows within five
    /// minutes group (avatar and header hidden), like the original.
    fn message_row(&self, index: usize) -> AnyElement {
        self.message_row_view(&self.views[index])
    }

    /// The full row builder for any message view (main pane and
    /// threads render the same way).
    fn message_row_view(&self, view: &MessageView) -> AnyElement {
        let index = self
            .views
            .iter()
            .position(|known| known.id == view.id)
            .unwrap_or(0);
        let grouped = index > 0 && !view.system && {
            let previous = &self.views[index - 1];
            !previous.system
                && previous.sender == view.sender
                && previous.day == view.day
                && within_five_minutes(&previous.ts_raw, &view.ts_raw)
        };
        let room_id = view.room_id.clone();
        let msg_id = view.id.clone();
        let edit_text = view.text.clone();
        let editing_this = matches!(
            self.editing.as_ref(),
            Some((room, msg)) if *room == room_id && *msg == msg_id
        );
        let emoji = &self.state.emoji;
        let me = self.state.username.clone();
        let badges = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .children(view.reactions.iter().map(|(name, reaction)| {
                let mine = me.as_deref().is_some_and(|me| reaction.has(me));
                let count = reaction.count();
                let names = reaction.usernames.join(", ");
                let pill = div()
                    .id(SharedString::from(format!("react-{msg_id}-{name}")))
                    .role(Role::Button)
                    .aria_label(format!(
                        "React {name} — {count} so far: {names}",
                        count = reaction.count()
                    ))
                    .px_1p5()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(if mine { ACCENT } else { BORDER }))
                    .bg(rgb(if mine { ROW_HOVER } else { PANEL }))
                    .text_xs()
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .tooltip(move |_, app| {
                        app.new(|_| ReactionTooltip {
                            text: SharedString::from(names.clone()),
                        })
                        .into()
                    })
                    .on_click({
                        let this = self.handle.clone();
                        let room_id = room_id.clone();
                        let msg_id = msg_id.clone();
                        let name = name.clone();
                        move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.toggle_reaction(&room_id, &msg_id, &name, cx)
                            });
                        }
                    });
                match emoji.get(name) {
                    Some(path) => pill.flex().gap_1().items_center().child(
                        img(std::path::PathBuf::from(path))
                            .h(rems(0.875))
                            .w(rems(0.875)),
                    ),
                    None => pill,
                }
                .child(SharedString::from(format!(
                    "{} ×{count}",
                    display_emoji(name)
                )))
            }));
        let thread_pill = view.thread.as_ref().map(|(count, last)| {
            let this = self.handle.clone();
            let tmid = msg_id.clone();
            div()
                .id(SharedString::from(format!("thread-{msg_id}")))
                .role(Role::Button)
                .aria_label(format!(
                    "Open thread — {count} {}",
                    if count == &1 { "reply" } else { "replies" }
                ))
                .mt_0p5()
                .px_2()
                .py_0p5()
                .rounded_md()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .text_xs()
                .text_color(rgb(MUTED))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(ROW_HOVER)))
                .on_click(move |_, _, cx| {
                    this.update(cx, |chat, cx| {
                        let _ = chat
                            .commands
                            .try_send(ChatCommand::OpenThread { tmid: tmid.clone() });
                        cx.notify();
                    });
                })
                .child(SharedString::from(format!(
                    "View thread · {count} {} · {last}",
                    if *count == 1 { "reply" } else { "replies" }
                )))
        });
        if view.system {
            // Service notice (join/leave): compact muted line, not a
            // chat row — no avatar, no toolbar, no grouping (m12 F2).
            return div()
                .id(SharedString::from(format!("sys-{msg_id}")))
                .role(Role::ListItem)
                .aria_label(SharedString::from(view.text.clone()))
                .flex()
                .justify_center()
                .py_0p5()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(view.text.clone())),
                )
                .into_any_element();
        }
        // The user card opens from the avatar, the sender name or a
        // mention pill (m12 D3/F8); unknown users fetch their card
        // data on the first open.
        let card_open = self.user_card.as_deref() == Some(view.sender.as_str());
        let lead = if grouped {
            div().flex_none().w(px(32.0)).into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("card-{msg_id}")))
                .role(Role::Button)
                .aria_label(format!("Open user card for {}", view.sender))
                .flex_none()
                .cursor_pointer()
                .on_click(card_opener(self.handle.clone(), view.sender.clone()))
                .child(match view.avatar.as_deref() {
                    Some(path) => img(std::path::PathBuf::from(path))
                        .flex_none()
                        .size(rems(2.0))
                        .rounded_full()
                        .into_any_element(),
                    None => avatar(&view.sender).into_any_element(),
                })
                .into_any_element()
        };
        div()
            .id(SharedString::from(format!("msg-{msg_id}")))
            .role(Role::ListItem)
            .aria_label(format!(
                "{} at {}, {}",
                view.sender,
                view.ts,
                if view.text.is_empty() {
                    "attachment".to_owned()
                } else {
                    view.text.replace('\n', " ")
                }
            ))
            .group("msg")
            .flex()
            .gap_2()
            .items_start()
            .px_2()
            .py_1()
            .rounded_md()
            .hover(|style| style.bg(rgb(ROW_HOVER)))
            .child(lead)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .when(!grouped, |column| {
                        column.child(
                            div()
                                .flex()
                                .items_baseline()
                                .gap_2()
                                .child(
                                    div()
                                        .id(SharedString::from(format!("cardname-{msg_id}")))
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(TEXT))
                                        .cursor_pointer()
                                        .hover(|style| style.text_color(rgb(ACCENT)))
                                        .on_click(card_opener(
                                            self.handle.clone(),
                                            view.sender.clone(),
                                        ))
                                        .child(SharedString::from(view.sender.clone())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(SharedString::from(view.ts.clone())),
                                ),
                        )
                    })
                    .child(message_element(
                        view,
                        self.state.username.as_deref(),
                        &self.handle,
                    ))
                    .children(thread_pill)
                    .children(view.file.as_ref().map(|(name, url, size)| {
                        attachment_card(&msg_id, name, url, *size, view.image_path.as_deref())
                    }))
                    .child(badges),
            )
            .when(card_open, |row| {
                let info = self.state.user_info.get(&view.sender);
                let name = info
                    .and_then(|info| info.name.clone())
                    .unwrap_or_else(|| view.sender.clone());
                let status = info
                    .and_then(|info| info.status.clone())
                    .unwrap_or_else(|| "…".to_owned());
                let is_me = self.state.username.as_deref() == Some(view.sender.as_str());
                let this = self.handle.clone();
                let sender = view.sender.clone();
                row.child(deferred(
                    anchored().anchor(Anchor::TopLeft).child(
                        div()
                            .occlude()
                            .flex()
                            .flex_col()
                            .min_w(px(170.0))
                            .p_2()
                            .gap_1()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded_md()
                            .shadow_lg()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(avatar(&view.sender))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(SharedString::from(name)),
                                            )
                                            .child(div().text_xs().text_color(rgb(MUTED)).child(
                                                SharedString::from(format!(
                                                    "@{} · {status}",
                                                    view.sender
                                                )),
                                            )),
                                    ),
                            )
                            .when(!is_me, |card| {
                                card.child(
                                    div()
                                        .id("card-dm")
                                        .role(Role::Button)
                                        .px_2()
                                        .py_1()
                                        .rounded_xs()
                                        .text_xs()
                                        .text_color(rgb(TEXT))
                                        .cursor_pointer()
                                        .hover(|style| style.bg(rgb(ROW_HOVER)))
                                        .child("✉ Send DM")
                                        .on_click({
                                            let this = this.clone();
                                            let sender = sender.clone();
                                            move |_, _, cx| {
                                                this.update(cx, |chat, cx| {
                                                    chat.user_card = None;
                                                    let _ = chat.commands.try_send(
                                                        ChatCommand::OpenDm {
                                                            username: sender.clone(),
                                                        },
                                                    );
                                                    cx.notify();
                                                });
                                            }
                                        }),
                                )
                            })
                            .child(
                                div()
                                    .id("card-close")
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(rgb(TEXT)))
                                    .child("✕ close")
                                    .on_click(move |_, _, cx| {
                                        this.update(cx, |chat, cx| {
                                            chat.user_card = None;
                                            cx.notify();
                                        });
                                    }),
                            ),
                    ),
                ))
            })
            .child(action_strip(ActionContext {
                this: self.handle.clone(),
                link_ready: self.permalink(&room_id, "").is_some(),
                menu_open: self.open_menu.as_ref() == Some(&(room_id.clone(), msg_id.clone())),
                room_id,
                msg_id,
                edit_text,
                editing_this,
                starred: view.starred,
            }))
            .into_any_element()
    }
}

/// One sidebar row: a section header or a room (m12 F4).
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
fn section_row(label: &'static str) -> AnyElement {
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
                .text_color(rgb(MUTED))
                .child(label),
        )
        .into_any_element()
}

fn panel(
    title: &str,
    subtitle: Option<&str>,
    content: impl IntoElement,
    width: Option<Pixels>,
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
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
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
                        .text_color(rgb(TEXT))
                        .child(title.to_owned()),
                )
                .children(subtitle.map(|subtitle| {
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .truncate()
                        .child(SharedString::from(subtitle.to_owned()))
                })),
        )
        .child(content)
}

impl gpui::Render for ChatRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let who = self
            .state
            .username
            .clone()
            .unwrap_or_else(|| "not logged in".into());
        let error = div().when_some(self.state.error.clone(), |row, message| {
            row.child(
                div()
                    .text_sm()
                    .text_color(gpui::red())
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

            range
                .map(|index| match entries[index] {
                    SideEntry::Section(label) => section_row(label),
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
                                    .text_color(rgb(TEXT))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(ROW_HOVER)))
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
                                        .bg(rgb(PANEL))
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .rounded_md()
                                        .shadow_lg()
                                        .child(item("🚩 Mark Unread", "r-unread").on_click({
                                            let this = this.clone();
                                            let room_id = room_id.clone();
                                            move |_, _, cx| {
                                                this.update(cx, |chat, cx| {
                                                    chat.room_menu = None;
                                                    let _ = chat.commands.try_send(
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
                                                    let _ = chat.commands.try_send(
                                                        ChatCommand::HideRoom {
                                                            room_id: room_id.clone(),
                                                        },
                                                    );
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
                            .when(selected, |row| row.bg(rgb(ACCENT)))
                            .when(selected, |row| row.aria_selected(true))
                            .hover(|style| style.bg(rgb(BORDER)))
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
                                                    .text_color(rgb(MUTED))
                                                    .child(SharedString::from(pretty_time(ts)))
                                            }))
                                            .when(unread > 0, |line| {
                                                line.child(
                                                    div()
                                                        .flex_none()
                                                        .px_1()
                                                        .rounded_xs()
                                                        .bg(rgb(ACCENT))
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
                                                        .bg(gpui::red())
                                                        .text_xs()
                                                        .text_color(white())
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
                                            .text_color(rgb(MUTED))
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

        // Plain scrollable rows — proven to paint on both stacks. The
        // virtualizing list() reproduces the empty-pane symptom on the
        // migrated stack as well; that investigation continues with a
        // fixed-height probe on a branch.
        let mut rows: Vec<AnyElement> = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            match row {
                PaneRow::Separator { day_ts } => rows.push(self.day_separator(day_ts)),
                PaneRow::UnreadMarker { view_index } => rows.push(self.unread_marker(*view_index)),
                PaneRow::Message { view_index } => rows.push(self.message_row(*view_index)),
            }
        }
        let messages = div()
            .id("messages")
            .flex()
            .flex_col()
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .children(rows);

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
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(ACCENT))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(ROW_HOVER)))
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
                                        .text_color(rgb(TEXT))
                                        .child(SharedString::from(note.title.clone())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
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
                                .text_color(rgb(MUTED))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
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
                        .border_color(rgb(BORDER))
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
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(ACCENT)))
                        .child("🔍")
                        .on_click(move |_, _, cx| {
                            search_this.update(cx, |chat, cx| chat.run_search(cx));
                        }),
                )
        };
        // Scroll-to-top paging: one older slice per visit to the top
        // edge (edge-triggered — a pinned top must not re-fire every
        // frame), stopping at the pane cap or when history runs out.
        let at_top = self.scroll.offset().y >= px(-1.0);
        if !at_top {
            self.paged_since_top = false;
        }
        let filled = self.state.active_messages().len() >= 400;
        if at_top
            && !self.paged_since_top
            && !self.loading_older
            && !filled
            && let Some(room) = self.state.active_room.clone()
            && !self.state.older_exhausted.contains(&room)
        {
            self.paged_since_top = true;
            self.loading_older = true;
            let _ = self
                .commands
                .try_send(ChatCommand::FetchOlder { room_id: room });
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
            let rows = thread_messages
                .iter()
                .map(|message| {
                    let view = self.message_view(message);
                    self.message_row_view(&view)
                })
                .collect::<Vec<_>>();
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
                        .bg(rgb(PANEL))
                        .text_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(ACCENT)))
                        .child("← thread")
                        .on_click(move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                let _ = chat.commands.try_send(ChatCommand::CloseThread);
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
                        .hover(|style| style.bg(rgb(ACCENT)))
                        .on_click(move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.open_room(room_id.clone(), cx);
                                chat.search.update(cx, |box_, cx| {
                                    box_.set_content("", cx);
                                });
                                let _ = chat.commands.try_send(ChatCommand::Search(String::new()));
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(TEXT))
                                .child(SharedString::from(format!("# {name}"))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(rgb(MUTED))
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
                        .hover(|style| style.bg(rgb(BORDER)))
                        .on_click(move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.open_room(room_id.clone(), cx);
                                chat.search.update(cx, |box_, cx| {
                                    box_.set_content("", cx);
                                });
                                // Clearing the box alone leaves the query
                                // in state: the results pane would keep
                                // covering the room that was just opened.
                                let _ = chat.commands.try_send(ChatCommand::Search(String::new()));
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(ACCENT))
                                .child(SharedString::from(hit.room.clone())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(MUTED))
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
            let mut rows: Vec<gpui::AnyElement> = pinned
                .iter()
                .map(|message| {
                    let view = self.message_view(message);
                    self.message_row_view(&view)
                })
                .collect();
            if rows.is_empty() {
                rows.push(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
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
                        .hover(|style| style.bg(rgb(BORDER)))
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
                        .hover(|style| style.bg(rgb(BORDER)))
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
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
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
                                .text_color(rgb(MUTED))
                                .child(SharedString::from(kind_label)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(TEXT))
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
            None => avatar(&me).into_any_element(),
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
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(ROW_HOVER)))
                    .child(div().size(px(10.0)).rounded_full().bg(rgb(dot)))
                    .child(label)
                    .on_click(move |_, _, cx| {
                        taken.update(cx, |chat, cx| {
                            let _ = chat.commands.try_send(ChatCommand::SetStatus {
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
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
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
                                    None => {
                                        div().size(rems(2.5)).child(avatar(&me)).into_any_element()
                                    }
                                })
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(rgb(TEXT))
                                                .child(SharedString::from(me.clone())),
                                        )
                                        .child(div().text_xs().text_color(rgb(MUTED)).child(
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
                                .text_color(rgb(MUTED))
                                .child("Status"),
                        )
                        .child(status_item(0x2f9e44, "online", "online", this_bar.clone()))
                        .child(status_item(0xe8b30f, "away", "away", this_bar.clone()))
                        .child(status_item(0xe03131, "busy", "busy", this_bar.clone()))
                        .child(
                            div()
                                .px_2()
                                .pt_1()
                                .pb_0p5()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(MUTED))
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
                                .text_color(rgb(0xe03131))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(ROW_HOVER)))
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
                    .bg(rgb(PANEL))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(TEXT))
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
                                .hover(|style| style.bg(rgb(BORDER)))
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
                                                    let _ = chat.commands.try_send(
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
                                .hover(|style| style.bg(rgb(BORDER)))
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
                            .text_color(rgb(MUTED))
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
                            chip.bg(rgb(ACCENT)).text_color(white())
                        })
                        .when(index != suggest.selected, |chip| {
                            chip.text_color(rgb(TEXT))
                                .hover(|style| style.bg(rgb(ROW_HOVER)))
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
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
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
                .text_color(rgb(MUTED))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(BORDER)))
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
            .text_color(rgb(MUTED))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(BORDER)))
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
                    .hover(|style| style.bg(rgb(ROW_HOVER)))
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
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
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
                        .views
                        .iter()
                        .find(|view| view.id == *quote_msg)
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
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .rounded_md()
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(ACCENT))
                                .child("↩"),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(SharedString::from(label)),
                        )
                        .child(
                            div()
                                .id("quote-clear")
                                .flex_none()
                                .px_1()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .cursor_pointer()
                                .hover(|style| style.text_color(rgb(TEXT)))
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
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
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
                    .child(panel("rooms", None, rooms, Some(px(240.0))))
                    .child(panel(
                        &pane_title,
                        self.state
                            .rooms
                            .iter()
                            .find(|room| {
                                Some(room.id.as_str()) == self.state.active_room.as_deref()
                            })
                            .and_then(|room| room.topic.as_deref()),
                        div().flex().flex_col().min_h_0().child(toasts).child(pane),
                        None,
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
                                    .bg(rgb(PANEL))
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .rounded_md()
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(ACCENT)))
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
                                    .bg(rgb(PANEL))
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(ACCENT)))
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

/// One message body: the text split into lines (newlines preserved), each
/// line parsed into markdown spans and laid out as a wrapping row so inline
/// emoji images flow with the text.
/// The sender's initial on a per-user color disc (Rocket.Chat colors
/// usernames too).
fn avatar(sender: &str) -> gpui::Div {
    let initial = sender
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .next()
        .unwrap_or('?');
    div()
        .flex_none()
        .size(rems(2.0))
        .rounded_full()
        .bg(user_color(sender))
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(white())
        .child(SharedString::from(initial.to_string()))
}

/// Deterministic per-sender accent from the name itself.
fn user_color(sender: &str) -> Hsla {
    let hash = sender.bytes().fold(5381u32, |acc, byte| {
        acc.wrapping_mul(33).wrapping_add(u32::from(byte))
    });
    hsla((hash % 360) as f32, 0.55, 0.62, 1.0)
}

/// The wire's emoji names (`+1`) render as their unicode glyphs
/// (Rocket.Chat's canonical names); unknown names stay as-is.
fn display_emoji(name: &str) -> String {
    match name {
        "+1" => "👍".to_owned(),
        "-1" => "👎".to_owned(),
        "smile" => "😄".to_owned(),
        "tada" => "🎉".to_owned(),
        "heart" => "❤️".to_owned(),
        "eyes" => "👀".to_owned(),
        other => other.to_owned(),
    }
}

/// Consecutive-grouping window: same sender within five minutes.
fn within_five_minutes(prev: &str, next: &str) -> bool {
    match (
        prev.parse::<jiff::Timestamp>(),
        next.parse::<jiff::Timestamp>(),
    ) {
        (Ok(a), Ok(b)) => b < a + std::time::Duration::from_secs(5 * 60),
        _ => false,
    }
}

/// The human day for a separator: `November 7`.
/// The message's local calendar day (`YYYY-MM-DD`): grouping and the
/// label use the same zoned value, so west-of-UTC users see their own
/// dates instead of a UTC day over local timestamps.
fn local_day(ts: &str) -> String {
    ts.parse::<jiff::Timestamp>()
        .map(|stamp| {
            stamp
                .to_zoned(jiff::tz::TimeZone::system())
                .date()
                .to_string()
        })
        .unwrap_or_else(|_| ts.split('T').next().unwrap_or(ts).to_owned())
}

fn pretty_day(ts: &str) -> String {
    match ts.parse::<jiff::Timestamp>() {
        Ok(stamp) => stamp
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%B %-d")
            .to_string(),
        Err(_) => ts.split('T').next().unwrap_or(ts).to_owned(),
    }
}

/// `2026-11-07T20:32:00.514Z` → `8:32 PM` in the machine's local zone;
/// unparsable stamps degrade to the raw time part.
fn pretty_time(ts: &str) -> String {
    pretty_time_in(ts, &jiff::tz::TimeZone::system())
}

/// The same formatting against an explicit zone (tests pin offsets).
fn pretty_time_in(ts: &str, tz: &jiff::tz::TimeZone) -> String {
    let Ok(stamp) = ts.parse::<jiff::Timestamp>() else {
        return raw_time(ts);
    };
    let local = stamp.to_zoned(tz.clone());
    let (hour, suffix) = match local.hour() {
        0 => (12, "AM"),
        hour @ 1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        hour => (hour - 12, "PM"),
    };
    format!("{hour}:{:02} {suffix}", local.minute())
}

/// The unparsable-stamp fallback: the raw time component.
fn raw_time(ts: &str) -> String {
    ts.split_once('T')
        .map(|(_, time)| time.to_owned())
        .unwrap_or_else(|| ts.to_owned())
}

/// One attachment as a card: type tile, name link, size — with the
/// fetched image inline when cached (the original client's look).
fn attachment_card(
    msg_id: &str,
    name: &str,
    url: &str,
    size: u64,
    image_path: Option<&str>,
) -> gpui::Div {
    let extension = name
        .rsplit('.')
        .next()
        .filter(|_| name.contains('.'))
        .unwrap_or("file")
        .to_uppercase();
    let url = url.to_owned();
    div()
        .mt_0p5()
        .max_w(rems(26.25))
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_md()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .size(rems(2.5))
                        .rounded_sm()
                        .bg(rgb(BORDER))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(TEXT))
                        .child(SharedString::from(extension)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            div()
                                .id(SharedString::from(format!("file-{msg_id}")))
                                .text_sm()
                                .text_color(rgb(ACCENT))
                                .cursor_pointer()
                                .hover(|style| style.underline())
                                .child(SharedString::from(name.to_owned()))
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(SharedString::from(human_bytes(size))),
                        ),
                ),
        )
        .children(image_path.map(|path| {
            img(std::path::PathBuf::from(path))
                .max_w(rems(25.0))
                .rounded_sm()
        }))
}

fn message_element(view: &MessageView, me: Option<&str>, this: &Entity<ChatRoot>) -> gpui::Div {
    let lines: Vec<gpui::Div> = view
        .lines
        .iter()
        .enumerate()
        .map(|(line_index, spans)| {
            let mut row = div().flex().flex_wrap().items_center();
            for (span_index, span) in spans.iter().enumerate() {
                let id = format!("{}-{line_index}-{span_index}", view.id);
                row = row.child(span_element(span.clone(), me, this, &id));
            }
            row
        })
        .collect();
    div().flex().flex_col().children(lines)
}

/// One span as an inline element; links open in the browser,
/// mentions highlight the own name and open the user's card (m12
/// D3). `id` makes the clickable spans stateful and unique.
fn span_element(
    span: Span,
    me: Option<&str>,
    this: &Entity<ChatRoot>,
    id: &str,
) -> gpui::AnyElement {
    match span {
        Span::Text {
            text,
            bold,
            italic,
            code,
            strike,
        } => {
            let mut element = div().text_sm().child(SharedString::from(text));
            if bold {
                element = element.font_weight(FontWeight::BOLD);
            }
            if italic {
                element = element.italic();
            }
            if strike {
                element = element.line_through();
            }
            if code {
                element = element
                    .font_family("monospace")
                    .bg(rgb(0x1c2430))
                    .px_1()
                    .rounded_sm();
            }
            element.into_any_element()
        }
        Span::Link { label, url } => div()
            .id(SharedString::from(format!("lnk-{id}")))
            .text_sm()
            .underline()
            .text_color(rgb(ACCENT))
            .cursor_pointer()
            .hover(|style| style.text_color(white()))
            .on_click({
                let url = url.clone();
                move |_: &gpui::ClickEvent, _: &mut gpui::Window, _: &mut gpui::App| {
                    open_url(&url);
                }
            })
            .child(SharedString::from(label))
            .into_any_element(),
        Span::Mention(name) => {
            let mine = name.strip_prefix('@') == me;
            div()
                .id(SharedString::from(format!("mnt-{id}")))
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .px_1()
                .rounded_xs()
                .cursor_pointer()
                .when(mine, |pill| pill.bg(rgb(ACCENT)).text_color(white()))
                .when(!mine, |pill| pill.text_color(rgb(ACCENT)))
                .hover(|style| style.bg(rgb(ROW_HOVER)))
                .on_click(card_opener(this.clone(), name.clone()))
                .child(SharedString::from(name))
                .into_any_element()
        }
        Span::Emoji { path } => div()
            .child(
                img(std::path::PathBuf::from(&path))
                    .h(rems(1.125))
                    .w(rems(1.125)),
            )
            .into_any_element(),
        Span::CodeBlock { body } => div()
            .font_family("monospace")
            .text_sm()
            .bg(rgb(0x121820))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_sm()
            .px_2()
            .py_1()
            .min_w_0()
            .child(SharedString::from(body.clone()))
            .into_any_element(),
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

/// The click handler opening a user's card — avatars, sender names
/// and mention pills (m12 D3/F8); unknown users fetch their card
/// data on the first open.
fn card_opener(
    this: Entity<ChatRoot>,
    name: String,
) -> impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
        this.update(cx, |chat, cx| {
            chat.user_card = Some(name.clone());
            if !chat.state.user_info.contains_key(&name) {
                let _ = chat.commands.try_send(ChatCommand::UserInfo {
                    username: name.clone(),
                });
            }
            cx.notify();
        });
    }
}

/// Opens a link in the system browser; the spawn is fire-and-forget
/// (m12 D3).
fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// What a message row's action buttons need at click time.
/// One virtualized pane row: a message or a day divider.
enum PaneRow {
    Separator { day_ts: String },
    UnreadMarker { view_index: usize },
    Message { view_index: usize },
}

struct ActionContext {
    this: Entity<ChatRoot>,
    room_id: String,
    msg_id: String,
    edit_text: String,
    editing_this: bool,
    /// This row's labeled menu is open.
    menu_open: bool,
    /// Starred by me (menu label flips).
    starred: bool,
    /// The room supports permalinks (channels/groups; not DMs).
    link_ready: bool,
}

/// The hover-revealed per-message actions: react, edit, delete. The strip
/// is invisible until its row (`group("msg")`) is hovered; the row being
/// edited also shows a cancel button.
fn action_strip(actions: ActionContext) -> gpui::Div {
    let ActionContext {
        this,
        room_id,
        msg_id,
        edit_text,
        editing_this,
        menu_open,
        starred,
        link_ready,
    } = actions;

    let button = |label: &'static str, suffix: &'static str| {
        div()
            .id(SharedString::from(format!("{suffix}-{msg_id}")))
            .px_1()
            .rounded_xs()
            .text_xs()
            .text_color(rgb(MUTED))
            .cursor_pointer()
            .child(label)
    };

    // The original client's quick-reaction set; wire names, glyphs for
    // display.
    let quick = [
        ("👍", "+1"),
        ("👎", "-1"),
        ("😄", "smile"),
        ("🎉", "tada"),
        ("👀", "eyes"),
    ];
    let react = div()
        .flex()
        .gap_1()
        .children(quick.iter().map(|(glyph, name)| {
            let this = this.clone();
            let room_id = room_id.clone();
            let msg_id = msg_id.clone();
            let name = *name;
            button(glyph, "react").on_click(move |_, _, cx| {
                this.update(cx, |chat, cx| {
                    chat.toggle_reaction(&room_id, &msg_id, name, cx)
                });
            })
        }));
    let edit = button("✏", "edit").on_click({
        let this = this.clone();
        let room_id = room_id.clone();
        let msg_id = msg_id.clone();
        let edit_text = edit_text.clone();
        move |_, window, cx| {
            this.update(cx, |chat, cx| {
                chat.begin_edit(
                    room_id.clone(),
                    msg_id.clone(),
                    edit_text.clone(),
                    window,
                    cx,
                );
            });
        }
    });
    let thread = button("🧵", "thread").on_click({
        let this = this.clone();
        let tmid = msg_id.clone();
        move |_, _, cx| {
            this.update(cx, |chat, cx| {
                let _ = chat
                    .commands
                    .try_send(ChatCommand::OpenThread { tmid: tmid.clone() });
                cx.notify();
            });
        }
    });
    let delete = button("🗑", "delete").on_click({
        let this = this.clone();
        let room_id = room_id.clone();
        let msg_id = msg_id.clone();
        move |_, _, cx| {
            this.update(cx, |chat, cx| chat.delete_message(&room_id, &msg_id, cx));
        }
    });

    // The original's labeled action menu (Start a Discussion / Star /
    // Copy link / Copy text / Edit): gpui 0.2.2 has no anchored popover,
    // so the ⋮ toggles an in-flow card in the strip.
    let kebab = button("⋮", "menu").on_click({
        let this = this.clone();
        let room_id = room_id.clone();
        let msg_id = msg_id.clone();
        move |_, _, cx| {
            this.update(cx, |chat, cx| {
                chat.toggle_menu(room_id.clone(), msg_id.clone(), cx)
            });
        }
    });
    // The labeled menu renders in a floating layer anchored to the
    // strip (SwitchAnchor keeps it inside the window) — no layout
    // shift, nothing painted while closed.
    let menu = menu_open.then(|| {
        let star_label = if starred {
            "☆ Remove Star"
        } else {
            "⭐ Star Message"
        };
        let item = |label: &'static str, suffix: &'static str| {
            div()
                .id(SharedString::from(format!("{suffix}-{msg_id}")))
                .px_2()
                .py_1()
                .rounded_xs()
                .text_xs()
                .text_color(rgb(TEXT))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(ROW_HOVER)))
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
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .shadow_lg()
                    .child(item("💬 Start a Discussion", "m-thread").on_click({
                        let this = this.clone();
                        let tmid = msg_id.clone();
                        move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.open_menu = None;
                                let _ = chat
                                    .commands
                                    .try_send(ChatCommand::OpenThread { tmid: tmid.clone() });
                                cx.notify();
                            });
                        }
                    }))
                    .child(item("↩ Quote", "m-quote").on_click({
                        let this = this.clone();
                        let room_id = room_id.clone();
                        let msg_id = msg_id.clone();
                        move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.open_menu = None;
                                chat.state.quote = Some((room_id.clone(), msg_id.clone()));
                                cx.notify();
                            });
                        }
                    }))
                    .child(item(star_label, "m-star").on_click({
                        let this = this.clone();
                        let msg_id = msg_id.clone();
                        move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.toggle_star(msg_id.clone(), starred, cx)
                            });
                        }
                    }))
                    .when(link_ready, |menu| {
                        menu.child(item("🔗 Copy Link", "m-link").on_click({
                            let this = this.clone();
                            let room_id = room_id.clone();
                            let msg_id = msg_id.clone();
                            move |_, _, cx| {
                                this.update(cx, |chat, cx| {
                                    chat.open_menu = None;
                                    if let Some(link) = chat.permalink(&room_id, &msg_id) {
                                        cx.write_to_clipboard(ClipboardItem::new_string(link));
                                    }
                                    cx.notify();
                                });
                            }
                        }))
                    })
                    .child(item("📋 Copy Text", "m-copy").on_click({
                        let this = this.clone();
                        let text = edit_text.clone();
                        move |_, _, cx| {
                            this.update(cx, |chat, cx| {
                                chat.open_menu = None;
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                cx.notify();
                            });
                        }
                    }))
                    .child(item("✏ Edit", "m-edit").on_click({
                        let this = this.clone();
                        let room_id = room_id.clone();
                        let msg_id = msg_id.clone();
                        let edit_text = edit_text.clone();
                        move |_, window, cx| {
                            this.update(cx, |chat, cx| {
                                chat.open_menu = None;
                                chat.begin_edit(
                                    room_id.clone(),
                                    msg_id.clone(),
                                    edit_text.clone(),
                                    window,
                                    cx,
                                );
                            });
                        }
                    })),
            ),
        )
        .into_any_element()
    });

    div()
        .flex_none()
        .flex()
        .gap_1()
        .opacity(0.)
        .group_hover("msg", |style| style.opacity(1.))
        .when(editing_this || menu_open, |strip| strip.opacity(1.))
        .child(react)
        .child(edit)
        .child(thread)
        .child(delete)
        .child(kebab)
        .children(menu)
        .when(editing_this, |strip| {
            strip.child(button("✖", "cancel").on_click({
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |chat, cx| {
                        chat.editing = None;
                        cx.notify();
                    });
                }
            }))
        })
}

/// Sizes in human units for attachment chips.
fn human_bytes(size: u64) -> String {
    match size {
        0 => "0 B".to_owned(),
        size if size < 1024 => format!("{size} B"),
        size if size < 1024 * 1024 => format!("{:.1} KB", size as f64 / 1024.0),
        size => format!("{:.1} MB", size as f64 / (1024.0 * 1024.0)),
    }
}

/// The hover card over a reaction pill: who reacted.
struct ReactionTooltip {
    text: SharedString,
}

impl gpui::Render for ReactionTooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_xs()
            .text_color(rgb(TEXT))
            .max_w(px(280.0))
            .child(self.text.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    #[test]
    fn pretty_time_renders_zone_offsets_and_falls_back() {
        let utc = jiff::tz::TimeZone::UTC;
        assert_eq!(pretty_time_in("2026-11-07T20:32:00.514Z", &utc), "8:32 PM");
        let msk = jiff::tz::TimeZone::fixed(jiff::tz::Offset::from_hours(3).unwrap());
        assert_eq!(pretty_time_in("2026-11-07T20:32:00.514Z", &msk), "11:32 PM");
        assert_eq!(pretty_time_in("2026-11-07T00:05:00Z", &utc), "12:05 AM");
        assert_eq!(pretty_time_in("not-a-stamp", &utc), "not-a-stamp");
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
