//! Background tasks of the application layer, spawned on the compio
//! runtime once a session exists. Each task owns its transport, degrades
//! to a stderr note on failure, and never blocks the command loop.

use std::time::Duration;

use async_channel::Sender;

use compio::buf::BufResult;

use crate::app::ChatEvent;
use crate::http::HttpTransport;
use crate::rc::{DdpLive, LiveCommand, LiveEvent, RcClient, RcSettings, Session, websocket_url};
use crate::store::Store;

/// The reconnect backoff step: doubles, capped.
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(30))
}

/// A connection that stayed up this long was healthy: reset the backoff.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// Starts the live WebSocket connection in its own task; returns the
/// control channel. Failures degrade to a stderr note — the REST loop keeps
/// working without the live layer.
pub fn spawn_live(
    settings: RcSettings,
    session: Session,
    events: Sender<ChatEvent>,
) -> Sender<LiveCommand> {
    // LiveCommand is advisory (Watch/Unwatch): a full queue drops the
    // newest watch hint, loud at the caller (m10 policy).
    let (commands, receiver) = async_channel::bounded(16);
    compio::runtime::spawn(async move {
        let url = websocket_url(&settings);
        // The protocol layer emits raw live updates; the application
        // boundary wraps them into chat events.
        // Live updates carry chat content: bounded with blocking
        // sends — backpressure, never drops (m10 policy).
        let (updates, update_rx) = async_channel::bounded(256);
        {
            let events = events.clone();
            compio::runtime::spawn(async move {
                while let Ok(event) = update_rx.recv().await {
                    let chat_event = match event {
                        LiveEvent::Message(message) => ChatEvent::LiveMessage(*message),
                        LiveEvent::Unread { room_id, unread } => {
                            ChatEvent::UnreadCount { room_id, unread }
                        }
                        LiveEvent::Notification(note) => ChatEvent::Notification(note),
                    };
                    if events.send(chat_event).await.is_err() {
                        break;
                    }
                }
            })
            .detach(); // lives as long as the channel; the loop below retries
        }

        // Reconnect forever with exponential backoff; the watched room
        // resubscribes on every new connection (remembered across
        // attempts here — the app re-sends Watch only on user clicks).
        let mut backoff = Duration::from_secs(1);
        let mut watched: Option<String> = None;
        let _ = events.send(ChatEvent::LiveStatus(false)).await;
        loop {
            // Logout or a 401 drops the sender: the channel closes and
            // this task must stop instead of reconnecting with a dead or
            // foreign session.
            if receiver.is_closed() {
                updates.close();
                break;
            }
            let started = std::time::Instant::now();
            // The DDP handshake has no built-in deadline; without this
            // wrap a silent server parks the live layer forever (observed
            // once in the wild) instead of falling through to backoff.
            let attempt = compio::runtime::time::timeout(
                Duration::from_secs(15),
                DdpLive::connect(&url, &session),
            );
            match attempt.await {
                Err(_elapsed) => {
                    eprintln!(
                        "blazer-chat: live connect timed out, retrying in {}s",
                        backoff.as_secs()
                    );
                    compio::runtime::time::sleep(backoff).await;
                    backoff = next_backoff(backoff);
                    continue;
                }
                Ok(result) => match result {
                    Ok(mut live) => {
                        if let Some(room) = watched.clone()
                            && let Err(error) = live.watch(&room).await
                        {
                            eprintln!("blazer-chat: resubscribe failed: {error}");
                        }
                        let _ = events.send(ChatEvent::LiveStatus(true)).await;
                        let result = live
                            .run(updates.clone(), receiver.clone(), Duration::from_secs(30))
                            .await;
                        if let Some(room) = live.watched() {
                            watched = Some(room.to_owned());
                        }
                        if started.elapsed() >= STABLE_CONNECTION {
                            backoff = Duration::from_secs(1);
                        }
                        eprintln!(
                            "blazer-chat: live connection lost ({result:?}), retrying in {}s",
                            backoff.as_secs()
                        );
                        let _ = events.send(ChatEvent::LiveStatus(false)).await;
                        compio::runtime::time::sleep(backoff).await;
                        backoff = next_backoff(backoff);
                    }
                    Err(error) => {
                        eprintln!(
                            "blazer-chat: live connect failed ({error}), retrying in {}s",
                            backoff.as_secs()
                        );
                        compio::runtime::time::sleep(backoff).await;
                        backoff = next_backoff(backoff);
                    }
                },
            }
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
                eprintln!("blazer-chat: emoji list unavailable: {error}");
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
                eprintln!("blazer-chat: {error}");
                return;
            }
        };
        for emoji in &merged {
            // A logout mid-run invalidates the session: stop burning
            // requests for an account that is gone (m10, sec-flow #9).
            if store.load_session().is_none() {
                eprintln!("blazer-chat: emoji cache: session gone, stopping");
                return;
            }
            // Server-supplied name/extension join into a filesystem
            // path: without sanitizing, a `../../.bashrc` name would
            // overwrite arbitrary user files (audit sec-flow #1 /
            // sec-local F1). The display key keeps the raw name.
            let file_name = crate::store::sanitize(&format!("{}.{}", emoji.name, emoji.extension));
            let path = dir.join(file_name);
            if path.exists() {
                continue;
            }
            match client.emoji_image(&emoji.name, &emoji.extension).await {
                Ok(bytes) => {
                    // compio::fs::write takes the buffer and hands it
                    // back inside the BufResult (m10).
                    let BufResult(result, _) = compio::fs::write(&path, bytes).await;
                    if let Err(error) = result {
                        eprintln!("blazer-chat: emoji image {}: {error}", path.display());
                    }
                }
                Err(error) => eprintln!("blazer-chat: emoji {}: {error}", emoji.name),
            }
        }
        if let Err(error) = store.save_emoji(&merged) {
            eprintln!("blazer-chat: emoji cache not stored: {error}");
        }

        let emojis: Vec<(String, String)> = merged
            .into_iter()
            .map(|emoji| {
                let file_name =
                    crate::store::sanitize(&format!("{}.{}", emoji.name, emoji.extension));
                let path = dir.join(file_name);
                (emoji.name, path.display().to_string())
            })
            .collect();
        let _ = events.send(ChatEvent::Emoji { emojis }).await;
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps_at_thirty_seconds() {
        let mut step = Duration::from_secs(1);
        for expected in [2, 4, 8, 16, 30, 30, 30] {
            step = next_backoff(step);
            assert_eq!(step, Duration::from_secs(expected));
        }
    }
}
