//! The chat view entity: room list, message pane, composer. State lives in
//! the pure [`ChatState`]; events fold in through the compio pump, clicks
//! push commands back.

use gpui::{
    Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, Pixels, ScrollHandle, SharedString,
    Window, div, hsla, img, prelude::*, px, rgb, uniform_list, white,
};
use std::sync::{Arc, Mutex};

use async_channel::Sender;

use super::input::{MUTED, PANEL, TextInput};
use super::markdown::{self, Span};
use crate::app::{ChatCommand, ChatEvent, ChatService, ChatState, UiSnapshot};

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
    focus: FocusHandle,
    /// Render views of the active room's messages, rebuilt on events.
    views: Vec<MessageView>,
    /// Local search box (the pane shows results while a query is set).
    search: Entity<TextInput>,
    /// `(room_id, msg_id)` while the composer edits an existing message.
    editing: Option<(String, String)>,
    /// The pane's scroll; pins to the newest message on room switches.
    scroll: ScrollHandle,
    /// Picked file paths from the attach dialog's worker thread.
    pick_tx: async_channel::Sender<Option<std::path::PathBuf>>,
    /// Room whose messages the scroll handle last re-pinned.
    pinned_room: Option<String>,
}

/// One precomputed message row of the active room: the pane renders
/// these without re-parsing markdown per frame.
struct MessageView {
    id: String,
    room_id: String,
    ts: String,
    sender: String,
    /// Raw text — the edit action puts it back into the composer.
    text: String,
    lines: Vec<Vec<Span>>,
    reactions: Vec<(String, u32)>,
    /// `(name, url, size)` when the message carries an uploaded file.
    file: Option<(String, String, u64)>,
    /// Cached preview path for image attachments.
    image_path: Option<String>,
    /// Cached avatar path for the sender; initials render as fallback.
    avatar: Option<String>,
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
                    let _ = cx.update(|app| app.quit());
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

        Self {
            state: ChatState::default(),
            commands,
            snapshot,
            composer: cx.new(|cx| TextInput::new("Message…", cx)),
            focus: cx.focus_handle(),
            views: Vec::new(),
            search: cx.new(|cx| TextInput::new("Search…", cx)),
            editing: None,
            scroll: ScrollHandle::new(),
            pinned_room: None,
            pick_tx,
        }
    }

    fn apply_event(&mut self, event: ChatEvent, cx: &mut Context<Self>) {
        let follow_up = self.state.apply(event);
        if let Some(command) = follow_up {
            let _ = self.commands.try_send(command);
        }
        // Render data derives in the entity, never in render: the pane only
        // maps the precomputed spans to elements.
        self.rebuild_views();
        self.mirror();
        cx.notify();
    }

    /// Re-parses the active room's messages into render views (markdown
    /// spans with emoji paths resolved).
    fn rebuild_views(&mut self) {
        let switched = self.pinned_room.as_deref() != self.state.active_room.as_deref();
        let at_bottom = self.scroll.offset().y >= -self.scroll.max_offset().height;
        let emoji = &self.state.emoji;
        let chat_avatars = &self.state.avatars;
        self.views = self
            .state
            .active_messages()
            .iter()
            .map(|message| MessageView {
                id: message.id.clone(),
                room_id: message.room_id.clone(),
                ts: pretty_time(&message.ts),
                sender: message.sender.clone(),
                text: message.text.clone(),
                lines: message
                    .text
                    .lines()
                    .map(|line| markdown::parse(line, &|name| emoji.get(name).cloned()))
                    .collect(),
                reactions: message
                    .reactions
                    .iter()
                    .map(|(name, count)| (name.clone(), *count))
                    .collect(),
                file: message
                    .file
                    .as_ref()
                    .map(|file| (file.name.clone(), file.url.clone(), file.size)),
                image_path: message
                    .file
                    .as_ref()
                    .and_then(|file| file.image_path.clone()),
                avatar: chat_avatars.get(&message.sender).cloned(),
            })
            .collect();
        if switched || at_bottom {
            self.scroll.scroll_to_bottom();
            self.pinned_room = self.state.active_room.clone();
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
        let _ = self.commands.try_send(ChatCommand::OpenRoom { room_id });
        cx.notify();
    }

    fn send_message(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(room_id) = self.state.active_room.clone() else {
            self.state.error = Some("open a room first".to_owned());
            cx.notify();
            return;
        };
        let text = self.composer.read(cx).content().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer
            .update(cx, |composer, cx| composer.set_content("", cx));
        match self.editing.take() {
            Some((_, msg_id)) => {
                let _ = self.commands.try_send(ChatCommand::EditMessage {
                    room_id,
                    msg_id,
                    text,
                });
            }
            None => {
                let _ = self.commands.try_send(ChatCommand::Send { room_id, text });
            }
        }
        cx.notify();
    }
}

