//! Domain types and settings of the Rocket.Chat protocol layer.

use std::fmt;

/// Where to find the server; module-owned settings with a public default.
#[derive(Clone)]
pub struct RcSettings {
    /// `http://host:port` of the Rocket.Chat server.
    pub server_url: String,
}

impl Default for RcSettings {
    fn default() -> Self {
        Self {
            server_url: "http://127.0.0.1:3000".to_owned(),
        }
    }
}

impl RcSettings {
    /// Parses `http://host:port`; https is rejected until the TLS slice.
    pub fn parse(server_url: &str) -> Result<Self, RcError> {
        let rest = server_url.strip_prefix("http://").ok_or_else(|| {
            RcError::Config(format!(
                "unsupported server_url {server_url:?}: use http://host:port"
            ))
        })?;
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| RcError::Config(format!("bad port in {server_url:?}")))?,
            ),
            None => (rest.to_owned(), 80),
        };
        let _ = port;
        if host.is_empty() {
            return Err(RcError::Config(format!("no host in {server_url:?}")));
        }
        Ok(Self {
            server_url: server_url.to_owned(),
        })
    }

    /// Socket address the transport connects to.
    pub(crate) fn addr(&self) -> (String, u16) {
        let rest = self
            .server_url
            .strip_prefix("http://")
            .expect("settings carry a parsed http url");
        match rest.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), port.parse().unwrap_or(80)),
            None => (rest.to_owned(), 80),
        }
    }

    pub(crate) fn host_header(&self) -> String {
        self.server_url
            .strip_prefix("http://")
            .expect("settings carry a parsed http url")
            .to_owned()
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
    /// `c` channel, `p` group, `d` direct — drives the subscription routing
    /// of the live-updates slice; the M1 REST view renders the name only.
    #[allow(dead_code)]
    pub kind: char,
}

/// One chat message, oldest position last.
#[derive(Clone, Debug)]
pub struct Msg {
    /// Message identity on the server; consumed by the live-updates slice
    /// (dedup and edits), not by the M1 REST view.
    #[allow(dead_code)]
    pub id: String,
    pub room_id: String,
    pub sender: String,
    pub text: String,
    pub ts: String,
}

#[derive(Debug)]
pub enum RcError {
    Config(String),
    Io(std::io::Error),
    Http { status: u16, body: String },
    Parse(String),
    NoSession,
}

impl fmt::Display for RcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "config: {message}"),
            Self::Io(error) => write!(f, "connection: {error}"),
            Self::Http { status, body } => write!(f, "server {status}: {body}"),
            Self::Parse(message) => write!(f, "protocol: {message}"),
            Self::NoSession => write!(f, "not logged in"),
        }
    }
}

impl std::error::Error for RcError {}
