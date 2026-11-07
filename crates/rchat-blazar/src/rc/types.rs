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
    /// Reaction name (colons stripped) → user count; the live stream is
    /// the authority and replaces this map on every redelivery.
    pub reactions: std::collections::BTreeMap<String, u32>,
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
