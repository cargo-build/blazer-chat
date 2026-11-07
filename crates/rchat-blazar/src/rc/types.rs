//! Domain types and settings of the Rocket.Chat protocol layer.

use std::fmt;

/// Where to find the server; module-owned settings with a public default.
#[derive(Clone)]
pub struct RcSettings {
    pub(crate) tls: bool,
    pub(crate) addr: (String, u16),
    pub(crate) host_header: String,
}

impl Default for RcSettings {
    fn default() -> Self {
        Self {
            tls: false,
            addr: ("127.0.0.1".to_owned(), 3000),
            host_header: "127.0.0.1:3000".to_owned(),
        }
    }
}

impl RcSettings {
    /// Parses `http://host[:port]` (default 80) and `https://host[:port]`
    /// (default 443).
    pub fn parse(server_url: &str) -> Result<Self, RcError> {
        let (tls, rest) = if let Some(rest) = server_url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = server_url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(RcError::Config(format!(
                "unsupported server_url {server_url:?}: use http:// or https://"
            )));
        };
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| RcError::Config(format!("bad port in {server_url:?}")))?,
            ),
            None => (rest.to_owned(), default_port),
        };
        if host.is_empty() {
            return Err(RcError::Config(format!("no host in {server_url:?}")));
        }
        Ok(Self {
            tls,
            addr: (host.clone(), port),
            host_header: rest.to_owned(),
        })
    }

    /// Socket address the transport connects to.
    pub(crate) fn addr(&self) -> (String, u16) {
        self.addr.clone()
    }

    pub(crate) fn host_header(&self) -> String {
        self.host_header.clone()
    }

    /// Whether the transport must speak TLS (`https://`).
    pub(crate) fn is_tls(&self) -> bool {
        self.tls
    }
}

/// One authenticated session.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub user_id: String,
    pub auth_token: String,
    pub username: String,
}

/// A room from `rooms.get`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Room {
    pub id: String,
    pub name: String,
    /// `c` channel, `p` group, `d` direct — drives history routing and the
    /// live subscription of the M2 slices.
    pub kind: char,
    /// Unread count from the `subscriptions-changed` stream; cleared
    /// locally when the room opens (server-side via `subscriptions.read`).
    pub unread: u32,
    /// Last-message timestamp; the list sorts by it, newest first.
    pub lm: Option<String>,
}

/// One chat message, oldest position last.
#[derive(Clone, Debug)]
pub struct Msg {
    /// Message identity on the server; consumed by the live-updates slice
    /// (dedup and edits), not by the M1 REST view.
    pub id: String,
    pub room_id: String,
    pub sender: String,
    pub text: String,
    pub ts: String,
    /// Thread parent id when this is a thread reply.
    pub tmid: Option<String>,
    /// Reply count when this message starts a thread (wire `tcount`).
    pub tcount: Option<u32>,
    /// The thread's last message timestamp (wire `tlm`).
    pub tlm: Option<String>,
    /// An uploaded attachment; the pane renders it as a chip that opens
    /// the file on the server.
    pub file: Option<AttachedFile>,
    /// Reaction name (colons stripped) → user count; the live stream is
    /// the authority and replaces this map on every redelivery.
    pub reactions: std::collections::BTreeMap<String, u32>,
}

/// One `stream-notify-user/{uid}/notification` event: a message for the
/// user outside the room they are reading. Wire shape per the vendored
/// ruqola client's captured document (`rocketchataccount.cpp`).
#[derive(Clone, Debug)]
#[expect(
    dead_code,
    reason = "wire-complete decode; the popup UI consumes the rest next slice"
)]
pub struct Notification {
    pub title: String,
    /// Envelope fallback text (present when `payload.message` is not).
    pub text: String,
    pub room_id: String,
    pub sender: Option<String>,
    /// `"d"` DM, `"c"` channel, `"p"` private group.
    pub room_type: Option<String>,
    pub message_id: Option<String>,
    pub tmid: Option<String>,
    /// The body from `payload.message.msg`, when present.
    pub message: Option<String>,
}

/// One uploaded attachment.
#[derive(Clone, Debug)]
pub struct AttachedFile {
    pub name: String,
    pub size: u64,
    /// Absolute URL on the server (browser-openable).
    pub url: String,
    /// Server-relative download path (authenticated GET).
    pub path: String,
    pub mime: String,
    /// Cached image path once the preview is fetched (images only).
    pub image_path: Option<String>,
}

impl AttachedFile {
    /// Inline-previewable: raster images the GPU stack can decode.
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/") && self.mime != "image/svg+xml"
    }
}

/// A server-defined custom emoji from `emoji-custom.list`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CustomEmoji {
    #[serde(rename = "_id")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub extension: String,
    /// Server-side update stamp — drives cache refresh decisions.
    #[serde(rename = "_updatedAt", default)]
    pub updated_at: String,
}

#[derive(Debug)]
pub enum RcError {
    Config(String),
    Io(std::io::Error),
    Http {
        status: u16,
        body: String,
    },
    Parse(String),
    /// The server answered 2xx but reported failure in the payload.
    Refused(String),
    NoSession,
}

impl fmt::Display for RcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "config: {message}"),
            Self::Io(error) => write!(f, "connection: {error}"),
            Self::Http { status, body } => write!(f, "server {status}: {body}"),
            Self::Parse(message) => write!(f, "protocol: {message}"),
            Self::Refused(reason) => write!(f, "refused: {reason}"),
            Self::NoSession => write!(f, "not logged in"),
        }
    }
}

impl std::error::Error for RcError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    fn attachment(mime: &str) -> AttachedFile {
        AttachedFile {
            name: "probe".to_owned(),
            size: 1,
            url: "https://example.com/file-upload/x/probe".to_owned(),
            path: "/file-upload/x/probe".to_owned(),
            mime: mime.to_owned(),
            image_path: None,
        }
    }

    #[test]
    fn image_classification_accepts_raster_and_rejects_svg() {
        assert!(attachment("image/png").is_image());
        assert!(attachment("image/jpeg").is_image());
        assert!(attachment("image/webp").is_image());
        assert!(!attachment("image/svg+xml").is_image(), "svg is not raster");
        assert!(!attachment("text/plain").is_image());
        assert!(!attachment("application/pdf").is_image());
    }
}
