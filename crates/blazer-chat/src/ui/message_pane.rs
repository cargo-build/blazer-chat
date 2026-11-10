//! The message pane's row model, pure helpers and the [`MessagePane`]
//! entity that owns the pane state; ChatRoot drives it per event and
//! per frame.

use gpui::{
    Anchor, AnyElement, Context, Entity, EventEmitter, FollowMode, FontWeight, Hsla, ListAlignment,
    ListOffset, ListState, Rems, Role, SharedString, Window, anchored, deferred, div, hsla, img,
    list, prelude::*, px, rems, rgb,
};

use super::markdown::{self, Span};
use super::theme::Palette;
use crate::app::{ChatCommand, ChatState};
use crate::rc::Msg;

/// One precomputed message row of the active room: the pane renders
/// these without re-parsing markdown per frame.
pub(crate) struct MessageView {
    pub(crate) id: String,
    pub(crate) system: bool,
    pub(crate) room_id: String,
    pub(crate) ts: String,
    pub(crate) sender: String,
    /// Raw text — the edit action puts it back into the composer.
    pub(crate) text: String,
    pub(crate) lines: std::sync::Arc<Vec<Vec<Span>>>,
    pub(crate) reactions: Vec<(String, crate::rc::Reaction)>,
    /// `(name, url, size)` when the message carries an uploaded file.
    pub(crate) file: Option<(String, String, u64)>,
    /// Cached preview path for image attachments.
    pub(crate) image_path: Option<String>,
    /// Cached avatar path for the sender; initials render as fallback.
    pub(crate) avatar: Option<String>,
    /// `(reply count, formatted last reply)` when this message starts a
    /// thread.
    pub(crate) thread: Option<(u32, String)>,
    /// Starred by the logged-in user (menu shows "Remove Star").
    pub(crate) starred: bool,
    /// The server-side edit flag: renders the "(edited)" marker.
    pub(crate) edited: bool,
    /// UTC date (YYYY-MM-DD); the pane inserts day separators between
    /// different dates.
    pub(crate) day: String,
    /// Raw server timestamp for the grouping window.
    pub(crate) ts_raw: String,
}

/// Consecutive-grouping window: same sender within five minutes.
/// How the pane's row list changed across a rebuild: identity of the
/// boundary views decides, not counts alone (deepseek S5 T5).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RowChange {
    Same,
    /// Grew at the tail (new message): `usize` rows added.
    Appended(usize),
    /// Grew at the head (load-older): `usize` rows added.
    Prepended(usize),
    /// Anything else — reset and restore the anchor by identity.
    Reshaped,
}

pub(crate) fn classify_row_change(
    old_len: usize,
    old_first: Option<&str>,
    old_last: Option<&str>,
    new_len: usize,
    new_first: Option<&str>,
    new_last: Option<&str>,
) -> RowChange {
    match old_len.cmp(&new_len) {
        // Equal length still hides a mid-list edit/delete+add: when a
        // boundary identity moved, the anchor must re-derive.
        std::cmp::Ordering::Equal => {
            if old_first == new_first && old_last == new_last {
                RowChange::Same
            } else {
                RowChange::Reshaped
            }
        }
        std::cmp::Ordering::Greater => RowChange::Reshaped,
        std::cmp::Ordering::Less => {
            let added = new_len - old_len;
            // Growing from empty is an append: splice(0..len) would
            // shift the anchor of an empty viewport for nothing.
            if old_first == new_first || old_first.is_none() {
                RowChange::Appended(added)
            } else if old_last == new_last {
                RowChange::Prepended(added)
            } else {
                RowChange::Reshaped
            }
        }
    }
}

/// Two adjacent views collapse into one compact row: same non-system
/// sender on the same day within five minutes.
pub(crate) fn adjacent_grouped(previous: &MessageView, view: &MessageView) -> bool {
    !previous.system
        && !view.system
        && previous.sender == view.sender
        && previous.day == view.day
        && within_five_minutes(&previous.ts_raw, &view.ts_raw)
}

