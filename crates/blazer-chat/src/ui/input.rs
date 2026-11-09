//! The message composer — vendored from the pinned `gpui 0.2.2` examples
//! (`examples/input.rs`, Apache-2.0, © Zed Industries), extended for
//! multiline input: the editor grows with its content (wrapped rows,
//! capped height, follows the cursor), Shift+Enter inserts a newline,
//! Enter submits, and paste keeps line breaks. Internal editing logic
//! (selection, IME marked ranges, mouse) stays as shipped except where a
//! mapping had to become line-aware.
//!
//! Shaping is cached per (text, font size, wrap width, line height): a
//! frame never reshapes text unless one of those changed, because the
//! window re-renders on every scroll tick and reshaping each frame made
//! scrolling janky.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AvailableSpace, Bounds, ClipboardEntry, ClipboardItem, ContentMask, Context, CursorStyle,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, ImageFormat, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, SharedString, Style, TextAlign,
    TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill, point,
    prelude::*, px, relative, rgb, rgba, size,
};
use unicode_segmentation::*;

use super::theme::palette;

/// The composer shows at most this many text rows; longer content stops
/// growing and scrolls so the cursor stays visible.
const COMPOSER_MAX_ROWS: usize = 8;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        MoveLeftWord,
        MoveRightWord,
        SelectLeftWord,
        SelectRightWord,
        SelectAll,
        Home,
        End,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        InsertNewline,
        Submit,
        WrapBold,
        WrapItalic,
        WrapStrike,
        WrapCode,
        Cancel,
        SuggestNext,
        SuggestPrev,
    ]
);

/// Emitted when the user presses Enter: the composer submits.
pub(crate) enum TextInputEvent {
    Submitted,
    /// Escape pressed: the host decides what backs out (m12 F12).
    Cancelled,
    /// ArrowUp fell through — caret at offset 0, no popup: the host
    /// walks to the previous own editable message (m14 P0).
    EditPrevious,
    /// ArrowDown fell through — caret at the very end, no popup: the
    /// host walks forward or exits the edit (m14 P0).
    EditNext,
    /// An image rode the clipboard: bytes plus the file extension
    /// (m12 F14).
    ImagePasted {
        bytes: Vec<u8>,
        ext: String,
    },
    /// The text changed under the user's fingers: content plus the
    /// caret (m12 F10).
    Changed {
        text: String,
        caret: usize,
    },
    /// The suggestion selection moved (m12 F10): the host re-renders
    /// the popup.
    SuggestMoved,
}

/// Composer suggestions (m12 F10): the token being completed, the
/// candidates and the selected row. State lives in the input so
/// Enter/Tab intercept before submit.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Suggest {
    pub(crate) token_start: usize,
    pub(crate) caret: usize,
    pub(crate) items: Vec<String>,
    pub(crate) selected: usize,
    pub(crate) emoji: bool,
}

/// What a shaping result depends on; equality means the cached wrapped
/// lines are still valid.
#[derive(PartialEq)]
struct ShapeKey {
    text: SharedString,
    font_size: Pixels,
    wrap_width: Option<Pixels>,
}

/// Shaped lines plus everything the mappings need, shared across frames.
struct ShapeEntry {
    key: ShapeKey,
    lines: Vec<WrappedLine>,
    /// Byte offset of each source line within the text — index-parallel
    /// to `lines`.
    line_offsets: Vec<usize>,
    /// Unwrapped byte length of each source line.
    line_lens: Vec<usize>,
    total_rows: usize,
}

impl ShapeEntry {
    fn build(key: ShapeKey, lines: Vec<WrappedLine>) -> Self {
        let line_lens: Vec<usize> = lines.iter().map(WrappedLine::len).collect();
        let total_rows: usize = lines
            .iter()
            .map(line_rows)
            .sum::<usize>()
            .clamp(1, COMPOSER_MAX_ROWS);
        Self {
            line_offsets: line_starts(&key.text),
            key,
            lines,
            line_lens,
            total_rows,
        }
    }
}

pub(crate) struct TextInput {
    focus_handle: FocusHandle,
    /// The Enter semantics (m14 P2); only the message composer
    /// takes the setting — the search box and other inputs keep
    /// `Normal`.
    pub(crate) send_on_enter: crate::settings::SendOnEnter,
    content: SharedString,
    placeholder: SharedString,
    /// The active completion popup; the host computes candidates (m12
    /// F10).
    suggest: Option<Suggest>,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    /// Shared across measure and prepaint so a frame reshapes only when
    /// the key changed; `Rc<RefCell>` because both phases run on the UI
    /// thread and the closure in `request_measured_layout` is `Fn`.
    shape_cache: Rc<RefCell<Option<Arc<ShapeEntry>>>>,
    /// Entry painted last; drives the mouse/IME mappings.
    last_shape: Option<Arc<ShapeEntry>>,
    /// Line height the entry was painted with.
    last_line_height: Pixels,
    /// Vertical scroll applied when content exceeds `COMPOSER_MAX_ROWS`.
    last_scroll: Pixels,
    last_bounds: Option<gpui::Bounds<Pixels>>,
    is_selecting: bool,
}