impl ChatRoot {
    /// Toggles a 👍 reaction on one message (chat.react toggles on
    /// repeat).
    fn toggle_reaction(&mut self, room_id: &str, msg_id: &str, cx: &mut Context<Self>) {
        let _ = self.commands.try_send(ChatCommand::React {
            room_id: room_id.to_owned(),
            msg_id: msg_id.to_owned(),
            emoji: "👍".to_owned(),
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
        window.focus(&self.composer.read(cx).focus_handle(cx));
        cx.notify();
    }

    /// Sends the current search box content as the active query; empty
    /// clears the results.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).content().trim().to_owned();
        let _ = self.commands.try_send(ChatCommand::Search(query));
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

impl Focusable for ChatRoot {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

fn panel(title: &str, content: impl IntoElement, width: Option<Pixels>) -> impl IntoElement {
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
                .text_xs()
                .text_color(rgb(MUTED))
                .child(title.to_owned()),
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
        // runs into the hundreds on real servers.
        let this = cx.entity();
        let active = self.state.active_room.clone();
        let room_count = self.state.rooms.len();
        let rooms = uniform_list("rooms", room_count, move |range, _window, cx| {
            let root = this.read(cx);

            range
                .map(|index| {
                    let room = &root.state.rooms[index];
                    let room_id = room.id.clone();
                    let selected = active.as_deref() == Some(room.id.as_str());
                    let unread = room.unread;
                    let this = this.clone();
                    div()
                        .id(SharedString::from(room.id.clone()))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .text_sm()
                        .overflow_hidden()
                        .cursor_pointer()
                        .when(selected, |row| row.bg(rgb(ACCENT)))
                        .hover(|style| style.bg(rgb(BORDER)))
                        .on_click(move |_, _, cx| {
                            this.update(cx, |chat, cx| chat.open_room(room_id.clone(), cx));
                        })
                        .child(SharedString::from(room.name.clone()))
                        // Unread badge: count from the subscriptions stream.
                        .when(unread > 0, |row| {
                            row.child(
                                div().flex_1().flex().justify_end().child(
                                    div()
                                        .px_1()
                                        .rounded_xs()
                                        .bg(rgb(ACCENT))
                                        .text_xs()
                                        .child(SharedString::from(unread.to_string())),
                                ),
                            )
                        })
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0();

        // The pane virtualizes: `list` materializes only the visible
        // rows, pins to the newest while the user is at the bottom
        // (ListAlignment::Bottom), and resets when the count changes.
        let this = cx.entity();
        let messages = div()
            .id("messages")
            .flex()
            .flex_col()
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .children(self.views.iter().map(|view| {
                let room_id = view.room_id.clone();
                let msg_id = view.id.clone();
                let edit_text = view.text.clone();
                let editing_this = matches!(
                    self.editing.as_ref(),
                    Some((room, msg)) if *room == room_id && *msg == msg_id
                );
                let badges = div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(view.reactions.iter().map(|(name, count)| {
                        div()
                            .px_1p5()
                            .rounded_full()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(PANEL))
                            .text_xs()
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(format!("{name} ×{count}")))
                    }));
                div()
                    .id(SharedString::from(format!("msg-{msg_id}")))
                    .group("msg")
                    .flex()
                    .gap_2()
                    .items_start()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .hover(|style| style.bg(rgb(ROW_HOVER)))
                    .child(match view.avatar.as_deref() {
                        Some(path) => img(std::path::PathBuf::from(path))
                            .flex_none()
                            .size(px(32.0))
                            .rounded_full()
                            .into_any_element(),
                        None => avatar(&view.sender).into_any_element(),
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
                                    .flex()
                                    .items_baseline()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(rgb(TEXT))
                                            .child(SharedString::from(view.sender.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .child(SharedString::from(view.ts.clone())),
                                    ),
                            )
                            .child(message_element(view))
                            .children(view.file.as_ref().map(|(name, url, size)| {
                                attachment_card(
                                    &msg_id,
                                    name,
                                    url,
                                    *size,
                                    view.image_path.as_deref(),
                                )
                            }))
                            .child(badges),
                    )
                    .child(action_strip(ActionContext {
                        this: this.clone(),
                        room_id,
                        msg_id,
                        edit_text,
                        editing_this,
                    }))
            }));

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
                .flex_none()
                .gap_2()
                .child(div().flex_1().child(self.search.clone()))
                .child(
                    div()
                        .id("search")
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
        let pane = if let Some((room_id, tmid)) = self.state.open_thread.clone() {
            // Thread view: back button, then the thread's messages.
            let this = search_this.clone();
            let rows = self
                .state
                .thread(&tmid)
                .iter()
                .map(|message| {
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(TEXT))
                                .child(SharedString::from(message.sender.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .child(SharedString::from(message.text.clone())),
                        )
                })
                .collect::<Vec<_>>();
            div()
                .flex()
                .flex_col()
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
                .children(rows)
                .into_any_element()
        } else if let Some(query) = self.state.search_query.as_deref()
            && !query.trim().is_empty()
        {
            let hits = self.state.search(query);
            let results = hits
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
                                let query = chat.search.read(cx).content().to_owned();
                                chat.open_room(room_id.clone(), cx);
                                chat.search.update(cx, |box_, cx| {
                                    box_.set_content("", cx);
                                });
                                let _ = chat.commands.try_send(ChatCommand::Search(query));
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
                .collect::<Vec<_>>();
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(results)
                .into_any_element()
        } else {
            messages.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_3()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .track_focus(&self.focus)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().text_xl().child("rchat-blazar"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(SharedString::from(who)),
                    ),
            )
            .child(error)
            .child(search_strip)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(panel("rooms", rooms, Some(px(240.0))))
                    .child(panel(
                        "messages",
                        div().flex().flex_col().min_h_0().child(toasts).child(pane),
                        None,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        div()
                            .id("attach")
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
                            .px_3()
                            .py_1()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(ACCENT)))
                            .active(|style| style.opacity(0.85))
                            .on_click(
                                cx.listener(|chat, _, window, cx| chat.send_message(window, cx)),
                            )
                            .child(match self.editing.is_some() {
                                true => "save",
                                false => "send",
                            }),
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
        .size(px(32.0))
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
        .max_w(px(420.0))
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
                        .size(px(40.0))
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
                .max_w(px(400.0))
                .rounded_sm()
        }))
}

fn message_element(view: &MessageView) -> gpui::Div {
    let lines: Vec<gpui::Div> = view
        .lines
        .iter()
        .map(|spans| {
            let mut row = div().flex().flex_wrap().items_center();
            for span in spans {
                row = row.child(span_element(span.clone()));
            }
            row
        })
        .collect();
    div().flex().flex_col().children(lines)
}

/// One span as an inline element.
fn span_element(span: Span) -> gpui::Div {
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
            element
        }
        Span::Link { label, .. } => div()
            .text_sm()
            .underline()
            .text_color(rgb(ACCENT))
            .child(SharedString::from(label)),
        Span::Mention(name) => div()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(ACCENT))
            .child(SharedString::from(name)),
        Span::Emoji { path } => {
            div().child(img(std::path::PathBuf::from(&path)).h(px(18.0)).w(px(18.0)))
        }
    }
}

/// What a message row's action buttons need at click time.
struct ActionContext {
    this: Entity<ChatRoot>,
    room_id: String,
    msg_id: String,
    edit_text: String,
    editing_this: bool,
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

    let react = button("👍", "react").on_click({
        let this = this.clone();
        let room_id = room_id.clone();
        let msg_id = msg_id.clone();
        move |_, _, cx| {
            this.update(cx, |chat, cx| chat.toggle_reaction(&room_id, &msg_id, cx));
        }
    });
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

    div()
        .flex_none()
        .flex()
        .gap_1()
        .opacity(0.)
        .group_hover("msg", |style| style.opacity(1.))
        .when(editing_this, |strip| strip.opacity(1.))
        .child(react)
        .child(edit)
        .child(thread)
        .child(delete)
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
}
