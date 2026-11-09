//! The GPUI layer: one native Wayland window (DECL0002), the compio event
//! pump, and the chat view. The entity holds the pure [`ChatState`]; render
//! derives from it; no I/O happens on the UI thread.

mod input;
mod markdown;
mod message_pane;
mod root;
mod theme;

use gpui::{
    App, Application, Bounds, Focusable, WindowBounds, WindowOptions, actions, prelude::*, px, size,
};
use gpui_platform::current_platform;

use crate::settings::{AppSettings, SendOnEnter};
use input::{
    Backspace, Copy, Cut, Delete, End, Home, InsertNewline, Left, MoveLeftWord, MoveRightWord,
    Paste, Right, SelectAll, SelectLeft, SelectLeftWord, SelectRight, SelectRightWord,
    ShowCharacterPalette, Submit,
};

actions!(zoom, [ZoomIn, ZoomOut, ZoomReset]);
// Focuses the search strip for a room jump (m12 F3: the Ctrl+K path).
actions!(roomnav, [FocusSearch]);

/// Wayland app-id of the surface.
const APP_ID: &str = "blazer-chat";

pub fn run(settings: AppSettings) {
    // Bootstrap without the platform crate's HTTP client: its
    // `application()` attaches a Reqwest client whose constructor
    // spawns a hidden Tokio runtime — a third executor, against
    // DECL0001 (one owner per layer; all app I/O belongs to compio).
    // The UI never loads network images through gpui: emoji, avatars
    // and attachments land on disk via the compio client first.
    // sendOnEnter bindings (m14 P2, RC MessageBox): scoped to the
    // composer's `Composer` context so the search box keeps its
    // plain semantics. In `normal` every modifier+Enter inserts a
    // newline (row 22); in `alternative` the modifier+Enter keys
    // send — plain Enter and shift-enter branch inside the
    // handlers.
    let enter_bindings: Vec<gpui::KeyBinding> = match settings.composer.send_on_enter {
        SendOnEnter::Normal => ["ctrl-enter", "alt-enter", "super-enter"]
            .map(|key| gpui::KeyBinding::new(key, InsertNewline, Some("Composer")))
            .into(),
        SendOnEnter::Alternative => ["ctrl-enter", "alt-enter", "super-enter"]
            .map(|key| gpui::KeyBinding::new(key, Submit, Some("Composer")))
            .into(),
    };
    let send_on_enter = settings.composer.send_on_enter;
    Application::with_platform(current_platform(false)).run(move |cx: &mut App| {
        let service = crate::app::spawn(settings.server);
        cx.bind_keys(enter_bindings);

        cx.bind_keys([
            gpui::KeyBinding::new("backspace", Backspace, None),
            gpui::KeyBinding::new("delete", Delete, None),
            gpui::KeyBinding::new("left", Left, None),
            gpui::KeyBinding::new("right", Right, None),
            gpui::KeyBinding::new("shift-left", SelectLeft, None),
            gpui::KeyBinding::new("shift-right", SelectRight, None),
            // Word motion: the DOM-textarea Ctrl+arrow convention the
            // operator flagged missing (m14 P0).
            gpui::KeyBinding::new("ctrl-left", MoveLeftWord, None),
            gpui::KeyBinding::new("ctrl-right", MoveRightWord, None),
            gpui::KeyBinding::new("ctrl-shift-left", SelectLeftWord, None),
            gpui::KeyBinding::new("ctrl-shift-right", SelectRightWord, None),
            gpui::KeyBinding::new("ctrl-a", SelectAll, None),
            gpui::KeyBinding::new("ctrl-v", Paste, None),
            gpui::KeyBinding::new("ctrl-c", Copy, None),
            gpui::KeyBinding::new("ctrl-x", Cut, None),
            gpui::KeyBinding::new("home", Home, None),
            gpui::KeyBinding::new("end", End, None),
            gpui::KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
            // The original's most-used shortcut: jump to a room by name
            // (m12 F3). Matches every context; the root handler focuses
            // the search strip.
            gpui::KeyBinding::new("ctrl-k", FocusSearch, None),
            // Browser-style zoom: rem scaling, works anywhere in the UI.
            gpui::KeyBinding::new("ctrl-=", ZoomIn, None),
            gpui::KeyBinding::new("ctrl-+", ZoomIn, None),
            gpui::KeyBinding::new("ctrl--", ZoomOut, None),
            gpui::KeyBinding::new("ctrl-0", ZoomReset, None),
        ]);

        // Full-HD request: niri derives the tiled column width from the
        // requested size's proportion of the view — the client opens as
        // the workspace's main surface.
        let bounds = Bounds::centered(None, size(px(1920.0), px(1050.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // Below ~800px the search field and the room column
                    // collide; keep the layout intact instead.
                    window_min_size: Some(size(px(760.0), px(480.0))),
                    titlebar: Some(gpui::TitlebarOptions {
                        // A debug binary lags visibly and reads as a
                        // regression (m10 incident 2): make the build
                        // profile impossible to miss.
                        title: Some(if cfg!(debug_assertions) {
                            "blazer-chat [DEBUG — unoptimized]".into()
                        } else {
                            "blazer-chat".into()
                        }),
                        ..Default::default()
                    }),
                    app_id: Some(APP_ID.to_owned()),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| root::ChatRoot::new(service, send_on_enter, window, cx)),
            )
            .expect("open the chat window");

        // Closing the last window ends the process; the compio thread is a
        // daemon by construction (the channel close is its shutdown signal).
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        window
            .update(cx, |root, window, cx| {
                window.focus(&root.composer.read(cx).focus_handle(cx), cx);
            })
            .expect("focus the composer");
        cx.activate(true);
    });
}
