//! The Rocket.Chat REST client, generic over the transport so protocol
//! tests run against canned responses with no sockets.
//!
//! Protocol facts verified against the vendored server source
//! (Rocket.Chat 8.9.0-rc.5-329-g1826a86264):
//!
//! - `POST /api/v1/login` `{user, password}` →
//!   `{status:"success", data:{userId, authToken, me}}` (`ApiClass.ts`).
//! - Auth headers are `X-Auth-Token` and `X-User-Id`.
//! - `GET /api/v1/rooms.get` → `{update:[IRoom], remove:[], success}` —
//!   the modern room list (`rooms.list` is gone in 8.9).
//! - `GET /api/v1/channels.history?roomId=&count=` → `{messages, success}`
//!   (channel-type rooms; groups/DMs need their own endpoints later).
//! - `POST /api/v1/chat.sendMessage` `{message:{rid, msg}}` →
//!   `{message: IMessage, success}` (`rest-typings` schema: `msg` is the
//!   text field).
//!
//! TLS is out of scope for this slice: the dev server is plain HTTP on
//! localhost, and `server_url` in `public.toml` says where to connect.

use serde::Deserialize;

use crate::http::{HttpTransport, Request, Transport};
use crate::rc::types::{Msg, RcError, RcSettings, Room, Session};
use crate::rc::wire::{HistoryResponse, LoginResponse, RoomsGetResponse, SendMessageResponse};

/// The typed client: one session, typed results out.
pub struct RcClient<T: Transport = HttpTransport> {
    settings: RcSettings,
    transport: T,
    session: Option<Session>,
}

impl RcClient<HttpTransport> {
    pub fn new(settings: RcSettings) -> Self {
        Self {
            transport: HttpTransport::new(settings.addr()),
            settings,
            session: None,
        }
    }
}

impl<T: Transport> RcClient<T> {
    /// Test seam: any transport, canned or real.
    #[cfg(test)]
    pub fn with_transport(settings: RcSettings, transport: T) -> Self {
        Self {
            settings,
            transport,
            session: None,
        }
    }

    #[cfg(test)]
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Installs a previously stored session (startup resume).
    pub fn set_session(&mut self, session: Session) {
        self.session = Some(session);
    }

    fn auth_headers(&self) -> Result<Vec<(String, String)>, RcError> {
        let session = self.session.as_ref().ok_or(RcError::NoSession)?;
        Ok(vec![
            ("x-auth-token".to_owned(), session.auth_token.clone()),
            ("x-user-id".to_owned(), session.user_id.clone()),
        ])
    }

    /// Sends one request; `auth` headers require a session (NoSession
    /// otherwise) and non-2xx answers surface as `RcError::Http`.
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<String>,
        auth: bool,
    ) -> Result<String, RcError> {
        let headers = if auth {
            self.auth_headers()?
        } else {
            Vec::new()
        };
        let request = Request {
            method,
            path,
            host: &self.settings.host_header(),
            headers,
            body,
        };
        let response = self.transport.call(request).await.map_err(RcError::Io)?;
        if !(200..300).contains(&response.status) {
            return Err(RcError::Http {
                status: response.status,
                body: response.body.chars().take(300).collect(),
            });
        }
        Ok(response.body)
    }

    fn parse<D: for<'de> Deserialize<'de>>(body: &str, what: &str) -> Result<D, RcError> {
        serde_json::from_str(body).map_err(|error| RcError::Parse(format!("{what}: {error}")))
    }

    pub async fn login(&mut self, user: &str, password: &str) -> Result<Session, RcError> {
        let body = serde_json::json!({ "user": user, "password": password }).to_string();
        let body = self
            .call("POST", "/api/v1/login", Some(body), false)
            .await?;
        let parsed: LoginResponse = Self::parse(&body, "login")?;
        if parsed.status != "success" {
            return Err(RcError::Parse("login was not successful".to_owned()));
        }
        let session = Session {
            user_id: parsed.data.user_id,
            auth_token: parsed.data.auth_token,
            username: parsed.data.me.username.unwrap_or_else(|| user.to_owned()),
        };
        self.session = Some(session.clone());
        Ok(session)
    }

    pub async fn rooms(&self) -> Result<Vec<Room>, RcError> {
        let body = self.call("GET", "/api/v1/rooms.get", None, true).await?;
        let parsed: RoomsGetResponse = Self::parse(&body, "rooms.get")?;
        Ok(parsed
            .update
            .into_iter()
            .filter_map(|room| {
                let kind = room.kind?.chars().next()?;
                let name = match kind {
                    'c' | 'p' => room.name?,
                    'd' => format!("dm:{}", room.id),
                    _ => return None,
                };
                Some(Room {
                    id: room.id,
                    name,
                    kind,
                })
            })
            .collect())
    }

    /// History of a channel-type room, oldest message first.
    pub async fn history(&self, room_id: &str) -> Result<Vec<Msg>, RcError> {
        let path = format!("/api/v1/channels.history?roomId={room_id}&count=50");
        let body = self.call("GET", &path, None, true).await?;
        let parsed: HistoryResponse = Self::parse(&body, "channels.history")?;
        Ok(parsed
            .messages
            .into_iter()
            .map(|message| Msg {
                id: message.id,
                room_id: message.rid,
                sender: message.u.and_then(|user| user.username).unwrap_or_default(),
                text: message.msg.unwrap_or_default(),
                ts: message.ts.unwrap_or_default(),
            })
            .rev()
            .collect())
    }

    pub async fn send(&self, room_id: &str, text: &str) -> Result<Msg, RcError> {
        let body = serde_json::json!({ "message": { "rid": room_id, "msg": text } }).to_string();
        let body = self
            .call("POST", "/api/v1/chat.sendMessage", Some(body), true)
            .await?;
        let parsed: SendMessageResponse = Self::parse(&body, "chat.sendMessage")?;
        Ok(Msg {
            id: parsed.message.id,
            room_id: parsed.message.rid,
            sender: parsed
                .message
                .u
                .and_then(|user| user.username)
                .unwrap_or_default(),
            text: parsed.message.msg.unwrap_or_default(),
            ts: parsed.message.ts.unwrap_or_default(),
        })
    }
}
