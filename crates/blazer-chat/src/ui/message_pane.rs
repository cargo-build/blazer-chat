//! The message pane's row model and pure helpers: render views, the
//! row enum, grouping and display shared by the main pane and the
//! thread and pinned slots.

use gpui::{
    Anchor, ClipboardItem, Entity, FontWeight, Hsla, SharedString, Window, anchored, deferred, div,
    hsla, img, prelude::*, px, rems, rgb, white,
};

use super::input::{ACCENT, BORDER, MUTED, PANEL, ROW_HOVER, TEXT};
use super::markdown::Span;
use super::root::ChatRoot;
use crate::app::ChatCommand;

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
/// usernames too).
pub(crate) fn avatar(sender: &str) -> gpui::Div {
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

pub(crate) fn message_element(
    view: &MessageView,
    me: Option<&str>,
    this: &Entity<ChatRoot>,
) -> gpui::Div {
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
pub(crate) fn span_element(
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

/// The click handler opening a user's card — avatars, sender names
/// and mention pills (m12 D3/F8); unknown users fetch their card
/// data on the first open.
pub(crate) fn card_opener(
    this: Entity<ChatRoot>,
    name: String,
) -> impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
        this.update(cx, |chat, cx| {
            chat.user_card = Some(name.clone());
            if !chat.state.user_info.contains_key(&name) {
                chat.send_command(ChatCommand::UserInfo {
                    username: name.clone(),
                });
            }
            cx.notify();
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
    pub(crate) this: Entity<ChatRoot>,
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
pub(crate) fn action_strip(actions: ActionContext) -> gpui::Div {
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
                chat.send_command(ChatCommand::OpenThread { tmid: tmid.clone() });
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
                                chat.send_command(ChatCommand::OpenThread { tmid: tmid.clone() });
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
}