pub(crate) fn within_five_minutes(prev: &str, next: &str) -> bool {
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
pub(crate) fn local_day(ts: &str) -> String {
    ts.parse::<jiff::Timestamp>()
        .map(|stamp| {
            stamp
                .to_zoned(jiff::tz::TimeZone::system())
                .date()
                .to_string()
        })
        .unwrap_or_else(|_| ts.split('T').next().unwrap_or(ts).to_owned())
}

pub(crate) fn pretty_day(ts: &str) -> String {
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
pub(crate) fn pretty_time(ts: &str) -> String {
    pretty_time_in(ts, &jiff::tz::TimeZone::system())
}

/// The same formatting against an explicit zone (tests pin offsets).
pub(crate) fn pretty_time_in(ts: &str, tz: &jiff::tz::TimeZone) -> String {
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
pub(crate) fn raw_time(ts: &str) -> String {
    ts.split_once('T')
        .map(|(_, time)| time.to_owned())
        .unwrap_or_else(|| ts.to_owned())
}

/// One message body: the text split into lines (newlines preserved), each
/// line parsed into markdown spans and laid out as a wrapping row so inline
/// emoji images flow with the text.
/// The sender's initial on a per-user color disc (Rocket.Chat colors
/// usernames too); `size` scales the disc to its site (message rows
/// 2.0, the sidebar identity column 1.25).
pub(crate) fn avatar(sender: &str, p: Palette, size: Rems) -> gpui::Div {
    let initial = sender
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .next()
        .unwrap_or('?');
    div()
        .flex_none()
        .size(size)
        // RC default: square avatars with a small radius (m18) —
        // the round disc read as a different product.
        .rounded_sm()
        .bg(user_color(sender))
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(rgb(p.accent_fg))
        .child(SharedString::from(initial.to_string()))
}

/// Deterministic per-sender accent from the name itself.
pub(crate) fn user_color(sender: &str) -> Hsla {
    let hash = sender.bytes().fold(5381u32, |acc, byte| {
        acc.wrapping_mul(33).wrapping_add(u32::from(byte))
    });
    hsla((hash % 360) as f32, 0.55, 0.62, 1.0)
}

/// The wire's emoji names (`+1`) render as their unicode glyphs
/// (Rocket.Chat's canonical names); unknown names stay as-is.
pub(crate) fn display_emoji(name: &str) -> String {
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

/// One attachment as a card: type tile, name link, size — with the
/// fetched image inline when cached (the original client's look).
pub(crate) fn attachment_card(
    msg_id: &str,
    name: &str,
    url: &str,
    size: u64,
    image_path: Option<&str>,
    p: Palette,
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
        .bg(rgb(p.panel))
        .border_1()
        .border_color(rgb(p.border))
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
                        .bg(rgb(p.code_bg))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(p.text))
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
                                .text_color(rgb(p.accent))
                                .cursor_pointer()
                                .hover(|style| style.underline())
                                .child(SharedString::from(name.to_owned()))
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(p.muted))
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

pub(crate) fn message_element(
    view: &MessageView,
    me: Option<&str>,
    this: &Entity<MessagePane>,
    p: Palette,
) -> gpui::Div {
    let lines: Vec<gpui::Div> = view
        .lines
        .iter()
        .enumerate()
        .map(|(line_index, spans)| {
            let mut row = div().flex().flex_wrap().items_center();
            for (span_index, span) in spans.iter().enumerate() {
                let id = format!("{}-{line_index}-{span_index}", view.id);
                row = row.child(span_element(span.clone(), me, this, &id, p));
            }
            row
        })
        .collect();
    div().flex().flex_col().children(lines)
}

/// One span as an inline element; links open in the browser,
/// mentions highlight the own name and open the user's card (m12
/// D3). `id` makes the clickable spans stateful and unique.
pub(crate) fn span_element(
    span: Span,
    me: Option<&str>,
    this: &Entity<MessagePane>,
    id: &str,
    p: Palette,
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
                    .bg(rgb(p.code_bg))
                    .px_1()
                    .rounded_sm();
            }
            element.into_any_element()
        }
        Span::Link { label, url } => div()
            .id(SharedString::from(format!("lnk-{id}")))
            .text_sm()
            .underline()
            .text_color(rgb(p.accent))
            .cursor_pointer()
            .hover(|style| style.text_color(rgb(p.accent_fg)))
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
                .when(mine, |pill| {
                    // accent_fill, not accent: white needs >= 4.5
                    // on the fill (deepseek D13; plain accent 2.57 dark)
                    pill.bg(rgb(p.accent_fill)).text_color(rgb(p.accent_fg))
                })
                .when(!mine, |pill| pill.text_color(rgb(p.accent)))
                .hover(|style| style.bg(rgb(p.row_hover)).text_color(rgb(p.text)))
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
            .bg(rgb(p.input_bg))
            .border_1()
            .border_color(rgb(p.border))
            .rounded_sm()
            .px_2()
            .py_1()
            .min_w_0()
            .child(SharedString::from(body.clone()))
            .into_any_element(),
    }
}

/// The click handler opening a user's card — avatars, sender names
/// and mention pills (m12 D3/F8); unknown users fetch their card
/// data on the first open.
pub(crate) fn card_opener(
    this: Entity<MessagePane>,
    name: String,
) -> impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
        this.update(cx, |_, cx| {
            cx.emit(PaneEvent::OpenUserCard {
                username: name.clone(),
            });
        });
    }
}

/// Opens a link in the system browser; the spawn is fire-and-forget
/// (m12 D3).
pub(crate) fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// What a message row's action buttons need at click time.
/// One virtualized pane row: a message or a day divider.
pub(crate) enum PaneRow {
    Separator {
        day_ts: String,
    },
    UnreadMarker {
        view_index: usize,
    },
    /// `grouped` is computed once per rebuild from view adjacency, not
    /// per frame (m10 S4).
    Message {
        view_index: usize,
        grouped: bool,
    },
}

pub(crate) struct ActionContext {
    pub(crate) this: Entity<MessagePane>,
    pub(crate) room_id: String,
    pub(crate) msg_id: String,
    pub(crate) edit_text: String,
    pub(crate) editing_this: bool,
    /// This row's labeled menu is open.
    pub(crate) menu_open: bool,
    /// Starred by me (menu label flips).
    pub(crate) starred: bool,
    /// The room supports permalinks (channels/groups; not DMs).
    pub(crate) link_ready: bool,
}