impl TextInput {
    pub(crate) fn new(placeholder: &str, cx: &mut Context<Self>) -> Self {
        // Modifiers match exactly in gpui, so shift-enter never fires the
        // plain `enter` binding — the two coexist without precedence.
        cx.bind_keys([
            KeyBinding::new("shift-enter", InsertNewline, Some("TextInput")),
            KeyBinding::new("enter", Submit, Some("TextInput")),
            // Format on type over the selection (m12 F11); Escape
            // backs out of an edit (m12 F12).
            KeyBinding::new("ctrl-b", WrapBold, Some("TextInput")),
            KeyBinding::new("ctrl-i", WrapItalic, Some("TextInput")),
            KeyBinding::new("escape", Cancel, Some("TextInput")),
            KeyBinding::new("down", SuggestNext, Some("TextInput")),
            KeyBinding::new("up", SuggestPrev, Some("TextInput")),
        ]);
        Self {
            focus_handle: cx.focus_handle(),
            send_on_enter: crate::settings::SendOnEnter::default(),
            content: "".into(),
            placeholder: placeholder.to_owned().into(),
            suggest: None,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            shape_cache: Rc::new(RefCell::new(None)),
            last_shape: None,
            last_line_height: px(28.),
            last_scroll: px(0.),
            last_bounds: None,
            is_selecting: false,
        }
    }

    /// The current text.
    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    /// Sets the empty-composer hint (the per-room "Message #room",
    /// m12 F12).
    /// Applies the sendOnEnter setting (m14 P2); called once for
    /// the message composer at construction.
    pub(crate) fn set_send_on_enter(&mut self, mode: crate::settings::SendOnEnter) {
        self.send_on_enter = mode;
    }

    pub(crate) fn set_placeholder(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.placeholder.as_ref() != text {
            self.placeholder = text.to_owned().into();
            self.last_shape = None;
            cx.notify();
        }
    }

    /// Installs or clears the completion popup (m12 F10); the host
    /// computes the candidates from the changed text.
    pub(crate) fn set_suggest(&mut self, suggest: Option<Suggest>, cx: &mut Context<Self>) {
        if self.suggest != suggest {
            self.suggest = suggest;
            cx.notify();
        }
    }

    /// The active completion popup, for the host to render (m12 F10).
    pub(crate) fn suggest(&self) -> Option<&Suggest> {
        self.suggest.as_ref()
    }

    /// Inserts text at the caret (the emoji picker's path, m12 F9).
    pub(crate) fn insert_at_caret(&mut self, text: &str, cx: &mut Context<Self>) {
        let at = self.cursor_offset();
        self.content = (self.content[..at].to_owned() + text + &self.content[at..]).into();
        let caret = at + text.len();
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_shape = None;
        self.last_bounds = None;
        self.is_selecting = false;
        let text = self.content.to_string();
        cx.emit(TextInputEvent::Changed { text, caret });
        cx.notify();
    }

