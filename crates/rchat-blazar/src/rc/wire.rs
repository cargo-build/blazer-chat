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
}

#[derive(Deserialize)]
pub struct HistoryResponse {
    pub messages: Vec<WireMessage>,
}

#[derive(Deserialize)]
pub struct SendMessageResponse {
    pub message: WireMessage,
}

#[derive(Deserialize)]
pub struct WireMessage {
    #[serde(rename = "_id")]
    pub id: String,
    pub rid: String,
    pub msg: Option<String>,
    pub ts: Option<String>,
    pub u: Option<WireUser>,
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