/// The hover-revealed per-message actions: react, edit, delete. The strip
/// is invisible until its row (`group("msg")`) is hovered; the row being
/// edited also shows a cancel button.
pub(crate) fn action_strip(actions: ActionContext, p: Palette) -> gpui::Div {
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
            .text_color(rgb(p.muted))
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
                this.update(cx, |_, cx| {
                    cx.emit(PaneEvent::ToggleReaction {
                        room_id: room_id.clone(),
                        msg_id: msg_id.clone(),
                        emoji: name.to_owned(),
                    });
                })
            })
        }));
    let edit = button("✏", "edit").on_click({
        let this = this.clone();
        let room_id = room_id.clone();
        let msg_id = msg_id.clone();
        let edit_text = edit_text.clone();
        move |_, _, cx| {
            this.update(cx, |_, cx| {
                cx.emit(PaneEvent::BeginEdit {
                    room_id: room_id.clone(),
                    msg_id: msg_id.clone(),
                    text: edit_text.clone(),
                });
            });
        }
    });
    let thread = button("🧵", "thread").on_click({
        let this = this.clone();
        let tmid = msg_id.clone();
        move |_, _, cx| {
            this.update(cx, |_, cx| {
                cx.emit(PaneEvent::OpenThread { tmid: tmid.clone() });
            });
        }
    });
    let delete = button("🗑", "delete").on_click({
        let this = this.clone();
        let room_id = room_id.clone();
        let msg_id = msg_id.clone();
        move |_, _, cx| {
            this.update(cx, |_, cx| {
                cx.emit(PaneEvent::DeleteMessage {
                    room_id: room_id.clone(),
                    msg_id: msg_id.clone(),
                });
            });
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
            this.update(cx, |_, cx| {
                cx.emit(PaneEvent::ToggleMenu {
                    room_id: room_id.clone(),
                    msg_id: msg_id.clone(),
                });
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
                    .child(item("💬 Start a Discussion", "m-thread").on_click({
                        let this = this.clone();
                        let tmid = msg_id.clone();
                        move |_, _, cx| {
                            this.update(cx, |_, cx| {
                                cx.emit(PaneEvent::CloseMenu);
                                cx.emit(PaneEvent::OpenThread { tmid: tmid.clone() });
                            });
                        }
                    }))
                    .child(item("↩ Quote", "m-quote").on_click({
                        let this = this.clone();
                        let room_id = room_id.clone();
                        let msg_id = msg_id.clone();
                        move |_, _, cx| {
                            this.update(cx, |_, cx| {
                                cx.emit(PaneEvent::CloseMenu);
                                cx.emit(PaneEvent::QuoteMessage {
                                    room_id: room_id.clone(),
                                    msg_id: msg_id.clone(),
                                });
                            });
                        }
                    }))
                    .child(item(star_label, "m-star").on_click({
                        let this = this.clone();
                        let msg_id = msg_id.clone();
                        move |_, _, cx| {
                            this.update(cx, |_, cx| {
                                cx.emit(PaneEvent::ToggleStar {
                                    msg_id: msg_id.clone(),
                                    starred,
                                });
                            });
                        }
                    }))
                    .when(link_ready, |menu| {
                        menu.child(item("🔗 Copy Link", "m-link").on_click({
                            let this = this.clone();
                            let room_id = room_id.clone();
                            let msg_id = msg_id.clone();
                            move |_, _, cx| {
                                this.update(cx, |_, cx| {
                                    cx.emit(PaneEvent::CloseMenu);
                                    cx.emit(PaneEvent::CopyPermalink {
                                        room_id: room_id.clone(),
                                        msg_id: msg_id.clone(),
                                    });
                                });
                            }
                        }))
                    })
                    .child(item("📋 Copy Text", "m-copy").on_click({
                        let this = this.clone();
                        let text = edit_text.clone();
                        move |_, _, cx| {
                            this.update(cx, |_, cx| {
                                cx.emit(PaneEvent::CloseMenu);
                                cx.emit(PaneEvent::CopyText { text: text.clone() });
                            });
                        }
                    }))
                    .child(item("✏ Edit", "m-edit").on_click({
                        let this = this.clone();
                        let room_id = room_id.clone();
                        let msg_id = msg_id.clone();
                        let edit_text = edit_text.clone();
                        move |_, _, cx| {
                            this.update(cx, |_, cx| {
                                cx.emit(PaneEvent::CloseMenu);
                                cx.emit(PaneEvent::BeginEdit {
                                    room_id: room_id.clone(),
                                    msg_id: msg_id.clone(),
                                    text: edit_text.clone(),
                                });
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
                    this.update(cx, |_, cx| {
                        cx.emit(PaneEvent::CancelEdit);
                    });
                }
            }))
        })
}

/// Sizes in human units for attachment chips.
pub(crate) fn human_bytes(size: u64) -> String {
    match size {
        0 => "0 B".to_owned(),
        size if size < 1024 => format!("{size} B"),
        size if size < 1024 * 1024 => format!("{:.1} KB", size as f64 / 1024.0),
        size => format!("{:.1} MB", size as f64 / (1024.0 * 1024.0)),
    }
}

/// The hover card over a reaction pill: who reacted.
pub(crate) struct ReactionTooltip {
    pub(crate) text: SharedString,
    /// The palette captured at construction (the tooltip re-renders
    /// with its row's frame, m9).
    pub(crate) palette: Palette,
}

impl gpui::Render for ReactionTooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let p = self.palette;
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(p.panel))
            .border_1()
            .border_color(rgb(p.border))
            .text_xs()
            .text_color(rgb(p.text))
            .max_w(px(280.0))
            .child(self.text.clone())
    }
}

/// Parent→pane render inputs, built per frame in `ChatRoot::render`
/// and captured by the list closure as an owned snapshot (2.4(7): the
/// closure must be 'static, so small clones replace today's borrows).
#[derive(Clone)]
pub(crate) struct PaneInputs {
    pub(crate) editing: Option<(String, String)>,
    pub(crate) open_menu: Option<(String, String)>,
    pub(crate) me: Option<String>,
    pub(crate) emoji: std::collections::BTreeMap<String, String>,
    /// The active room supports permalinks (channels/groups; not DMs).
    pub(crate) link_ready: bool,
    /// The window's palette for this frame (m9 appearance plumbing).
    pub(crate) palette: Palette,
}

/// One parent touch per variant: everything a pane row can ask the
/// root to do (m13 S2b). Emitted by row closures; ChatRoot maps each
/// to its existing method in `on_pane_event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PaneEvent {
    ToggleReaction {
        room_id: String,
        msg_id: String,
        emoji: String,
    },
    BeginEdit {
        room_id: String,
        msg_id: String,
        text: String,
    },
    CancelEdit,
    DeleteMessage {
        room_id: String,
        msg_id: String,
    },
    OpenThread {
        tmid: String,
    },
    QuoteMessage {
        room_id: String,
        msg_id: String,
    },
    ToggleStar {
        msg_id: String,
        starred: bool,
    },
    CopyPermalink {
        room_id: String,
        msg_id: String,
    },
    CopyText {
        text: String,
    },
    ToggleMenu {
        room_id: String,
        msg_id: String,
    },
    CloseMenu,
    OpenUserCard {
        username: String,
    },
}