    /// Replaces the text, moving the caret to the end. Unlike
    /// [`Self::reset`] this keeps the new content — delegating to
    /// `reset` here wiped it and broke the Edit flow.
    pub(crate) fn set_content(&mut self, text: &str, cx: &mut Context<Self>) {
        self.content = text.to_owned().into();
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_shape = None;
        self.last_bounds = None;
        self.is_selecting = false;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.selected_range.end), cx);
    }

    fn left_word(&mut self, _: &MoveLeftWord, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_word_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right_word(&mut self, _: &MoveRightWord, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_word_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left_word(&mut self, _: &SelectLeftWord, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    fn select_right_word(&mut self, _: &SelectRightWord, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word_boundary(self.selected_range.end), cx);
    }

    /// Start of the word before `from`: word runs are alphanumeric
    /// plus underscore; skip the trailing gap, then the run.
    fn previous_word_boundary(&self, from: usize) -> usize {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let chars: Vec<(usize, char)> = self
            .content
            .char_indices()
            .take_while(|(index, _)| *index < from)
            .collect();
        let mut position = chars.len();
        while position > 0 && !is_word(chars[position - 1].1) {
            position -= 1;
        }
        while position > 0 && is_word(chars[position - 1].1) {
            position -= 1;
        }
        chars.get(position).map(|(offset, _)| *offset).unwrap_or(0)
    }

    /// End of the word after `from`: skip the leading gap, then the
    /// run (DOM-textarea Ctrl+Right convention).
    fn next_word_boundary(&self, from: usize) -> usize {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let chars: Vec<(usize, char)> = self
            .content
            .char_indices()
            .skip_while(|(index, _)| *index < from)
            .collect();
        let mut taken = 0;
        while taken < chars.len() && !is_word(chars[taken].1) {
            taken += 1;
        }
        while taken < chars.len() && is_word(chars[taken].1) {
            taken += 1;
        }
        chars
            .get(taken)
            .map(|(offset, _)| *offset)
            .unwrap_or(self.content.len())
    }

    /// Caret to offset 0 (the ArrowDown edit walk lands here).
    pub(crate) fn caret_to_start(&mut self, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;

        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        if let Some(text) = item.text() {
            // Line breaks survive pasting — multiline messages are the
            // point of a growing composer.
            self.replace_text_in_range(None, &text, window, cx);
            return;
        }
        // An image on the clipboard rides the upload path (m12 F14):
        // the bytes go to the app, the attach flow does the upload.
        let Some(image) = item.entries.iter().find_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(image),
            _ => None,
        }) else {
            return;
        };
        let ext = match image.format {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Webp => "webp",
            ImageFormat::Gif => "gif",
            _ => return,
        };
        cx.emit(TextInputEvent::ImagePasted {
            bytes: image.bytes.clone(),
            ext: ext.to_owned(),
        });
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.suggest = None;
        cx.emit(TextInputEvent::Cancelled);
    }

    fn wrap_bold(&mut self, _: &WrapBold, _: &mut Window, cx: &mut Context<Self>) {
        // RC bold is a single `*` (markdown.rs) — `**` rendered literally.
        self.apply_wrap("*", cx);
    }

    fn wrap_italic(&mut self, _: &WrapItalic, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_wrap("_", cx);
    }

    fn wrap_strike(&mut self, _: &WrapStrike, _: &mut Window, cx: &mut Context<Self>) {
        // RC strike is a single `~` (markdown.rs).
        self.apply_wrap("~", cx);
    }

    fn wrap_code(&mut self, _: &WrapCode, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_wrap("`", cx);
    }

    /// Wraps the selection (or inserts the pair at the caret) in a
    /// markdown marker (m12 F11).
    pub(crate) fn apply_wrap(&mut self, marker: &str, cx: &mut Context<Self>) {
        let (text, caret) = wrap_with_marker(&self.content, self.selected_range.clone(), marker);
        self.set_content(&text, cx);
        self.selected_range = caret..caret;
    }

    fn insert_newline(&mut self, _: &InsertNewline, window: &mut Window, cx: &mut Context<Self>) {
        // sendOnEnter=alternative: shift-enter is the send
        // keystroke (m14 P2) — the modifier keys route here only
        // in `normal`.
        if self.send_on_enter == crate::settings::SendOnEnter::Alternative {
            return self.submit_or_accept(cx);
        }
        self.insert_newline_only(window, cx);
    }

    fn insert_newline_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_range.is_some() {
            return;
        }
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        // Mouse and IME offsets are derived from the painted text, which
        // is the placeholder while the content is empty; an unclamped
        // offset here made the next replace slice out of bounds (the
        // crash on first click into an empty field).
        let offset = offset.min(self.content.len());
        self.selected_range = offset..offset;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        // The painted entry shapes the PLACEHOLDER while the content is
        // empty — offsets derived from it are indices into a string the
        // content does not have. No content, no caret offset.
        if self.content.is_empty() {
            return 0;
        }
        let Some(shape) = self.last_shape.as_ref() else {
            return 0;
        };
        if shape.key.text.is_empty() {
            return 0;
        }

        let Some(bounds) = self.last_bounds.as_ref() else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }

        // Walk the painted lines top-down, translating the window
        // position into each line's own coordinate space.
        let line_height = self.last_line_height;
        let mut row_top = bounds.top() - self.last_scroll;
        let mut bytes_before = 0usize;
        for (index, line) in shape.lines.iter().enumerate() {
            let is_last = bytes_before + shape.line_lens[index] >= shape.key.text.len();
            let height = line_height * line_rows(line);
            if position.y < row_top + height || is_last {
                let local = line
                    .closest_index_for_position(
                        point(position.x - bounds.left(), position.y - row_top),
                        line_height,
                    )
                    .unwrap_or_else(|closest| closest);
                return (bytes_before + local).min(self.content.len());
            }
            row_top += height;
            bytes_before += shape.line_lens[index] + 1;
        }
        self.content.len()
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        // sendOnEnter=alternative (RC MessageBox inverted roles,
        // m14 P2): plain Enter inserts a newline; the send
        // keystrokes are the modifier variants (ctrl/alt/super are
        // bound to Submit in the Composer context, shift-enter
        // branches in `insert_newline`). The suggestion popup
        // still accepts first.
        if self.send_on_enter == crate::settings::SendOnEnter::Alternative && self.suggest.is_none()
        {
            return self.insert_newline_only(window, cx);
        }
        self.submit_or_accept(cx);
    }

    /// The shared send path: a suggestion up means Enter accepts it
    /// (m12 F10); IME composition commits instead of sending;
    /// whitespace-only content is not a message either (the send
    /// path trims, an empty command would still hit the server).
    fn submit_or_accept(&mut self, cx: &mut Context<Self>) {
        if self.suggest.is_some() {
            self.accept_suggestion(cx);
            return;
        }
        if self.marked_range.is_some() || !self.content.trim().is_empty() {
            cx.emit(TextInputEvent::Submitted);
        }
    }

    /// Replaces the token with the selected suggestion (m12 F10).
    pub(crate) fn accept_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(suggest) = self.suggest.take() else {
            return;
        };
        let Some(item) = suggest.items.get(suggest.selected) else {
            return;
        };
        let replacement = if suggest.emoji {
            format!(":{item}: ")
        } else {
            format!("{item} ")
        };
        let caret = suggest.token_start + replacement.len();
        self.content = (self.content[..suggest.token_start].to_owned()
            + &replacement
            + &self.content[suggest.caret.min(self.content.len())..])
            .into();
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_shape = None;
        self.last_bounds = None;
        self.is_selecting = false;
        let text = self.content.to_string();
        cx.emit(TextInputEvent::Changed { text, caret });
        cx.notify();
    }

    /// Clicks a popup chip: select it, then accept (m12 F10).
    pub(crate) fn pick_suggestion(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(suggest) = &mut self.suggest {
            suggest.selected = index.min(suggest.items.len().saturating_sub(1));
        }
        self.accept_suggestion(cx);
    }

    fn suggest_next(&mut self, _: &SuggestNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(suggest) = &mut self.suggest {
            // Wrap-around, like the original's popup
            // (useComposerBoxPopup): first -> last, no clamping.
            suggest.selected = if suggest.items.is_empty() {
                0
            } else {
                (suggest.selected + 1) % suggest.items.len()
            };
            cx.emit(TextInputEvent::SuggestMoved);
            cx.notify();
        } else if self.selected_range.is_empty() && self.cursor_offset() == self.content.len() {
            // No popup and the caret sits at the very end: walk the
            // own messages forward (RC MessageBox ArrowDown).
            cx.emit(TextInputEvent::EditNext);
        }
    }

    fn suggest_prev(&mut self, _: &SuggestPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(suggest) = &mut self.suggest {
            // Wrap-around: last -> first.
            suggest.selected = if suggest.items.is_empty() {
                0
            } else {
                (suggest.selected + suggest.items.len() - 1) % suggest.items.len()
            };
            cx.emit(TextInputEvent::SuggestMoved);
            cx.notify();
        } else if self.selected_range.is_empty() && self.cursor_offset() == 0 {
            // No popup and the caret sits at the very start: edit the
            // previous own message (RC MessageBox ArrowUp).
            cx.emit(TextInputEvent::EditPrevious);
        }
    }

    pub(crate) fn reset(&mut self, cx: &mut Context<Self>) {
        self.suggest = None;
        self.content = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_shape = None;
        self.last_bounds = None;
        self.is_selecting = false;
        cx.notify();
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = safe_range(
            range_utf16
                .as_ref()
                .map(|range_utf16| self.range_from_utf16(range_utf16))
                .or(self.marked_range.clone())
                .unwrap_or(self.selected_range.clone()),
            self.content.len(),
        );

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        let text = self.content.to_string();
        let caret = self.selected_range.end;
        cx.emit(TextInputEvent::Changed { text, caret });
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = safe_range(
            range_utf16
                .as_ref()
                .map(|range_utf16| self.range_from_utf16(range_utf16))
                .or(self.marked_range.clone())
                .unwrap_or(self.selected_range.clone()),
            self.content.len(),
        );

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: gpui::Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let start = self.painted_position(range.start)?;
        let end = self.painted_position(range.end)?;
        Some(Bounds::from_corners(
            point(
                bounds.left() + start.x,
                bounds.top() + start.y - self.last_scroll,
            ),
            point(
                bounds.left() + end.x,
                bounds.top() + end.y - self.last_scroll + self.last_line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_mouse_position(point)))
    }
}

