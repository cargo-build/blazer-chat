//! The compio layer: a tako HTTP endpoint plus a client loop that proves it
//! serves, both on one io_uring executor.
//!
//! The compio driver is thread-local, so this layer owns one std thread with
//! its `compio::runtime::Runtime`; GPUI never blocks on it. Both directions
//! cross through `async-channel`, which is executor-agnostic — channels, not
//! nested executors. The endpoint listens on an ephemeral loopback port; the
//! `Listening` event carries the address to the UI.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_channel::{Receiver, Sender, unbounded};
use compio::buf::BufResult;
use compio::io::{AsyncReadExt, AsyncWriteExt};
use tako::body::TakoBody;
use tako::responder::Responder;
use tako::router::Router;
use tako::types::Response;
use tako::{CompioServer, StatusCode, header};

/// What the UI keeps on screen about the compio side.
pub enum ServiceEvent {
    /// The tako endpoint accepts on this loopback address.
    Listening { addr: SocketAddr },
    /// One loopback fetch of the endpoint completed.
    Fetched {
        seq: u64,
        body: String,
        latency_ms: u64,
    },
    /// One loopback fetch failed; the endpoint may still recover.
    Failed { seq: u64, message: String },
    /// The control socket asks the application to quit.
    Quit,
}

/// A UI request for one more fetch of the endpoint.
#[derive(Clone, Copy)]
pub struct Fetch;

/// Mirror of the window state, written by the UI entity and served by the
/// control socket so background tooling can verify without the compositor.
#[derive(Clone)]
pub struct UiSnapshot {
    pub status: String,
    pub endpoint: Option<String>,
    pub body: Option<String>,
    pub latency_ms: Option<u64>,
    pub fetches: u64,
    pub error: Option<String>,
}

/// The UI-facing half of the service boundary.
pub struct Service {
    /// UI consumes events produced on the compio thread.
    pub events: Receiver<ServiceEvent>,
    /// UI asks the compio thread for work.
    pub requests: Sender<Fetch>,
    /// Latest UI state, read by the control socket.
    pub snapshot: Arc<Mutex<Option<UiSnapshot>>>,
}

/// Spawns the compio thread and returns its UI-facing handles.
pub fn spawn() -> Service {
    let (events, event_rx) = unbounded();
    let (request_tx, requests) = unbounded();
    let snapshot = Arc::new(Mutex::new(None));
    let thread_snapshot = Arc::clone(&snapshot);
    let thread_fetches = request_tx.clone();
    std::thread::Builder::new()
        .name("compio".into())
        .spawn(move || run(events, requests, thread_fetches, thread_snapshot))
        .expect("spawn the compio service thread");
    Service {
        events: event_rx,
        requests: request_tx,
        snapshot,
    }
}

fn run(
    events: Sender<ServiceEvent>,
    requests: Receiver<Fetch>,
    control_fetches: Sender<Fetch>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
) {
    let runtime = compio::runtime::Runtime::new().expect("start the compio runtime");
    runtime.block_on(drive(events, requests, control_fetches, snapshot));
}

/// Serves the endpoint and the control socket, and answers fetch requests
/// until the boundary closes (UI dropped). tako spawns accept work on the
/// ambient runtime, so this must run inside `block_on` for the process
/// lifetime.
async fn drive(
    events: Sender<ServiceEvent>,
    requests: Receiver<Fetch>,
    control_fetches: Sender<Fetch>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
) {
    let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind the tako endpoint");
    let addr = listener.local_addr().expect("read the bound address");

    let served = Arc::new(AtomicU64::new(0));
    let mut router = Router::new();
    router.get("/m0/health", move || {
        let served = served.clone();
        async move {
            let count = served.fetch_add(1, Ordering::Relaxed) + 1;
            Json(format!(
                r#"{{"layer":"tako on compio","ok":true,"served":{count}}}"#
            ))
        }
    });

    let server = CompioServer::builder().build();
    let _endpoint = server.spawn_http(listener, router);

    let mut control = Router::new();
    control.get("/m0/state", {
        let snapshot = Arc::clone(&snapshot);
        move || {
            let snapshot = Arc::clone(&snapshot);
            async move { Json(state_json(&snapshot)) }
        }
    });
    control.post("/m0/fetch", {
        let control_fetches = control_fetches.clone();
        move || {
            let control_fetches = control_fetches.clone();
            async move {
                if control_fetches.try_send(Fetch).is_ok() {
                    (StatusCode::OK, Json(r#"{"queued":true}"#.to_owned()))
                } else {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(r#"{"queued":false}"#.to_owned()),
                    )
                }
            }
        }
    });
    control.post("/m0/quit", {
        let events = events.clone();
        move || {
            let events = events.clone();
            async move {
                if events.try_send(ServiceEvent::Quit).is_ok() {
                    (StatusCode::OK, Json(r#"{"quitting":true}"#.to_owned()))
                } else {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(r#"{"quitting":false}"#.to_owned()),
                    )
                }
            }
        }
    });

    if let Err(error) = server
        .try_spawn_unix_http(control_socket_path(), control)
        .await
    {
        // The endpoint still serves; background tooling just has no handle.
        eprintln!("rchat-blazar: control socket unavailable: {error}");
    }
    let _ = events.send(ServiceEvent::Listening { addr }).await;

    let mut seq = 0u64;
    while requests.recv().await.is_ok() {
        seq += 1;
        let started = Instant::now();
        match fetch_health(addr).await {
            Ok(body) => {
                let _ = events
                    .send(ServiceEvent::Fetched {
                        seq,
                        body,
                        latency_ms: elapsed_ms(started),
                    })
                    .await;
            }
            Err(message) => {
                let _ = events
                    .send(ServiceEvent::Failed {
                        seq,
                        message: message.to_string(),
                    })
                    .await;
            }
        }
    }
}

/// Control socket under the runtime dir; one process owns the fixed name per
/// user, so a second instance fails to bind instead of shadowing the first.
fn control_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("rchat-blazar-m0.sock")
}

/// JSON view of the mirrored UI state; poisoned locks mean a UI-thread panic,
/// and stale data is still the best available answer.
fn state_json(snapshot: &Mutex<Option<UiSnapshot>>) -> String {
    let state = snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match state.as_ref() {
        Some(state) => serde_json::json!({
            "status": state.status,
            "endpoint": state.endpoint,
            "body": state.body,
            "latency_ms": state.latency_ms,
            "fetches": state.fetches,
            "error": state.error,
        })
        .to_string(),
        None => r#"{"status":"no window reported yet"}"#.to_owned(),
    }
}

/// One minimal HTTP/1.1 GET over the compio socket: the same executor serves
/// and consumes the endpoint, so a round trip proves the whole layer.
async fn fetch_health(addr: SocketAddr) -> std::io::Result<String> {
    let mut stream = compio::net::TcpStream::connect(addr).await?;
    let request = format!("GET /m0/health HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n");
    let BufResult(written, _) = stream.write_all(request.into_bytes()).await;
    written?;
    let BufResult(_read, raw) = stream.read_to_end(Vec::with_capacity(1024)).await;
    let raw = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| invalid_data("no HTTP header/body separator"))?;
    if !head.starts_with("HTTP/1.1 200") {
        let status = head.lines().next().unwrap_or("<empty>");
        return Err(invalid_data(&format!("unexpected status: {status}")));
    }
    Ok(body.to_owned())
}

fn invalid_data(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_owned())
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// A JSON responder with the content type set.
struct Json(String);

impl Responder for Json {
    fn into_response(self) -> Response {
        let mut response = Response::new(TakoBody::from(self.0));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        response
    }
}
