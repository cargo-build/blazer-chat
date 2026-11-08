//! `rc` — the Rocket.Chat protocol layer: typed client, domain types and
//! settings. The rest of the app never sees wire shapes or HTTP details.

mod client;
pub mod live;
mod types;
mod wire;

pub use client::RcClient;
pub use live::{DdpLive, LiveCommand, LiveEvent, websocket_url};
pub use types::{
    AttachedFile, CustomEmoji, Msg, Notification, RcError, RcSettings, Reaction, Room, Session,
};

#[cfg(test)]
pub(crate) mod client_tests;