impl TextInput {
    /// Painted offset of a byte index within the last painted entry, in
    /// element coordinates (before scroll). `None` without a prior paint.
    fn painted_position(&self, offset: usize) -> Option<Point<Pixels>> {
        let shape = self.last_shape.as_ref()?;
        let (line_index, local) = locate_offset(&shape.line_offsets, &shape.line_lens, offset);
        let line = shape.lines.get(line_index)?;
        let position = line.position_for_index(local, self.last_line_height)?;
        Some(point(
            position.x,
            position.y + self.last_line_height * rows_before(&shape.lines, line_index),
        ))
    }
}

/// A selection range clamped into `len`: mouse and IME offsets are
/// derived from the painted text (the placeholder while empty), so a
/// stale or hostile range must degrade to a valid one instead of
/// panicking the string slice (crash on first click, m12-era).
fn safe_range(range: Range<usize>, len: usize) -> Range<usize> {
    let start = range.start.min(len);
    let end = range.end.min(len);
    start.min(end)..start.max(end)
}

/// Wrapped rows a shaped source line occupies.
fn line_rows(line: &WrappedLine) -> usize {
    1 + line.wrap_boundaries().len()
}

/// Byte offset of every source line — mirrors the `split('\n')` split
/// `shape_text` performs, including the trailing empty line after a
/// final newline.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = 0usize;
    for piece in text.split('\n') {
        starts.push(offset);
        offset += piece.len() + 1;
    }
    starts
}

