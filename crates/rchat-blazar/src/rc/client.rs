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
    EmojiListResponse, HistoryResponse, LoginResponse, MediaUploadResponse, RoomsGetResponse,
    SendMessageResponse, SubscriptionsGetResponse, SuccessResponse, UpdateMessageResponse,
    WireMessage,
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
            body: body.map(String::into_bytes),
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
        let mut rooms: Vec<Room> = parsed
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
                    unread: 0,
                    lm: room.lm,
                })
            })
            .collect();
        self.room_kinds = rooms
            .iter()
            .map(|room| (room.id.clone(), room.kind))
            .collect();

        // The sidebar reads like the original client: newest activity
        // first; rooms without `lm` sink to the bottom.
        rooms.sort_by(|a, b| b.lm.as_deref().cmp(&a.lm.as_deref()));

        // Seed the unread badges from the subscription state (`rooms.get`
        // carries none; the notify stream only delivers later changes).
        let body = self
            .call("GET", "/api/v1/subscriptions.get", None, true)
            .await?;
        if let Ok(parsed) = Self::parse::<SubscriptionsGetResponse>(&body, "subscriptions.get") {
            for room in &mut rooms {
                if let Some(sub) = parsed.update.iter().find(|sub| sub.room_id == room.id)
                    && let Some(unread) = sub.unread
                {
                    room.unread = unread;
                }
            }
        }
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
            .map(Self::message_into_msg)
            .rev()
            .collect())
    }

    pub async fn send(&self, room_id: &str, text: &str) -> Result<Msg, RcError> {
        let body = serde_json::json!({ "message": { "rid": room_id, "msg": text } }).to_string();
        let body = self
            .call("POST", "/api/v1/chat.sendMessage", Some(body), true)
            .await?;
        let parsed: SendMessageResponse = Self::parse(&body, "chat.sendMessage")?;
        Ok(Self::message_into_msg(parsed.message))
    }

    /// Edits a message authored by the session's own user (`chat.update`).
    pub async fn update_message(
        &self,
        room_id: &str,
        msg_id: &str,
        text: &str,
    ) -> Result<Msg, RcError> {
        let body =
            serde_json::json!({ "roomId": room_id, "msgId": msg_id, "text": text }).to_string();
        let body = self
            .call("POST", "/api/v1/chat.update", Some(body), true)
            .await?;
        let parsed: UpdateMessageResponse = Self::parse(&body, "chat.update")?;
        Ok(Self::message_into_msg(parsed.message))
    }

    /// Deletes a message (`chat.delete`); servers may refuse by policy
    /// (`error-action-not-allowed`) even for the author's own messages.
    pub async fn delete_message(&self, room_id: &str, msg_id: &str) -> Result<(), RcError> {
        let body = serde_json::json!({ "roomId": room_id, "msgId": msg_id }).to_string();
        self.call("POST", "/api/v1/chat.delete", Some(body), true)
            .await?;
        Ok(())
    }

    /// Toggles a reaction (`chat.react`): the emoji name without colons —
    /// a repeat call removes it. Unicode emoji pass through as-is.
    pub async fn react(&self, msg_id: &str, emoji: &str) -> Result<(), RcError> {
        let body = serde_json::json!({ "messageId": msg_id, "emoji": emoji }).to_string();
        let body = self
            .call("POST", "/api/v1/chat.react", Some(body), true)
            .await?;
        match Self::parse::<SuccessResponse>(&body, "chat.react")?.success {
            true => Ok(()),
            false => Err(RcError::Refused(
                "chat.react answered success=false".to_owned(),
            )),
        }
    }

    /// One thread's messages, oldest first (`chat.getThreadMessages`).
    pub async fn thread_messages(&mut self, tmid: &str) -> Result<Vec<Msg>, RcError> {
        let path =
            format!("/api/v1/chat.getThreadMessages?tmid={tmid}&count=50&sort=%7B%22ts%22%3A-1%7D");
        let body = self.call("GET", &path, None, true).await?;
        let parsed: HistoryResponse = Self::parse(&body, "chat.getThreadMessages")?;
        Ok(parsed
            .messages
            .into_iter()
            .map(Self::message_into_msg)
            .rev()
            .collect())
    }

    /// Posts a thread reply (`chat.sendMessage` with `tmid`).
    pub async fn send_thread(&self, room_id: &str, tmid: &str, text: &str) -> Result<Msg, RcError> {
        let body = serde_json::json!({
            "message": { "rid": room_id, "msg": text, "tmid": tmid }
        })
        .to_string();
        let body = self
            .call("POST", "/api/v1/chat.sendMessage", Some(body), true)
            .await?;
        let parsed: SendMessageResponse = Self::parse(&body, "chat.sendMessage")?;
        Ok(Self::message_into_msg(parsed.message))
    }

    /// Marks the room read server-side (`subscriptions.read`), so the
    /// unread stream stops counting it.
    pub async fn mark_read(&mut self, room_id: &str) -> Result<(), RcError> {
        let body = serde_json::json!({ "rid": room_id }).to_string();
        let body = self
            .call("POST", "/api/v1/subscriptions.read", Some(body), true)
            .await?;
        match Self::parse::<SuccessResponse>(&body, "subscriptions.read")?.success {
            true => Ok(()),
            false => Err(RcError::Refused(
                "subscriptions.read answered success=false".to_owned(),
            )),
        }
    }

    /// Uploads a file and posts it as a message: the two-phase
    /// `rooms.media/{rid}` → `rooms.mediaConfirm/{rid}/{fileId}` flow
    /// (RC 7+; the legacy `rooms.upload` is 404 on RC 8.5). `msg` is the
    /// caption.
    pub async fn upload_file(
        &mut self,
        room_id: &str,
        path: &str,
        msg: &str,
    ) -> Result<Msg, RcError> {
        let bytes = std::fs::read(path)
            .map_err(|error| RcError::Config(format!("read {}: {error}", path)))?;
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("file")
            .to_owned();
        let boundary = format!(
            "rchat-blazar-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.subsec_nanos())
                .unwrap_or_default()
        );
        let body = multipart_body(&boundary, &filename, mime_type(&filename), &bytes, msg);

        let path_prefix = format!("/api/v1/rooms.media/{room_id}");
        let body = self.call_multipart(&path_prefix, &boundary, body).await?;
        let parsed: MediaUploadResponse = Self::parse(&body, "rooms.media")?;

        let confirm_path = format!("/api/v1/rooms.mediaConfirm/{room_id}/{}", parsed.file.id);
        let body = serde_json::json!({ "msg": msg }).to_string();
        let body = self.call("POST", &confirm_path, Some(body), true).await?;
        let parsed: SendMessageResponse = Self::parse(&body, "rooms.mediaConfirm")?;
        Ok(Self::message_into_msg(parsed.message))
    }

    /// One raw multipart POST; the JSON `call` wraps this for string
    /// bodies.
    async fn call_multipart(
        &self,
        path: &str,
        boundary: &str,
        body: Vec<u8>,
    ) -> Result<String, RcError> {
        let mut headers = self.auth_headers()?;
        headers.push((
            "content-type".to_owned(),
            format!("multipart/form-data; boundary={boundary}"),
        ));
        let request = Request {
            method: "POST",
            path,
            headers,
            body: Some(body),
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

    /// One wire message → the application message; the single owner of
    /// the mapping.
    fn message_into_msg(message: WireMessage) -> Msg {
        let reactions = message
            .reactions
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(name, reaction)| {
                let count = reaction.usernames.map_or(0, |users| users.len() as u32);
                (count > 0).then(|| (name.trim_matches(':').to_owned(), count))
            })
            .collect();
        Msg {
            id: message.id,
            room_id: message.rid,
            sender: message.u.and_then(|user| user.username).unwrap_or_default(),
            text: message.msg.unwrap_or_default(),
            ts: message.ts.unwrap_or_default(),
            tmid: message.tmid,
            reactions,
        }
    }
}

