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
//! - `GET /api/v1/emoji-custom.list` → `{emojis:{update, remove}}`; images
//!   at `/emoji-custom/{name}.{extension}`.
//!
//! The transport speaks `http://` and `https://` (server URL in
//! `public.toml` says where to connect).

use serde::Deserialize;

use crate::http::{HttpTransport, Request, Transport};
use crate::rc::types::{CustomEmoji, Msg, RcError, RcSettings, Room, Session};
use crate::rc::wire::{
    EmojiListResponse, HistoryResponse, LoginResponse, RoomsGetResponse, SendMessageResponse,
};

/// The typed client: one session, typed results out.
pub struct RcClient<T: Transport = HttpTransport> {
    transport: T,
    session: Option<Session>,
    /// `rooms.get` results, keyed by room id — history routes by kind:
    /// `c` → channels.history, `p` → groups.history, `d` → im.history.
    room_kinds: std::collections::HashMap<String, char>,
}

impl RcClient<HttpTransport> {
    pub fn new(settings: RcSettings) -> Self {
        Self {
            transport: HttpTransport::new(
                settings.addr(),
                settings.host_header(),
                settings.is_tls(),
            ),
            session: None,
            room_kinds: std::collections::HashMap::new(),
        }
    }

    /// Custom emojis of the server: the `update` side of the
    /// `emoji-custom.list` delta — enough for a name-keyed cache.
    pub async fn emoji_custom_list(&mut self) -> Result<Vec<CustomEmoji>, RcError> {
        let body = self
            .call("GET", "/api/v1/emoji-custom.list", None, true)
            .await?;
        let parsed: EmojiListResponse = Self::parse(&body, "emoji-custom.list")?;
        Ok(parsed
            .emojis
            .update
            .into_iter()
            .map(|emoji| CustomEmoji {
                id: emoji.id,
                name: emoji.name,
                aliases: emoji.aliases,
                extension: emoji.extension,
                updated_at: emoji.updated_at,
            })
            .collect())
    }

    /// The image bytes of a custom emoji (`/emoji-custom/{name}.{ext}`).
    /// Inherent to the real transport: `get_bytes` is not on the [`Transport`]
    /// seam (test doubles have no binary bodies).
    pub async fn emoji_image(&self, name: &str, extension: &str) -> Result<Vec<u8>, RcError> {
        let path = format!("/emoji-custom/{name}.{extension}");
        self.transport.get_bytes(&path).await.map_err(RcError::Io)
    }
}

impl<T: Transport> RcClient<T> {
    /// Injection point: any transport (canned in tests, real in prod).
    /// `settings` stays on the signature for call-site symmetry; only the
    /// transport decides the connection today.
    pub fn with_transport(_settings: RcSettings, transport: T) -> Self {
        Self {
            transport,
            session: None,
            room_kinds: std::collections::HashMap::new(),
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

    /// Display name of a direct-message room: the participants other than
    /// the session's own username, joined; a self-DM shows the own
    /// username.
    fn dm_display_name(usernames: &Option<Vec<String>>, me: Option<&str>) -> Option<String> {
        let names = usernames.as_deref()?;
        let others: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|name| Some(*name) != me)
            .collect();
        match others.is_empty() {
            false => Some(others.join(", ")),
            true => names.first().map(String::to_owned),
        }
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

    pub async fn rooms(&mut self) -> Result<Vec<Room>, RcError> {
        let body = self.call("GET", "/api/v1/rooms.get", None, true).await?;
        let parsed: RoomsGetResponse = Self::parse(&body, "rooms.get")?;
        let me = self
            .session
            .as_ref()
            .map(|session| session.username.clone());
        let rooms: Vec<Room> = parsed
            .update
            .into_iter()
            .filter_map(|room| {
                let kind = room.kind?.chars().next()?;
                let name = match kind {
                    // Direct messages have no `name`: the original client
                    // shows the other participants' usernames, joined; a
                    // self-DM shows the own username.
                    'd' => Self::dm_display_name(&room.usernames, me.as_deref())
                        .or_else(|| Some(format!("dm:{}", room.id)))?,
                    'c' | 'p' => room.name?,
                    _ => return None,
                };
                Some(Room {
                    id: room.id.clone(),
                    name,
                    kind,
                })
            })
            .collect();
        self.room_kinds = rooms
            .iter()
            .map(|room| (room.id.clone(), room.kind))
            .collect();
        Ok(rooms)
    }

    /// History of a room, oldest message first. The endpoint routes by the
    /// room kind from `rooms.get`: channels, groups, direct messages —
    /// each has its own REST path with the same response shape.
    pub async fn history(&mut self, room_id: &str) -> Result<Vec<Msg>, RcError> {
        let endpoint = match self.room_kinds.get(room_id).copied().unwrap_or('c') {
            'p' => "groups.history",
            'd' => "im.history",
            _ => "channels.history",
        };
        let path = format!("/api/v1/{endpoint}?roomId={room_id}&count=50");
        let body = self.call("GET", &path, None, true).await?;
        let parsed: HistoryResponse = Self::parse(&body, endpoint)?;
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
