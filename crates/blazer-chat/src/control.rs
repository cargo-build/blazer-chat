//! The tako control socket (`$XDG_RUNTIME_DIR/blazer-chat.sock`): a unix
//! HTTP surface that lets background tooling drive and verify the app
//! without the compositor. Every route is a thin enqueue onto the same
//! command/event channels the UI uses — one boundary, two doors.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_channel::Sender;
use serde::Deserialize;
use tako::body::TakoBody;
use tako::extractors::json::Json as JsonExtractor;
use tako::responder::Responder;
use tako::router::Router;
use tako::types::Response;
use tako::{CompioServer, StatusCode, header};

use crate::app::{ChatCommand, ChatEvent, UiSnapshot};

/// Serves the control socket until the process ends. Failure degrades to a
/// stderr note — the UI loop keeps working without background tooling.
pub async fn serve(
    events: Sender<ChatEvent>,
    self_commands: Sender<ChatCommand>,
    snapshot: Arc<Mutex<Option<UiSnapshot>>>,
) {
    let mut control = Router::new();
    control.get("/state", {
        let snapshot = Arc::clone(&snapshot);
        move || {
            let snapshot = Arc::clone(&snapshot);
            async move { JsonBody(state_json(&snapshot)) }
        }
    });
    control.post("/login", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<CredentialsBody>| {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(
                    &self_commands,
                    ChatCommand::Login {
                        user: body.user,
                        password: body.password,
                    },
                ) {
                    queued()
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/refresh", {
        let self_commands = self_commands.clone();
        move || {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(&self_commands, ChatCommand::RefreshRooms) {
                    queued()
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/open", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<RoomBody>| {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(&self_commands, ChatCommand::OpenRoom { room_id: body.room }) {
                    queued()
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/send", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<SendBody>| {
            let self_commands = self_commands.clone();
            async move {
                let command = match body.tmid {
                    Some(tmid) => ChatCommand::SendThread {
                        room_id: body.room,
                        tmid,
                        text: body.text,
                    },
                    None => ChatCommand::Send {
                        room_id: body.room,
                        text: body.text,
                    },
                };
                if enqueue(&self_commands, command) {
                    queued()
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/send-file", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<FileBody>| {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(
                    &self_commands,
                    ChatCommand::UploadFile {
                        room_id: body.room,
                        path: body.path,
                        msg: body.msg,
                    },
                ) {
                    queued()
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/thread", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<ThreadBody>| {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(&self_commands, ChatCommand::OpenThread { tmid: body.tmid }) {
                    queued()
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/thread-close", {
        let self_commands = self_commands.clone();
        move || {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(&self_commands, ChatCommand::CloseThread) {
                    queued()
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/search", {
        let self_commands = self_commands.clone();
        move |JsonExtractor(body): JsonExtractor<SearchBody>| {
            let self_commands = self_commands.clone();
            async move {
                if enqueue(&self_commands, ChatCommand::Search(body.query)) {
                    queued()
                } else {
                    unavailable(r#"{"queued":false}"#)
                }
            }
        }
    });
    control.post("/quit", {
        let events = events.clone();
        move || {
            let events = events.clone();
            async move {
                if enqueue(&events, ChatEvent::Quit) {
                    (StatusCode::OK, JsonBody(r#"{"quitting":true}"#.to_owned()))
                } else {
                    unavailable(r#"{"quitting":false}"#)
                }
            }
        }
    });

    // The socket is an unauthenticated local RPC: without
    // XDG_RUNTIME_DIR there is no private directory to hold it, so the
    // surface stays off instead of dropping a world-connectable socket
    // into /tmp.
    let Some(socket_path) = control_socket_path() else {
        eprintln!("blazer-chat: control socket disabled: XDG_RUNTIME_DIR is not set");
        return;
    };
    let server = CompioServer::builder().build();
    if let Err(error) = server
        .try_spawn_unix_http(socket_path.clone(), control)
        .await
    {
        eprintln!("blazer-chat: control socket unavailable: {error}");
        return;
    }
    // The default socket mode is umask-derived and world-connectable on
    // a shared machine; owner-only keeps the RPC private.
    if let Err(error) = restrict_socket_permissions(&socket_path) {
        eprintln!("blazer-chat: control socket permissions: {error}");
    }
}

/// A `200` ack for an enqueued command.
fn queued() -> (StatusCode, JsonBody) {
    (StatusCode::OK, JsonBody(r#"{"queued":true}"#.to_owned()))
}

fn unavailable(body: &str) -> (StatusCode, JsonBody) {
    (StatusCode::SERVICE_UNAVAILABLE, JsonBody(body.to_owned()))
}

/// Queues one item; `false` means the receiving side is gone.
fn enqueue<T>(queue: &Sender<T>, item: T) -> bool {
    queue.try_send(item).is_ok()
}

/// `None` without `XDG_RUNTIME_DIR`: the surface refuses a world-readable
/// location such as `/tmp`.
fn control_socket_path() -> Option<PathBuf> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(PathBuf::from(runtime_dir).join("blazer-chat.sock"))
}

/// `0600`: the surface is an unauthenticated RPC, so only the owner may
/// connect.
fn restrict_socket_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// JSON view of the snapshot; a poisoned lock degrades to stale data.
fn state_json(snapshot: &Arc<Mutex<Option<UiSnapshot>>>) -> String {
    let guard = snapshot.lock().map(|guard| guard.clone());
    match guard {
        Ok(Some(snapshot)) => serde_json::to_string(&snapshot)
            .unwrap_or_else(|error| format!("{{\"error\":\"snapshot: {error}\"}}")),
        Ok(None) => "{\"status\":\"no window reported yet\"}".to_owned(),
        Err(_) => "{\"error\":\"snapshot lock poisoned\"}".to_owned(),
    }
}

/// A JSON responder with the content type set.
struct JsonBody(String);

impl Responder for JsonBody {
    fn into_response(self) -> Response {
        let mut response = Response::new(TakoBody::from(self.0));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        response
    }
}

#[derive(Deserialize)]
struct CredentialsBody {
    user: String,
    password: String,
}

#[derive(Deserialize)]
struct RoomBody {
    room: String,
}

#[derive(Deserialize)]
struct SendBody {
    room: String,
    text: String,
    #[serde(default)]
    tmid: Option<String>,
}

#[derive(Deserialize)]
struct FileBody {
    room: String,
    path: String,
    #[serde(default)]
    msg: String,
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
}

#[derive(Deserialize)]
struct ThreadBody {
    tmid: String,
}

/// Visible to tests through the app boundary fold: the snapshot is the
/// serialized [`UiSnapshot`].
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ChatState;

    #[test]
    fn state_json_reports_an_absent_window() {
        let snapshot = Arc::new(Mutex::new(None));
        assert!(state_json(&snapshot).contains("no window reported yet"));
    }

    #[test]
    fn state_json_carries_the_snapshot_fields() {
        let state = ChatState::default();
        let snapshot = Arc::new(Mutex::new(Some(UiSnapshot::from_state(&state))));
        let json = state_json(&snapshot);
        assert!(json.contains("not logged in"));
    }
}
