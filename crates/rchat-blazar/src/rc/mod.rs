//! `rc` — the Rocket.Chat protocol layer: typed client, domain types and
//! settings. The rest of the app never sees wire shapes or HTTP details.

mod client;
pub mod live;
mod types;
mod wire;

pub use client::RcClient;
pub use live::{DdpLive, LiveCommand, websocket_url};
pub use types::{CustomEmoji, Msg, RcError, RcSettings, Room, Session};

#[cfg(test)]
mod client_tests;