/// `(line index, local byte offset)` for a global offset: the first line
/// whose range reaches `offset`, so end-of-line and start-of-next both
/// resolve predictably.
fn locate_offset(line_starts: &[usize], line_lens: &[usize], offset: usize) -> (usize, usize) {
    for (index, (start, len)) in line_starts.iter().zip(line_lens).enumerate() {
        if offset <= start + *len {
            return (index, offset - start);
        }
    }
    (
        line_starts.len().saturating_sub(1),
        line_lens.last().copied().unwrap_or(0),
    )
}

/// Row count actually shown for `rows` of content.
fn visible_rows(rows: usize) -> usize {
    rows.clamp(1, COMPOSER_MAX_ROWS)
}

/// Rows scrolled past so the cursor row stays inside the visible window.
fn scroll_rows(cursor_row: usize, visible_rows: usize) -> usize {
    cursor_row.saturating_sub(visible_rows.saturating_sub(1))
}

/// Rows of `lines` before line `index`.
fn rows_before(lines: &[WrappedLine], index: usize) -> usize {
    lines.iter().take(index).map(line_rows).sum()
}

/// Shaped lines for `key`, through the shared cache: a key miss shapes
/// once and stores; a hit reuses the previous frame's lines untouched.
fn ensure_shaped(
    cache: &RefCell<Option<Arc<ShapeEntry>>>,
    key: ShapeKey,
    window: &mut Window,
) -> Arc<ShapeEntry> {
    {
        let slot = cache.borrow();
        if let Some(entry) = slot.as_ref()
            && entry.key == key
        {
            return entry.clone();
        }
    }
    let style = window.text_style();
    let font_size = key.font_size;
    let run = TextRun {
        len: key.text.len(),
        font: style.font(),
        color: style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let lines: Vec<WrappedLine> = window
        .text_system()
        .shape_text(key.text.clone(), font_size, &[run], key.wrap_width, None)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let entry = Arc::new(ShapeEntry::build(key, lines));
    *cache.borrow_mut() = Some(entry.clone());
    entry
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    shape: Arc<ShapeEntry>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    scroll: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let input = self.input.clone();
        let cache = input.read(cx).shape_cache.clone();
        let line_height = window.line_height();
        // The element is as tall as its wrapped content, capped at
        // COMPOSER_MAX_ROWS — measured from the cached shaping because
        // the row count depends on both the text and the width.
        (
            window.request_measured_layout(style, move |known, available, window, cx| {
                let input = input.read(cx);
                let display = if input.content.is_empty() {
                    input.placeholder.clone()
                } else {
                    input.content.clone()
                };
                // Taffy's flex-basis pass hands us degenerate widths (Some(0)
                // while the box is still basis-sized); wrapping at one would
                // blow the measured height up per character and collapse the
                // row. Below a floor, measure the single unwrapped row and let
                // the style width fill the parent instead.
                let wrap_width = known
                    .width
                    .or(match available.width {
                        AvailableSpace::Definite(width) => Some(width),
                        _ => None,
                    })
                    .filter(|width| *width >= px(120.));
                let font_size = window.text_style().font_size.to_pixels(window.rem_size());
                let key = ShapeKey {
                    text: display,
                    font_size,
                    wrap_width,
                };
                let shape = ensure_shaped(&cache, key, window);
                size(
                    known.width.unwrap_or_default(),
                    line_height * shape.total_rows,
                )
            }),
            (),
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let marked_range = input.marked_range.clone();
        let style = window.text_style();

        // Placeholder at full opacity but muted: the vendored example's
        // 20%-black is unreadable on the panel (contrast finding).
        let p = palette(window.appearance());
        let (display_text, text_color) = if input.content.is_empty() {
            (input.placeholder.clone(), rgb(p.muted).into())
        } else {
            (input.content.clone(), style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect::<Vec<_>>()
        } else {
            vec![run]
        };

        // The IME underline needs different runs than the cached entry
        // was shaped with; only shape with decoration when composing.
        let shape = if marked_range.is_some() {
            let font_size = style.font_size.to_pixels(window.rem_size());
            let lines: Vec<WrappedLine> = window
                .text_system()
                .shape_text(
                    display_text.clone(),
                    font_size,
                    &runs,
                    Some(bounds.size.width),
                    None,
                )
                .unwrap_or_default()
                .into_iter()
                .collect();
            Arc::new(ShapeEntry::build(
                ShapeKey {
                    text: display_text.clone(),
                    font_size,
                    wrap_width: Some(bounds.size.width),
                },
                lines,
            ))
        } else {
            let font_size = style.font_size.to_pixels(window.rem_size());
            ensure_shaped(
                &input.shape_cache,
                ShapeKey {
                    text: display_text,
                    font_size,
                    wrap_width: Some(bounds.size.width),
                },
                window,
            )
        };

        let line_height = window.line_height();
        let visible = visible_rows(shape.total_rows);

        // Cursor position and the follow-cursor scroll.
        let mut scroll = px(0.);
        let mut cursor_quad = None;
        let (cursor_line, cursor_local) =
            locate_offset(&shape.line_offsets, &shape.line_lens, cursor);
        if let Some(line) = shape.lines.get(cursor_line)
            && let Some(position) = line.position_for_index(cursor_local, line_height)
        {
            let row_within = (position.y.to_f64() / line_height.to_f64()).round() as usize;
            let cursor_row = rows_before(&shape.lines, cursor_line) + row_within;
            scroll = line_height * scroll_rows(cursor_row, visible);
            let y_before = line_height * rows_before(&shape.lines, cursor_line);
            cursor_quad = Some(fill(
                Bounds::new(
                    point(
                        bounds.left() + position.x,
                        bounds.top() + y_before + position.y - scroll,
                    ),
                    size(px(2.), line_height),
                ),
                rgb(p.accent),
            ));
        }

        let selection = selection_quads(&shape, &selected_range, bounds, line_height, scroll);

        PrepaintState {
            shape,
            cursor: cursor_quad,
            selection,
            scroll,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let line_height = window.line_height();
        let selection = std::mem::take(&mut prepaint.selection);
        let cursor = prepaint.cursor.take();
        let scroll = prepaint.scroll;
        let lines = &prepaint.shape.lines;

        // Wrapped rows overflowing the capped height must not paint over
        // neighboring UI, so everything clips to the element bounds.
        window.with_content_mask(
            Some(ContentMask {
                bounds,
                corner_radii: Default::default(),
            }),
            |window| {
                let mut line_top = bounds.top() - scroll;
                for line in lines {
                    if let Err(error) = line.paint(
                        point(bounds.left(), line_top),
                        line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    ) {
                        eprintln!("blazer-chat: composer line paint failed: {error}");
                    }
                    line_top += line_height * line_rows(line);
                }
                for quad in selection {
                    window.paint_quad(quad);
                }
                if focus_handle.is_focused(window)
                    && let Some(cursor) = cursor
                {
                    window.paint_quad(cursor);
                }
            },
        );

        self.input.update(cx, |input, _cx| {
            input.last_shape = Some(prepaint.shape.clone());
            input.last_line_height = line_height;
            input.last_scroll = scroll;
            input.last_bounds = Some(bounds);
        });
    }
}

/// Selection rectangles, one per wrapped row the selection touches.
fn selection_quads(
    shape: &ShapeEntry,
    selected_range: &Range<usize>,
    bounds: gpui::Bounds<Pixels>,
    line_height: Pixels,
    scroll: Pixels,
) -> Vec<PaintQuad> {
    if selected_range.is_empty() {
        return Vec::new();
    }
    let mut quads = Vec::new();
    let mut bytes_before = 0usize;
    let mut line_top = bounds.top() - scroll;
    for line in &shape.lines {
        let len = line.len();
        let start = selected_range.start.saturating_sub(bytes_before).min(len);
        let end = selected_range.end.saturating_sub(bytes_before).min(len);
        if start < end {
            let width = line.size(line_height).width;
            // Byte index where each wrapped row begins.
            let mut row_starts = vec![0usize];
            for boundary in line.wrap_boundaries() {
                if let Some(glyph) = line
                    .runs()
                    .get(boundary.run_ix)
                    .and_then(|run| run.glyphs.get(boundary.glyph_ix))
                {
                    row_starts.push(glyph.index);
                }
            }
            row_starts.push(len);
            row_starts.dedup();

            for (row_ix, pair) in row_starts.windows(2).enumerate() {
                let (row_start, row_end) = (pair[0], pair[1]);
                let rs = start.clamp(row_start, row_end);
                let re = end.clamp(row_start, row_end);
                if rs >= re {
                    continue;
                }
                let x0 = if rs == row_start {
                    px(0.)
                } else {
                    line.position_for_index(rs, line_height)
                        .map(|position| position.x)
                        .unwrap_or_default()
                };
                let x1 = if re == len {
                    line.position_for_index(re, line_height)
                        .map(|position| position.x)
                        .unwrap_or(width)
                } else if re == row_end {
                    // Selected through the wrap boundary: paint the row's
                    // full width (the overshoot on a short row reads as
                    // the row being covered, which it is).
                    width
                } else {
                    line.position_for_index(re, line_height)
                        .map(|position| position.x)
                        .unwrap_or(width)
                };
                let y = line_top + line_height * row_ix;
                quads.push(fill(
                    Bounds::from_corners(
                        point(bounds.left() + x0, y),
                        point(bounds.left() + x1.max(x0), y + line_height),
                    ),
                    rgba(0x3311ff30),
                ));
            }
        }
        line_top += line_height * line_rows(line);
        bytes_before += len + 1;
    }
    quads
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(window.appearance());
        div()
            .flex()
            .w_full()
            .key_context("TextInput")
            // Tab accepts the focused suggestion only when a popup is
            // open; otherwise the key falls through to focus
            // navigation (m14 P0 tail).
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    if event.keystroke.key == "tab"
                        && event.keystroke.modifiers == gpui::Modifiers::default()
                        && this.suggest.is_some()
                    {
                        this.accept_suggestion(cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::left_word))
            .on_action(cx.listener(Self::right_word))
            .on_action(cx.listener(Self::select_left_word))
            .on_action(cx.listener(Self::select_right_word))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::insert_newline))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::wrap_bold))
            .on_action(cx.listener(Self::wrap_italic))
            .on_action(cx.listener(Self::wrap_strike))
            .on_action(cx.listener(Self::wrap_code))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::suggest_next))
            .on_action(cx.listener(Self::suggest_prev))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .bg(rgb(p.panel))
            .line_height(px(28.))
            .text_size(px(14.))
            .text_color(rgb(p.text))
            .child(
                div()
                    .w_full()
                    .p(px(4.))
                    .bg(rgb(p.panel))
                    .child(TextElement { input: cx.entity() }),
            )
    }
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Wraps `text[range]` in a marker pair; returns the new text and the
/// caret past the wrapped span (m12 F11).
fn wrap_with_marker(text: &str, range: std::ops::Range<usize>, marker: &str) -> (String, usize) {
    let mut start = range.start.min(text.len());
    let mut end = range.end.clamp(start, text.len());
    // Mouse and keyboard offsets are char boundaries, but a clamped
    // stale range may not be: snap before slicing.
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    // Toggle-off (m14 P1): unwrap when the selection covers or hugs
    // a full marker pair; keep the selection over the freed text.
    let selected = &text[start..end];
    if start >= marker.len()
        && text.get(start - marker.len()..start) == Some(marker)
        && text.get(end..end + marker.len()) == Some(marker)
    {
        let mut out = String::with_capacity(text.len() - marker.len() * 2);
        out.push_str(&text[..start - marker.len()]);
        out.push_str(&text[start..end]);
        out.push_str(&text[end + marker.len()..]);
        let caret = start - marker.len() + (end - start);
        return (out, caret);
    }
    if selected.len() >= marker.len() * 2
        && selected.starts_with(marker)
        && selected.ends_with(marker)
    {
        let mut out = String::with_capacity(text.len() - marker.len() * 2);
        out.push_str(&text[..start]);
        out.push_str(&selected[marker.len()..selected.len() - marker.len()]);
        out.push_str(&text[end..]);
        let caret = start + selected.len() - marker.len() * 2;
        return (out, caret);
    }
    let mut out = String::with_capacity(text.len() + marker.len() * 2);
    out.push_str(&text[..start]);
    out.push_str(marker);
    out.push_str(&text[start..end]);
    out.push_str(marker);
    out.push_str(&text[end..]);
    let caret = if end > start {
        start + marker.len() + (end - start) + marker.len()
    } else {
        // An empty selection: the caret sits between the inserted pair.
        start + marker.len()
    };
    (out, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_lens(text: &str) -> Vec<usize> {
        text.split('\n').map(|piece| piece.len()).collect()
    }

    #[test]
    fn wrap_with_marker_unwraps_when_already_wrapped() {
        // The selection covers the whole wrapped span.
        let (text, caret) = wrap_with_marker("*hello*", 0..7, "*");
        assert_eq!(text, "hello");
        assert_eq!(caret, 5);
        // The selection hugs the inner text; both markers surround it.
        let (text, caret) = wrap_with_marker("a *b* c", 3..4, "*");
        assert_eq!(text, "a b c");
        assert_eq!(caret, 3);
        // Unwrapped text still wraps (single `*`, RC marker).
        let (text, _) = wrap_with_marker("*a* b", 4..5, "*");
        assert_eq!(text, "*a* *b*");
    }

    #[test]
    fn wrap_uses_rc_markers() {
        // Ctrl+B must produce what markdown.rs renders (RC: single `*`).
        let (text, _) = wrap_with_marker("hello world", 0..11, "*");
        assert_eq!(text, "*hello world*");
        let (text, _) = wrap_with_marker("oops", 0..4, "~");
        assert_eq!(text, "~oops~");
    }

    #[test]
    fn line_starts_maps_newlines() {
        assert_eq!(line_starts("abc"), vec![0]);
        assert_eq!(line_starts("a\nb"), vec![0, 2]);
        assert_eq!(line_starts("a\nbb\nccc"), vec![0, 2, 5]);
    }

    #[test]
    fn line_starts_handles_trailing_newline() {
        // "a\n" shapes as "a" plus an empty final line — the cursor sits
        // there after Shift+Enter.
        assert_eq!(line_starts("a\n"), vec![0, 2]);
        assert_eq!(line_lens("a\n"), vec![1, 0]);
    }

    #[test]
    fn locate_maps_offsets_across_lines() {
        let starts = line_starts("a\nbcd\ne");
        let lens = line_lens("a\nbcd\ne");
        assert_eq!(locate_offset(&starts, &lens, 0), (0, 0));
        assert_eq!(locate_offset(&starts, &lens, 1), (0, 1));
        // End-of-line resolves to that line; one past is the next line.
        assert_eq!(locate_offset(&starts, &lens, 1 + 1), (1, 0));
        assert_eq!(locate_offset(&starts, &lens, 4), (1, 2));
        // Offset 7 is past `e` — end of the last line.
        assert_eq!(locate_offset(&starts, &lens, 7), (2, 1));
    }

    #[test]
    fn locate_clamps_past_the_end() {
        let starts = line_starts("a\nb");
        let lens = line_lens("a\nb");
        assert_eq!(locate_offset(&starts, &lens, 99), (1, 1));
    }

    #[test]
    fn visible_rows_clamps_both_ways() {
        assert_eq!(visible_rows(0), 1);
        assert_eq!(visible_rows(3), 3);
        assert_eq!(visible_rows(50), COMPOSER_MAX_ROWS);
    }

    #[test]
    fn scroll_follows_cursor_only_past_bottom() {
        assert_eq!(scroll_rows(0, 8), 0);
        assert_eq!(scroll_rows(7, 8), 0);
        assert_eq!(scroll_rows(8, 8), 1);
        assert_eq!(scroll_rows(20, 8), 13);
        // A short composer still pins its cursor into view.
        assert_eq!(scroll_rows(3, 2), 2);
    }

    #[test]
    fn multiline_content_shapes_into_growing_height() {
        // Geometry contract behind the auto-grow: N source lines are N
        // row starts, so the measured height grows with newlines until
        // the cap. This pins the mapping the element measures from.
        let one_line = line_starts("hello");
        let two_lines = line_starts("hello\nworld");
        assert_eq!(one_line.len(), 1);
        assert_eq!(two_lines.len(), 2);
        assert_eq!(visible_rows(two_lines.len()), 2);
    }

    #[test]
    fn safe_range_degrades_stale_caret_offsets() {
        // The crash: content empty, a caret offset from the 9-byte
        // placeholder.
        assert_eq!(safe_range(9..9, 0), 0..0);
        assert_eq!(safe_range(3..9, 0), 0..0);
        // Normal selections survive untouched.
        assert_eq!(safe_range(2..7, 10), 2..7);
        // An end past the content clamps to it; start wins the order.
        assert_eq!(safe_range(4..99, 6), 4..6);
        // A reversed (malformed) range still degrades to a valid span.
        assert_eq!(safe_range(Range { start: 99, end: 4 }, 6), 4..6);
        assert_eq!(safe_range(99..99, 6), 6..6);
    }

    #[test]
    fn wrap_over_selection_and_caret() {
        let (text, caret) = wrap_with_marker("hello world", 6..11, "**");
        assert_eq!(text, "hello **world**");
        assert_eq!(caret, 15, "the caret lands past the closing marker");
        let (text, caret) = wrap_with_marker("ab", 2..2, "_");
        assert_eq!(text, "ab__", "an empty selection inserts the pair");
        assert_eq!(caret, 3, "the caret sits between the markers");
        let (text, _) = wrap_with_marker("кирлица", 0..14, "`");
        assert_eq!(text, "`кирлица`", "byte offsets slice on boundaries");
        let (text, _) = wrap_with_marker("abc", 1..12, "**");
        assert_eq!(text, "a**bc**", "out-of-range ends clamp to the text");
    }
}
