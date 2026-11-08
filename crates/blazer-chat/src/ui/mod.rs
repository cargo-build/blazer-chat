//! The GPUI layer: one native Wayland window (DECL0002), the compio event
//! pump, and the chat view. The entity holds the pure [`ChatState`]; render
//! derives from it; no I/O happens on the UI thread.

mod input;
mod markdown;
mod root;

use gpui::{
    App, Application, Bounds, Focusable, WindowBounds, WindowOptions, actions, prelude::*, px, size,
};
use gpui_platform::current_platform;

use crate::rc::RcSettings;
use input::{
    Backspace, Copy, Cut, Delete, End, Home, Left, Paste, Right, SelectAll, SelectLeft,
    SelectRight, ShowCharacterPalette,
};

actions!(zoom, [ZoomIn, ZoomOut, ZoomReset]);
// Focuses the search strip for a room jump (m12 F3: the Ctrl+K path).
actions!(roomnav, [FocusSearch]);

/// Wayland app-id of the surface.
const APP_ID: &str = "blazer-chat";

pub fn run(settings: RcSettings) {
    // Bootstrap without the platform crate's HTTP client: its
    // `application()` attaches a Reqwest client whose constructor
    // spawns a hidden Tokio runtime — a third executor, against
    // DECL0001 (one owner per layer; all app I/O belongs to compio).
    // The UI never loads network images through gpui: emoji, avatars
    // and attachments land on disk via the compio client first.
    Application::with_platform(current_platform(false)).run(|cx: &mut App| {
        let service = crate::app::spawn(settings);

        cx.bind_keys([
            gpui::KeyBinding::new("backspace", Backspace, None),
            gpui::KeyBinding::new("delete", Delete, None),
            gpui::KeyBinding::new("left", Left, None),
            gpui::KeyBinding::new("right", Right, None),
            gpui::KeyBinding::new("shift-left", SelectLeft, None),
            gpui::KeyBinding::new("shift-right", SelectRight, None),
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
                        title: Some("blazer-chat".into()),
                        ..Default::default()
                    }),
                    app_id: Some(APP_ID.to_owned()),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| root::ChatRoot::new(service, cx)),
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
