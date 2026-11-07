//! Serde shapes of the Rocket.Chat REST responses — private wire forms;
//! the domain types in [`super::types`] are what the rest of the app sees.
//! Only needed fields are declared; the server sends more.

use serde::Deserialize;

#[derive(Deserialize)]
pub struct LoginResponse {
    pub status: String,
    pub data: LoginData,
}

#[derive(Deserialize)]
pub struct LoginData {
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "authToken")]
    pub auth_token: String,
    pub me: Me,
}

#[derive(Deserialize)]
pub struct Me {
    pub username: Option<String>,
}

#[derive(Deserialize)]
pub struct RoomsGetResponse {
    pub update: Vec<WireRoom>,
}

#[derive(Deserialize)]
pub struct WireRoom {
    #[serde(rename = "_id")]
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "t")]
    pub kind: Option<String>,
    /// Direct-message participants (`[others…, me]`); the display name
    /// derives from it when `name` is absent.
    #[serde(default)]
    pub usernames: Option<Vec<String>>,
    /// Last-message timestamp; the room list sorts by it (missing =
    /// oldest).
    #[serde(default)]
    pub lm: Option<String>,
    /// The room's newest message — the sidebar preview.
    #[serde(default)]
    #[serde(rename = "lastMessage")]
    pub last_message: Option<WireLastMessage>,
}

/// The sidebar preview source (wire `lastMessage`).
#[derive(Deserialize)]
pub struct WireLastMessage {
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub u: Option<WireUser>,
}

#[derive(Deserialize)]
pub struct HistoryResponse {
    pub messages: Vec<WireMessage>,
}

#[derive(Deserialize)]
pub struct SendMessageResponse {
    pub message: WireMessage,
}

/// `chat.update` answers with the refreshed message.
pub type UpdateMessageResponse = SendMessageResponse;

/// `chat.react` answers with only the success flag.
#[derive(Deserialize)]
pub struct SuccessResponse {
    pub success: bool,
}

/// `rooms.media/{rid}` phase one: the stored file's id, consumed by the
/// confirm call.
#[derive(Deserialize)]
pub struct MediaUploadResponse {
    pub file: MediaFile,
}

#[derive(Deserialize)]
pub struct MediaFile {
    #[serde(rename = "_id")]
    pub id: String,
}

#[derive(Deserialize)]
pub struct WireMessage {
    #[serde(rename = "_id")]
    pub id: String,
    pub rid: String,
    pub msg: Option<String>,
    pub ts: Option<String>,
    pub u: Option<WireUser>,
    /// Thread parent id when the message is a thread reply.
    #[serde(default)]
    pub tmid: Option<String>,
    /// Reply count when the message starts a thread (wire `tcount`).
    #[serde(default)]
    pub tcount: Option<u32>,
    /// The thread's last message timestamp (wire `tlm`).
    #[serde(default)]
    pub tlm: Option<String>,
    /// An uploaded attachment (rooms.media / file uploads).
    #[serde(default)]
    pub file: Option<WireFile>,
    /// Reaction name (server sends it colon-wrapped) → participant list.
    #[serde(default)]
    pub reactions: Option<std::collections::BTreeMap<String, WireReaction>>,
}

#[derive(Deserialize)]
pub struct WireFile {
    #[serde(rename = "_id")]
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(rename = "type")]
    pub mime: Option<String>,
}

#[derive(Deserialize)]
pub struct WireReaction {
    #[serde(default)]
    pub usernames: Option<Vec<String>>,
}

/// `subscriptions.get` rows: the per-user subscription state the room
/// list merges its unread badges from.
#[derive(Deserialize)]
pub struct SubscriptionsGetResponse {
    pub update: Vec<WireSubscription>,
}

#[derive(Deserialize)]
pub struct WireSubscription {
    #[serde(rename = "rid")]
    pub room_id: String,
    pub unread: Option<u32>,
}

#[derive(Deserialize)]
pub struct WireUser {
    pub username: Option<String>,
}

#[derive(Deserialize)]
pub struct EmojiListResponse {
    pub emojis: EmojiDelta,
}

#[derive(Deserialize)]
pub struct EmojiDelta {
    #[serde(default)]
    pub update: Vec<WireEmoji>,
}

#[derive(Deserialize)]
pub struct WireEmoji {
    #[serde(rename = "_id")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub extension: String,
    #[serde(rename = "_updatedAt", default)]
    pub updated_at: String,
}