impl EventEmitter<PaneEvent> for MessagePane {}

/// The virtualized message pane: owns the row model, the list state
/// and the parse cache (m13 S2a). ChatRoot folds events, then drives
/// `sync`; render calls `element`/`poll_paging`; row clicks emit
/// [`PaneEvent`] (m13 S2b).
pub(crate) struct MessagePane {
    /// This entity's own handle, read by the list closure and used by
    /// row closures to emit PaneEvent.
    entity: Entity<Self>,
    /// Real pane rebuilds (version-guard misses); served over /state
    /// as the runtime half of the S5 regression guard.
    pub(crate) pane_rebuilds: u64,
    /// Render views of the active room's messages, rebuilt on events.
    views: Vec<MessageView>,
    /// The pane's rows: messages interleaved with day separators.
    rows: Vec<PaneRow>,
    /// The pane's virtualized list state; follows the tail on new
    /// messages and stops when the operator scrolls up (m10 S4).
    messages_list: ListState,
    /// Parsed markdown per (message id, text): rebuilds reuse it instead
    /// of re-parsing every message on every event.
    parsed: std::collections::HashMap<(String, String), std::sync::Arc<Vec<Vec<markdown::Span>>>>,
    /// The state versions the views were built from (skip no-op
    /// rebuilds).
    views_room: Option<String>,
    views_room_version: u64,
    views_emoji_version: u64,
    views_avatars_version: u64,
    /// An older-slice fetch is in flight (one at a time per pane).
    loading_older: bool,
    /// One older-slice fetch per visit to the pane's top edge.
    paged_since_top: bool,
    /// Room whose messages the scroll handle last re-pinned.
    pinned_room: Option<String>,
}

