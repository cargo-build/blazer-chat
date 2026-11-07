//! The chat view entity: room list, message pane, composer. State lives in
//! the pure [`ChatState`]; events fold in through the compio pump, clicks
//! push commands back.

use gpui::{
    Context, Entity, FocusHandle, Focusable, FontWeight, ListAlignment, ListState, SharedString,
    Window, div, img, list, prelude::*, px, rgb, uniform_list,
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

/// The one window's root view.
pub(crate) struct ChatRoot {
    state: ChatState,
    commands: Sender<ChatCommand>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
    pub(crate) composer: Entity<TextInput>,
    focus: FocusHandle,
    /// Render views of the active room's messages, rebuilt on events.
    views: Vec<MessageView>,
    /// Virtualized pane state: only visible rows materialize.
    messages_list: ListState,
    /// Local search box (the pane shows results while a query is set).
    search: Entity<TextInput>,
    /// `(room_id, msg_id)` while the composer edits an existing message.
    editing: Option<(String, String)>,
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

        Self {
            state: ChatState::default(),
            commands,
            snapshot,
            composer: cx.new(|cx| TextInput::new("Message…", cx)),
            focus: cx.focus_handle(),
            views: Vec::new(),
            messages_list: ListState::new(0, ListAlignment::Bottom, px(64.)),
            search: cx.new(|cx| TextInput::new("Search…", cx)),
            editing: None,
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
        let emoji = &self.state.emoji;
        self.views = self
            .state
            .active_messages()
            .iter()
            .map(|message| MessageView {
                id: message.id.clone(),
                room_id: message.room_id.clone(),
                ts: message.ts.split('T').nth(1).unwrap_or_default().to_owned(),
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
            })
            .collect();
        self.messages_list.reset(self.views.len());
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

fn panel(title: &str, content: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
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
        });

        // The pane virtualizes: `list` materializes only the visible
        // rows, pins to the newest while the user is at the bottom
        // (ListAlignment::Bottom), and resets when the count changes.
        let this = cx.entity();
        let state = self.messages_list.clone();
        let messages = list(state, move |index, _window, cx| {
            let root = this.read(cx);
            let view = &root.views[index];
            let room_id = view.room_id.clone();
            let msg_id = view.id.clone();
            let edit_text = view.text.clone();
            let editing_this = matches!(
                root.editing.as_ref(),
                Some((room, msg)) if *room == room_id && *msg == msg_id
            );
            let badges = div()
                .flex()
                .flex_wrap()
                .gap_1()
                .children(view.reactions.iter().map(|(name, count)| {
                    div()
                        .px_1()
                        .rounded_xs()
                        .bg(rgb(PANEL))
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(format!("{name} ×{count}")))
                }));
            div()
                .id(SharedString::from(format!("msg-{msg_id}")))
                .group("msg")
                .flex()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(view.ts.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(view.sender.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(message_element(view))
                        .child(badges),
                )
                .child(action_strip(ActionContext {
                    this: this.clone(),
                    room_id,
                    msg_id,
                    edit_text,
                    editing_this,
                }))
                .into_any_element()
        });

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
                                .text_color(rgb(MUTED))
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
                    .child(panel("rooms", rooms))
                    .child(panel("messages", pane)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap_2()
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
