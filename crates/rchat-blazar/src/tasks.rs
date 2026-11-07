//! Background tasks of the application layer, spawned on the compio
//! runtime once a session exists. Each task owns its transport, degrades
//! to a stderr note on failure, and never blocks the command loop.

use async_channel::Sender;

use crate::app::ChatEvent;
use crate::http::HttpTransport;
use crate::rc::{DdpLive, LiveCommand, LiveEvent, RcClient, RcSettings, Session, websocket_url};
use crate::store::Store;

/// Starts the live WebSocket connection in its own task; returns the
/// control channel. Failures degrade to a stderr note — the REST loop keeps
/// working without the live layer.
pub fn spawn_live(
    settings: RcSettings,
    session: Session,
    events: Sender<ChatEvent>,
) -> Sender<LiveCommand> {
    let (commands, receiver) = async_channel::unbounded();
    compio::runtime::spawn(async move {
        let url = websocket_url(&settings);
        match DdpLive::connect(&url, &session).await {
            Ok(live) => {
                // The protocol layer emits raw live updates; the
                // application boundary wraps them into chat events.
                let (updates, update_rx) = async_channel::unbounded();
                let forward = compio::runtime::spawn(async move {
                    while let Ok(event) = update_rx.recv().await {
                        let chat_event = match event {
                            LiveEvent::Message(message) => ChatEvent::LiveMessage(message),
                            LiveEvent::Unread { room_id, unread } => {
                                ChatEvent::UnreadCount { room_id, unread }
                            }
                        };
                        if events.send(chat_event).await.is_err() {
                            break;
                        }
                    }
                });
                // The messages sender moves into run() and drops when the
                // connection ends, which unblocks the forward task below.
                let result = live
                    .run(updates, receiver, std::time::Duration::from_secs(30))
                    .await;
                let _ = forward.await;
                if let Err(error) = result {
                    eprintln!("rchat-blazar: live connection ended: {error}");
                }
            }
            Err(error) => eprintln!("rchat-blazar: live layer unavailable: {error}"),
        }
    })
    .detach(); // background task; failures degrade to the stderr notes above
    commands
}

/// Refreshes the custom-emoji cache in the background: fetch the
/// `emoji-custom.list` delta, merge with the store, download images that
/// have no file yet, then hand the name→path map to the UI. First run
/// downloads every image (minutes, silently); later runs only new ones.
pub fn spawn_emoji(
    settings: RcSettings,
    session: Session,
    store: std::sync::Arc<Store>,
    events: Sender<ChatEvent>,
) {
    compio::runtime::spawn(async move {
        let transport =
            HttpTransport::new(settings.addr(), settings.host_header(), settings.is_tls());
        let mut client = RcClient::with_transport(settings, transport);
        client.set_session(session);

        let fresh = match client.emoji_custom_list().await {
            Ok(fresh) => fresh,
            Err(error) => {
                eprintln!("rchat-blazar: emoji list unavailable: {error}");
                return;
            }
        };

        let mut merged = store.load_emoji();
        for emoji in fresh {
            match merged
                .iter()
                .position(|known: &crate::rc::CustomEmoji| known.id == emoji.id)
            {
                Some(index) => merged[index] = emoji,
                None => merged.push(emoji),
            }
        }

        let dir = match Store::emoji_dir() {
            Ok(dir) => dir,
            Err(error) => {
                eprintln!("rchat-blazar: {error}");
                return;
            }
        };
        for emoji in &merged {
            let path = dir.join(format!("{}.{}", emoji.name, emoji.extension));
            if path.exists() {
                continue;
            }
            match client.emoji_image(&emoji.name, &emoji.extension).await {
                Ok(bytes) => {
                    if let Err(error) = std::fs::write(&path, bytes) {
                        eprintln!("rchat-blazar: emoji image {}: {error}", path.display());
                    }
                }
                Err(error) => eprintln!("rchat-blazar: emoji {}: {error}", emoji.name),
            }
        }
        if let Err(error) = store.save_emoji(&merged) {
            eprintln!("rchat-blazar: emoji cache not stored: {error}");
        }

        let emojis: Vec<(String, String)> = merged
            .into_iter()
            .map(|emoji| {
                let path = dir.join(format!("{}.{}", emoji.name, emoji.extension));
                (emoji.name, path.display().to_string())
            })
            .collect();
        let _ = events.send(ChatEvent::Emoji { emojis }).await;
    })
    .detach();
}