impl MessagePane {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            entity: cx.entity(),
            pane_rebuilds: 0,
            views: Vec::new(),
            rows: Vec::new(),
            messages_list: {
                let state = ListState::new(0, ListAlignment::Bottom, px(400.))
                    .with_uniform_item_height(px(52.));
                state.set_follow_mode(FollowMode::Tail);
                state
            },
            parsed: Default::default(),
            views_room: None,
            views_room_version: u64::MAX,
            views_emoji_version: u64::MAX,
            views_avatars_version: u64::MAX,
            loading_older: false,
            paged_since_top: false,
            pinned_room: None,
        }
    }

    /// Real rebuild count for the S5 regression guard (/state).
    pub(crate) fn rebuilds(&self) -> u64 {
        self.pane_rebuilds
    }

    /// An older slice landed: the fetch slot frees up.
    pub(crate) fn clear_loading_older(&mut self) {
        self.loading_older = false;
    }

    /// Re-parses the active room's messages into render views (markdown
    /// spans with emoji paths resolved).
    pub(crate) fn sync(&mut self, state: &ChatState) {
        let switched = self.pinned_room.as_deref() != state.active_room.as_deref();
        // Nothing visible changed (badge/event churn): keep the views.
        let room_version = state
            .active_room
            .as_deref()
            .and_then(|room| state.message_versions.get(room).copied())
            .unwrap_or(0);
        if !switched
            && self.views_room == state.active_room
            && self.views_room_version == room_version
            && self.views_emoji_version == state.emoji_version
            && self.views_avatars_version == state.avatars_version
        {
            return;
        }
        let anchor = self.messages_list.logical_scroll_top();
        let tail = self.messages_list.is_following_tail();
        let old_rows_len = self.rows.len();
        let old_first = self.views.first().map(|view| view.id.clone());
        let old_last = self.views.last().map(|view| view.id.clone());
        // The one anchor: the first message row at-or-below the
        // viewport top, with its sub-row offset. Restore it if the
        // rebuild reshapes the rows (deletes, edits that rewrap);
        // splice arms keep it natively.
        let restore = (!tail && !switched).then(|| {
            let id = self
                .rows
                .iter()
                .skip(anchor.item_ix)
                .find_map(|row| match row {
                    PaneRow::Message { view_index, .. } => Some(self.views[*view_index].id.clone()),
                    _ => None,
                });
            id.map(|id| (id, anchor.offset_in_item))
        });
        let messages = state.active_messages().to_vec();
        self.views = messages
            .iter()
            .map(|message| self.message_view(state, message))
            .collect();
        // Interleave day separators; the pane renders them between
        // messages. The first-unread boundary draws a divider before
        // its message (m12 F1).
        let boundary = state
            .first_unread
            .get(state.active_room.as_deref().unwrap_or_default())
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
            let grouped = view_index > 0 && adjacent_grouped(&self.views[view_index - 1], view);
            self.rows.push(PaneRow::Message {
                view_index,
                grouped,
            });
        }
        if switched {
            // Jump to the unread divider when the room opened with
            // unreads; otherwise land at the newest message. While
            // following, the list's Tail mode re-pins by itself.
            match self
                .rows
                .iter()
                .position(|row| matches!(row, PaneRow::UnreadMarker { .. }))
            {
                Some(index) => self.messages_list.scroll_to_reveal_item(index),
                None => self.messages_list.scroll_to_end(),
            }
            self.pinned_room = state.active_room.clone();
        }
        // Splice preserves the anchor and scroll fill for append-only
        // (new message) and prepend-only (load-older) growth; reset is
        // the general fallback (drops one wheel frame, list.rs:907).
        let new_first = self.views.first().map(|view| view.id.clone());
        let new_last = self.views.last().map(|view| view.id.clone());
        match classify_row_change(
            old_rows_len,
            old_first.as_deref(),
            old_last.as_deref(),
            self.rows.len(),
            new_first.as_deref(),
            new_last.as_deref(),
        ) {
            RowChange::Same => {}
            RowChange::Appended(added) => {
                self.messages_list.splice(old_rows_len..old_rows_len, added);
            }
            RowChange::Prepended(added) => {
                self.messages_list.splice(0..0, added);
            }
            RowChange::Reshaped => {
                self.messages_list.reset(self.rows.len());
                let restored = restore.flatten().and_then(|(id, offset)| {
                    self.rows
                        .iter()
                        .position(|row| {
                            matches!(row, PaneRow::Message { view_index, .. }
                                if self.views[*view_index].id == id)
                        })
                        .map(|index| ListOffset {
                            item_ix: index,
                            offset_in_item: offset,
                        })
                });
                match restored {
                    Some(offset) => self.messages_list.scroll_to(offset),
                    None if !tail => self.messages_list.scroll_to(anchor),
                    None => {}
                }
            }
        }
        self.pane_rebuilds += 1;
        self.views_room = state.active_room.clone();
        self.views_room_version = room_version;
        self.views_emoji_version = state.emoji_version;
        self.views_avatars_version = state.avatars_version;
    }

    /// Builds the render view for one message: markdown parsed through
    /// the shared cache, emoji/avatars resolved, attachments carried.
    pub(crate) fn message_view(&mut self, state: &ChatState, message: &Msg) -> MessageView {
        MessageView {
            id: message.id.clone(),
            system: message.system,
            room_id: message.room_id.clone(),
            starred: message.starred_by_me,
            edited: message.edited,
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
                                            state.emoji.get(name).cloned()
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
            avatar: state.avatars.get(&message.sender).cloned(),
            thread: message
                .tcount
                .map(|count| (count, pretty_time(message.tlm.as_deref().unwrap_or("")))),
        }
    }

    /// The accent divider marking the first unread message (m12 F1).
    fn unread_marker(&self, _view_index: usize, p: Palette) -> AnyElement {
        let label = "Unread messages";
        div()
            .id("unread-divider")
            .aria_label(label)
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .child(div().flex_1().h(px(1.0)).bg(rgb(p.accent)))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(rgb(p.accent))
                    .child(label),
            )
            .child(div().flex_1().h(px(1.0)).bg(rgb(p.accent)))
            .into_any_element()
    }

    /// A centered day divider between messages of different dates.
    fn day_separator(&self, day_ts: &str, p: Palette) -> AnyElement {
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
            .child(div().flex_1().h(px(1.0)).bg(rgb(p.border)))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(p.muted))
                    .child(SharedString::from(pretty_day(day_ts))),
            )
            .child(div().flex_1().h(px(1.0)).bg(rgb(p.border)))
            .into_any_element()
    }

    /// One message row; consecutive same-sender rows within five
    /// minutes group (avatar and header hidden), like the original.
    fn message_row(&self, index: usize, grouped: bool, inputs: &PaneInputs) -> AnyElement {
        self.message_row_view(&self.views[index], grouped, inputs)
    }

    /// The full row builder for any message view (main pane and
    /// threads render the same way).
    pub(crate) fn message_row_view(
        &self,
        view: &MessageView,
        grouped: bool,
        inputs: &PaneInputs,
    ) -> AnyElement {
        let p = inputs.palette;
        let room_id = view.room_id.clone();
        let msg_id = view.id.clone();
        let edit_text = view.text.clone();
        let editing_this = matches!(
            inputs.editing.as_ref(),
            Some((room, msg)) if *room == room_id && *msg == msg_id
        );
        let emoji = &inputs.emoji;
        let me = inputs.me.clone();
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
                    .border_color(rgb(if mine { p.accent } else { p.border }))
                    .bg(rgb(if mine { p.row_hover } else { p.panel }))
                    .text_xs()
                    .text_color(rgb(p.text))
                    .cursor_pointer()
                    .tooltip(move |_, app| {
                        app.new(|_| ReactionTooltip {
                            text: SharedString::from(names.clone()),
                            palette: p,
                        })
                        .into()
                    })
                    .on_click({
                        let this = self.entity.clone();
                        let room_id = room_id.clone();
                        let msg_id = msg_id.clone();
                        let name = name.clone();
                        move |_, _, cx| {
                            this.update(cx, |_, cx| {
                                cx.emit(PaneEvent::ToggleReaction {
                                    room_id: room_id.clone(),
                                    msg_id: msg_id.clone(),
                                    emoji: name.clone(),
                                });
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
            let this = self.entity.clone();
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
                .bg(rgb(p.panel))
                .border_1()
                .border_color(rgb(p.border))
                .text_xs()
                .text_color(rgb(p.muted))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(p.row_hover)))
                .on_click(move |_, _, cx| {
                    this.update(cx, |_, cx| {
                        cx.emit(PaneEvent::OpenThread { tmid: tmid.clone() });
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
                        .text_color(rgb(p.muted))
                        .child(SharedString::from(view.text.clone())),
                )
                .into_any_element();
        }
        // The user card opens from the avatar, the sender name or a
        // mention pill (m12 D3/F8); unknown users fetch their card
        // data on the first open.
        let lead = if grouped {
            div().flex_none().w(rems(2.0)).into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("card-{msg_id}")))
                .debug_selector(|| format!("sender-profile-{msg_id}"))
                .role(Role::Button)
                .aria_label(format!("Open user card for {}", view.sender))
                .flex_none()
                .cursor_pointer()
                .on_click(card_opener(self.entity.clone(), view.sender.clone()))
                .child(match view.avatar.as_deref() {
                    Some(path) => img(std::path::PathBuf::from(path))
                        .flex_none()
                        .size(rems(2.0))
                        .rounded_full()
                        .into_any_element(),
                    None => avatar(&view.sender, p, rems(2.0)).into_any_element(),
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
            // Ungrouped rows breathe one step more; grouped
            // continuations keep the compact lead (deepseek D7).
            .when(!grouped, |row| row.py_1p5())
            .rounded_md()
            .hover(|style| style.bg(rgb(p.row_hover)))
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
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(p.text))
                                        .cursor_pointer()
                                        .hover(|style| style.text_color(rgb(p.accent)))
                                        .on_click(card_opener(
                                            self.entity.clone(),
                                            view.sender.clone(),
                                        ))
                                        .child(SharedString::from(view.sender.clone())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(p.muted))
                                        .child(SharedString::from(view.ts.clone())),
                                )
                                .when(view.edited, |header| {
                                    header.child(
                                        div().text_xs().text_color(rgb(p.muted)).child("(edited)"),
                                    )
                                }),
                        )
                    })
                    .child(message_element(view, inputs.me.as_deref(), &self.entity, p))
                    .children(thread_pill)
                    .children(view.file.as_ref().map(|(name, url, size)| {
                        attachment_card(&msg_id, name, url, *size, view.image_path.as_deref(), p)
                    }))
                    .child(badges),
            )
            .child(action_strip(
                ActionContext {
                    this: self.entity.clone(),
                    link_ready: inputs.link_ready,
                    menu_open: inputs.open_menu.as_ref()
                        == Some(&(room_id.clone(), msg_id.clone())),
                    room_id,
                    msg_id,
                    edit_text,
                    editing_this,
                    starred: view.starred,
                },
                p,
            ))
            .into_any_element()
    }

    /// The virtualized list element; the closure reads only this
    /// entity, never the root (m13 S2a).
    pub(crate) fn element(&self, inputs: &PaneInputs) -> AnyElement {
        let p = inputs.palette;
        // Empty states (m9, m5 finding 21): a closed pane and an
        // open-but-empty room are different states, not blank space.
        if self.rows.is_empty() {
            let text = if self.views_room.is_none() {
                "open a room to see its messages"
            } else {
                "no messages yet"
            };
            return div()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .child(div().text_base().text_color(rgb(p.muted)).child(text))
                .into_any_element();
        }
        let entity = self.entity.clone();
        let inputs = std::rc::Rc::new(inputs.clone());
        // Loading pill (m9, m5 finding 21): fetch-older has a visual.
        let loading_pill = self.loading_older.then(|| {
            div()
                .flex()
                .w_full()
                .items_center()
                .justify_center()
                .py_1()
                .child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.border))
                        .text_xs()
                        .text_color(rgb(p.muted))
                        .child("loading older…"),
                )
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .children(loading_pill)
            .child(
                list(self.messages_list.clone(), move |ix, _window, cx| {
                    let pane = entity.read(cx);
                    let p = inputs.palette;
                    match pane.rows.get(ix) {
                        Some(PaneRow::Separator { day_ts }) => pane.day_separator(day_ts, p),
                        Some(PaneRow::UnreadMarker { view_index }) => {
                            pane.unread_marker(*view_index, p)
                        }
                        Some(PaneRow::Message {
                            view_index,
                            grouped,
                        }) => pane.message_row(*view_index, *grouped, &inputs),
                        None => div().into_any_element(),
                    }
                })
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    /// The paging trigger as a pure query: flips the edge flags and
    /// returns the fetch command; render only forwards it.
    pub(crate) fn poll_paging(&mut self, state: &ChatState) -> Option<ChatCommand> {
        let at_top = self.messages_list.logical_scroll_top().item_ix == 0;
        if !at_top {
            self.paged_since_top = false;
        }
        let filled = state
            .active_room
            .as_deref()
            .is_some_and(|room| state.at_cache_cap(room));
        if at_top
            && !self.paged_since_top
            && !self.loading_older
            && !filled
            && let Some(room) = state.active_room.clone()
            && !state.older_exhausted.contains(&room)
        {
            self.paged_since_top = true;
            self.loading_older = true;
            return Some(ChatCommand::FetchOlder { room_id: room });
        }
        None
    }

    /// A pane view by message id, for the composer's quote card.
    pub(crate) fn view(&self, msg_id: &str) -> Option<&MessageView> {
        self.views.iter().find(|view| view.id == msg_id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;
    use crate::rc::{Msg, Room};
    use gpui::TestAppContext;

    #[test]
    fn row_change_classifies_by_boundary_identity() {
        let (f, l) = (Some("a"), Some("z"));
        // Same boundary views: append at the tail.
        assert_eq!(
            classify_row_change(3, f, l, 4, f, Some("y")),
            RowChange::Appended(1)
        );
        // Same tail view: prepend at the head.
        assert_eq!(
            classify_row_change(3, f, l, 5, Some("p"), l),
            RowChange::Prepended(2)
        );
        // Both boundaries moved: reshape even at the same length.
        assert_eq!(
            classify_row_change(3, f, l, 3, Some("p"), Some("y")),
            RowChange::Reshaped
        );
        // Same length and boundaries: no change.
        assert_eq!(classify_row_change(3, f, l, 3, f, l), RowChange::Same);
        // Shrinks are reshapes (delete) — never classified as growth.
        assert_eq!(
            classify_row_change(4, f, l, 2, Some("p"), Some("y")),
            RowChange::Reshaped
        );
        // Empty to filled: append from a missing old first.
        assert_eq!(
            classify_row_change(0, None, None, 2, Some("a"), Some("b")),
            RowChange::Appended(2)
        );
    }

    #[test]
    fn adjacent_grouped_needs_same_sender_day_and_recency() {
        let base = |sender: &str, day: &str, ts: &str| MessageView {
            id: format!("m-{sender}-{ts}"),
            system: false,
            room_id: "r".to_owned(),
            ts: ts.to_owned(),
            edited: false,
            sender: sender.to_owned(),
            text: String::new(),
            lines: Default::default(),
            reactions: Vec::new(),
            file: None,
            image_path: None,
            avatar: None,
            thread: None,
            starred: false,
            day: day.to_owned(),
            ts_raw: ts.to_owned(),
        };
        let a = base("alice", "2026-11-09", "2026-11-09T10:00:00Z");
        let b = base("alice", "2026-11-09", "2026-11-09T10:04:00Z");
        assert!(adjacent_grouped(&a, &b), "four minutes apart groups");
        let far = base("alice", "2026-11-09", "2026-11-09T10:06:00Z");
        assert!(!adjacent_grouped(&a, &far), "six minutes does not");
        let other = base("bob", "2026-11-09", "2026-11-09T10:01:00Z");
        assert!(!adjacent_grouped(&a, &other), "other sender does not");
        let new_day = base("alice", "2026-11-10", "2026-11-10T09:00:00Z");
        assert!(!adjacent_grouped(&a, &new_day), "new day does not");
        // System rows never group — neither as the previous row nor as
        // the view (m12 F2).
        let mut system_previous = base("alice", "2026-11-09", "2026-11-09T10:01:00Z");
        system_previous.system = true;
        assert!(
            !adjacent_grouped(&system_previous, &b),
            "a system previous row does not group"
        );
        let mut system_view = base("alice", "2026-11-09", "2026-11-09T10:02:00Z");
        system_view.system = true;
        assert!(
            !adjacent_grouped(&a, &system_view),
            "a system view does not group"
        );
    }

    #[test]
    fn within_five_minutes_bounds_the_grouping_window() {
        assert!(
            within_five_minutes("2026-11-09T10:00:00Z", "2026-11-09T10:04:59Z"),
            "just inside the window groups"
        );
        assert!(
            !within_five_minutes("2026-11-09T10:00:00Z", "2026-11-09T10:05:00Z"),
            "the window is half-open: exactly five minutes does not group"
        );
        // One-sided window: `b < a + 5 min` never checks order, so an
        // earlier stamp still groups. Unreachable in the pane, where
        // rows are chronological — pinned as shipped semantics.
        assert!(
            within_five_minutes("2026-11-09T10:00:00Z", "2026-11-09T09:59:59Z"),
            "the window is one-sided: only the upper bound is checked"
        );
        assert!(
            !within_five_minutes("not-a-stamp", "2026-11-09T10:01:00Z"),
            "unparsable stamps never group"
        );
    }

    #[test]
    fn pretty_time_renders_zone_offsets_and_falls_back() {
        let utc = jiff::tz::TimeZone::UTC;
        assert_eq!(pretty_time_in("2026-11-07T20:32:00.514Z", &utc), "8:32 PM");
        let msk = jiff::tz::TimeZone::fixed(jiff::tz::Offset::from_hours(3).unwrap());
        assert_eq!(pretty_time_in("2026-11-07T20:32:00.514Z", &msk), "11:32 PM");
        assert_eq!(pretty_time_in("2026-11-07T00:05:00Z", &utc), "12:05 AM");
        assert_eq!(pretty_time_in("not-a-stamp", &utc), "not-a-stamp");
    }

    const ROOM: &str = "room-a";

    /// Alternating senders: `adjacent_grouped` never fires, so rows
    /// are one message each and the splice arithmetic stays exact.
    fn msg(index: usize) -> Msg {
        Msg {
            id: format!("m{index:04}"),
            room_id: ROOM.to_owned(),
            sender: format!("user{}", index % 2),
            text: format!("message {index}"),
            edited: false,
            ts: format!(
                "2026-11-09T10:{:02}:{:02}.000Z",
                (index / 60) % 60,
                index % 60
            ),
            tmid: None,
            tcount: None,
            tlm: None,
            file: None,
            reactions: Default::default(),
            starred_by_me: false,
            system: false,
        }
    }

    fn seeded(count: usize) -> ChatState {
        let mut state = ChatState::default();
        state.rooms = vec![Room {
            id: ROOM.to_owned(),
            name: "general".to_owned(),
            kind: 'c',
            unread: 0,
            lm: None,
            preview: None,
            topic: None,
            mentions: 0,
        }];
        state.active_room = Some(ROOM.to_owned());
        state
            .messages
            .insert(ROOM.to_owned(), (0..count).map(msg).collect());
        state.message_versions.insert(ROOM.to_owned(), 1);
        state
    }

    fn bump(state: &mut ChatState, room: &str) {
        *state.message_versions.entry(room.to_owned()).or_default() += 1;
    }

    #[gpui::test]
    fn background_traffic_does_not_rebuild_the_pane(cx: &mut TestAppContext) {
        let pane = cx.new(MessagePane::new);
        let mut state = seeded(50);
        pane.update(cx, |pane, _| pane.sync(&state));
        let after_first = pane.update(cx, |pane, _| pane.rebuilds());
        assert_eq!(after_first, 1, "the first sync rebuilds once");

        // A bump for another room must not touch this pane.
        bump(&mut state, "room-b");
        pane.update(cx, |pane, _| pane.sync(&state));
        let still = pane.update(cx, |pane, _| pane.rebuilds());
        assert_eq!(
            still, after_first,
            "background traffic rebuilt the pane (S4 regression)"
        );

        // The active room's own bump does.
        bump(&mut state, ROOM);
        pane.update(cx, |pane, _| pane.sync(&state));
        let now = pane.update(cx, |pane, _| pane.rebuilds());
        assert_eq!(now, after_first + 1);
    }

    #[gpui::test]
    fn tail_follows_appends_and_parking_survives(cx: &mut TestAppContext) {
        let pane = cx.new(MessagePane::new);
        let mut state = seeded(100);
        pane.update(cx, |pane, _| pane.sync(&state));
        let following = pane.update(cx, |pane, _| pane.messages_list.is_following_tail());
        assert!(following);

        state.messages.get_mut(ROOM).unwrap().push(msg(100));
        bump(&mut state, ROOM);
        pane.update(cx, |pane, _| pane.sync(&state));
        let following = pane.update(cx, |pane, _| pane.messages_list.is_following_tail());
        assert!(following, "following must survive an append");

        // Park at the head: the operator scrolled up.
        pane.update(cx, |pane, _| {
            pane.messages_list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            })
        });
        let parked = pane.update(cx, |pane, _| pane.messages_list.is_following_tail());
        assert!(!parked);

        state.messages.get_mut(ROOM).unwrap().push(msg(101));
        bump(&mut state, ROOM);
        pane.update(cx, |pane, _| pane.sync(&state));
        let parked = pane.update(cx, |pane, _| pane.messages_list.is_following_tail());
        assert!(!parked, "parking must survive appends");
    }

    #[gpui::test]
    fn prepend_keeps_the_viewport_anchor(cx: &mut TestAppContext) {
        let pane = cx.new(MessagePane::new);
        let mut state = seeded(300);
        pane.update(cx, |pane, _| pane.sync(&state));
        pane.update(cx, |pane, _| {
            pane.messages_list.scroll_to(ListOffset {
                item_ix: 100,
                offset_in_item: px(2.),
            })
        });

        // 40 older messages, same day: no new separators, so the row
        // delta is exactly 40 and the anchored row shifts by 40.
        let older: Vec<Msg> = (0..40)
            .map(|index| Msg {
                id: format!("old{index:02}"),
                ts: format!("2026-11-09T09:{:02}:00.000Z", index),
                ..msg(0)
            })
            .collect();
        state.messages.get_mut(ROOM).unwrap().splice(0..0, older);
        bump(&mut state, ROOM);
        pane.update(cx, |pane, _| pane.sync(&state));

        let top = pane.update(cx, |pane, _| pane.messages_list.logical_scroll_top());
        assert_eq!(
            top.item_ix, 140,
            "the row under the viewport top must not move"
        );
        assert_eq!(
            top.offset_in_item,
            px(2.),
            "the sub-row offset must survive the prepend"
        );
    }
}
