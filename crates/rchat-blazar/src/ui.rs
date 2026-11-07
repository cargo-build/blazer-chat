//! The GPUI layer: one native Wayland window whose entity is fed by
//! compio-side events (Wayland native, never XWayland).
//!
//! The entity never touches compio types beyond the channel handles: events
//! arrive on the foreground executor through a `WeakEntity` pump, and clicks
//! push [`Fetch`] requests back. The channel never blocks the frame.

use std::sync::{Arc, Mutex};

use gpui::{
    App, Application, Bounds, Context, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

use crate::service::{self, Fetch, Service, ServiceEvent, UiSnapshot};

/// Wayland app-id of the M0 surface.
const APP_ID: &str = "rchat-blazar";

const BG: u32 = 0x11151c;
const PANEL: u32 = 0x1a212b;
const BORDER: u32 = 0x394354;
const TEXT: u32 = 0xe8eaed;
const MUTED: u32 = 0x93a4bc;
const ACCENT: u32 = 0x4f8cff;

/// One window: status of the compio layer, the last response body it fetched
/// through the tako endpoint, and a button requesting the next fetch.
struct M0Root {
    endpoint: Option<SharedString>,
    status: SharedString,
    body: Option<String>,
    latency_ms: Option<u64>,
    fetches: u64,
    error: Option<SharedString>,
    requests: async_channel::Sender<Fetch>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
}

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let service = service::spawn();
        let bounds = Bounds::centered(None, size(px(760.0), px(440.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("rchat-blazar — M0".into()),
                    ..Default::default()
                }),
                app_id: Some(APP_ID.to_owned()),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| M0Root::new(service, cx)),
        )
        .expect("open the M0 window");

        // Closing the last window ends the process; the compio thread is a
        // daemon by construction (the channel close is its shutdown signal).
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        cx.activate(true);
    });
}

impl M0Root {
    fn new(service: Service, cx: &mut Context<Self>) -> Self {
        // compio → UI: pump service events into this entity for as long as
        // the window lives; a failed upgrade means the window closed.
        cx.spawn(async move |root, cx| {
            while let Ok(event) = service.events.recv().await {
                if let ServiceEvent::Quit = event {
                    let _ = cx.update(|app| app.quit());
                    break;
                }
                if root.update(cx, |state, cx| state.apply(event, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            endpoint: None,
            status: "waiting for the compio executor…".into(),
            body: None,
            latency_ms: None,
            fetches: 0,
            error: None,
            requests: service.requests,
            snapshot: service.snapshot,
        }
    }

    /// Mirrors the entity into the control-socket snapshot. The UI thread
    /// holds the lock only for the store, never across a frame.
    fn mirror(&self) {
        let state = UiSnapshot {
            status: self.status.to_string(),
            endpoint: self.endpoint.as_ref().map(ToString::to_string),
            body: self.body.clone(),
            latency_ms: self.latency_ms,
            fetches: self.fetches,
            error: self.error.as_ref().map(ToString::to_string),
        };
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state);
    }

    fn apply(&mut self, event: ServiceEvent, cx: &mut Context<Self>) {
        match event {
            ServiceEvent::Listening { addr } => {
                self.endpoint = Some(format!("http://{addr}/m0/health").into());
                self.status = "compio executor up — tako endpoint listening".into();
            }
            ServiceEvent::Fetched {
                seq,
                body,
                latency_ms,
            } => {
                self.fetches = seq;
                self.body = Some(body);
                self.latency_ms = Some(latency_ms);
                self.error = None;
                self.status = "fetch complete".into();
            }
            ServiceEvent::Failed { seq, message } => {
                self.fetches = seq;
                self.error = Some(message.into());
                self.status = "fetch failed".into();
            }
            ServiceEvent::Quit => return, // the pump quits before apply
        }
        self.mirror();
        cx.notify();
    }

    fn request_fetch(&mut self, cx: &mut Context<Self>) {
        if self.requests.try_send(Fetch).is_err() {
            self.error = Some("the compio service is gone".into());
            cx.notify();
        }
    }
}

fn stat(label: &str, value: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .child(label.to_owned()),
        )
        .child(div().text_sm().child(value))
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

impl gpui::Render for M0Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let endpoint = self.endpoint.clone().unwrap_or("starting…".into());
        let latency: SharedString = match self.latency_ms {
            Some(ms) => format!("{ms} ms").into(),
            None => "—".into(),
        };
        let body = self
            .body
            .clone()
            .unwrap_or_else(|| "no response yet".to_owned());
        let error = div().when_some(self.error.clone(), |row, message| {
            row.child(
                div()
                    .text_sm()
                    .text_color(gpui::red())
                    .child(format!("error: {message}")),
            )
        });

        div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_3()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().text_xl().child("rchat-blazar — M0 wire-up"))
                    .child(div().flex_1())
                    .child(div().text_sm().text_color(rgb(MUTED)).child(endpoint)),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .gap_3()
                    .child(panel(
                        "compio executor",
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(stat("status", self.status.clone()))
                            .child(stat("fetches", self.fetches.to_string()))
                            .child(stat("last latency", latency)),
                    ))
                    .child(panel(
                        "tako GET /m0/health body",
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_sm().child(body))
                            .child(error),
                    )),
            )
            .child(
                div()
                    .id("fetch")
                    .flex_none()
                    .px_3()
                    .py_1()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(ACCENT)))
                    .active(|style| style.opacity(0.85))
                    .on_click(cx.listener(|this, _, _, cx| this.request_fetch(cx)))
                    .child("Fetch again"),
            )
    }
}
