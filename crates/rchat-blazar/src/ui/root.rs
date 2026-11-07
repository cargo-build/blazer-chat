//! The chat view entity: room list, message pane, composer. State lives in
//! the pure [`ChatState`]; events fold in through the compio pump, clicks
//! push commands back.

use gpui::{
    Context, Entity, FocusHandle, Focusable, FontWeight, SharedString, Window, div, img,
    prelude::*, px, rgb, uniform_list,
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
}

/// One precomputed message row of the active room: the pane renders
/// these without re-parsing markdown per frame.
struct MessageView {
    ts: String,
    sender: String,
    lines: Vec<Vec<Span>>,
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
                ts: message.ts.split('T').nth(1).unwrap_or_default().to_owned(),
                sender: message.sender.clone(),
                lines: message
                    .text
                    .lines()
                    .map(|line| markdown::parse(line, &|name| emoji.get(name).cloned()))
                    .collect(),
            })
            .collect();
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
        let _ = self.commands.try_send(ChatCommand::Send { room_id, text });
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
                })
                .collect::<Vec<_>>()
        });

        let messages = div()
            .id("messages")
            .flex()
            .flex_col()
            .gap_1()
            .overflow_y_scroll()
            .children(self.views.iter().map(|view| {
                div()
                    .flex()
                    .gap_2()
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
                    .child(message_element(view))
            }));

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
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(panel("rooms", rooms))
                    .child(panel("messages", messages)),
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
                            .child("Send"),
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