/// Builds the multipart/form-data body for `rooms.media`: one `file`
/// part plus an optional `msg` text part.
pub(super) fn multipart_body(
    boundary: &str,
    filename: &str,
    mime: &str,
    bytes: &[u8],
    msg: &str,
) -> Vec<u8> {
    let mut body = Vec::new();
    let push = |body: &mut Vec<u8>, part: &str| body.extend_from_slice(part.as_bytes());
    push(&mut body, &format!("--{boundary}\r\n"));
    push(
        &mut body,
        &format!("content-disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n"),
    );
    push(&mut body, &format!("content-type: {mime}\r\n\r\n"));
    body.extend_from_slice(bytes);
    push(&mut body, "\r\n");
    if !msg.is_empty() {
        push(&mut body, &format!("--{boundary}\r\n"));
        push(
            &mut body,
            "content-disposition: form-data; name=\"msg\"\r\n\r\n",
        );
        push(&mut body, msg);
        push(&mut body, "\r\n");
    }
    push(&mut body, &format!("--{boundary}--\r\n"));
    body
}

/// Minimal extension → MIME map for the file types the client uploads;
/// unknown extensions upload as `application/octet-stream` (the server
/// renders them as generic attachments).
pub(super) fn mime_type(filename: &str) -> &'static str {
    let extension = filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "txt" | "md" | "log" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}
