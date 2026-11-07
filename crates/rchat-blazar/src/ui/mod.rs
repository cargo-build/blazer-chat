//! The GPUI layer: one native Wayland window (DECL0002), the compio event
//! pump, and the chat view. The entity holds the pure [`ChatState`]; render
//! derives from it; no I/O happens on the UI thread.

mod input;
mod markdown;
mod root;

use gpui::{App, Bounds, Focusable, WindowBounds, WindowOptions, actions, prelude::*, px, size};
use gpui_platform::application;

use crate::rc::RcSettings;
use input::{
    Backspace, Copy, Cut, Delete, End, Home, Left, Paste, Right, SelectAll, SelectLeft,
    SelectRight, ShowCharacterPalette,
};

actions!(zoom, [ZoomIn, ZoomOut, ZoomReset]);

/// Wayland app-id of the surface.
const APP_ID: &str = "rchat-blazar";

pub fn run(settings: RcSettings) {
    application().run(|cx: &mut App| {
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
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("rchat-blazar".into()),
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
