//! Local persistence (redb, embedded) — session and cached rooms.
//!
//! redb is a synchronous, single-process KV store: no async runtime, which is
//! exactly why it fits the compio layer (surrealdb would drag Tokio in,
//! violating DECL0001). The file lives under `$XDG_DATA_HOME`, defaulting to
//! `~/.local/share`; everything in it is resumable state, never credentials
//! beyond the session token the server issued.
//!
//! Failure policy: persistence is an optimization around the live protocol —
//! an unreadable store degrades to in-memory-only, reported on stderr, and
//! never blocks the chat.

use std::path::PathBuf;

use redb::ReadableDatabase;
use serde::Serialize;

use crate::rc::{Room, Session};

const TABLE: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("kv");
const SESSION_KEY: &str = "session";
const ROOMS_KEY: &str = "rooms";
const EMOJI_KEY: &str = "emoji";

pub struct Store {
    db: redb::Database,
}

#[derive(serde::Deserialize)]
struct StoredRooms {
    rooms: Vec<Room>,
}

#[derive(Serialize)]
struct StoredRoomsRef<'a> {
    rooms: &'a [Room],
}

impl Store {
    /// Opens (creating if needed) the store file.
    pub fn open() -> Result<Self, String> {
        let path = store_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let db = redb::Database::create(&path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        {
            let setup = db.begin_write().map_err(|error| error.to_string())?;
            setup.open_table(TABLE).map_err(|error| error.to_string())?;
            setup.commit().map_err(|error| error.to_string())?;
        }
        Ok(Self { db })
    }

    fn read(&self, key: &str) -> Option<Vec<u8>> {
        let read = self.db.begin_read().ok()?;
        let table = read.open_table(TABLE).ok()?;
        table.get(key).ok()?.map(|value| value.value().to_vec())
    }

    fn write(&self, key: &str, value: &[u8]) -> Result<(), String> {
        let write = self.db.begin_write().map_err(|error| error.to_string())?;
        {
            let mut table = write.open_table(TABLE).map_err(|error| error.to_string())?;
            table
                .insert(key, value)
                .map_err(|error| error.to_string())?;
        }
        write.commit().map_err(|error| error.to_string())
    }

    fn remove(&self, key: &str) -> Result<(), String> {
        let write = self.db.begin_write().map_err(|error| error.to_string())?;
        {
            let mut table = write.open_table(TABLE).map_err(|error| error.to_string())?;
            table.remove(key).map_err(|error| error.to_string())?;
        }
        write.commit().map_err(|error| error.to_string())
    }

    fn put_json<T: Serialize>(&self, key: &str, value: &T) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        self.write(key, &bytes)
    }

    fn get_json<T: for<'de> serde::Deserialize<'de>>(&self, key: &str) -> Option<T> {
        let bytes = self.read(key)?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The resumable session, if one was stored.
    pub fn load_session(&self) -> Option<Session> {
        self.get_json(SESSION_KEY)
    }

    pub fn save_session(&self, session: &Session) -> Result<(), String> {
        self.put_json(SESSION_KEY, session)
    }

    pub fn clear_session(&self) -> Result<(), String> {
        self.remove(SESSION_KEY)
    }

    /// The last known room list, for an instant first paint.
    pub fn load_rooms(&self) -> Option<Vec<Room>> {
        self.get_json::<StoredRooms>(ROOMS_KEY)
            .map(|stored| stored.rooms)
    }

    pub fn save_rooms(&self, rooms: &[Room]) -> Result<(), String> {
        self.put_json(ROOMS_KEY, &StoredRoomsRef { rooms })
    }

    /// The cached custom emojis (the merged delta state).
    pub fn load_emoji(&self) -> Vec<crate::rc::CustomEmoji> {
        self.get_json(EMOJI_KEY).unwrap_or_default()
    }

    pub fn save_emoji(&self, emoji: &[crate::rc::CustomEmoji]) -> Result<(), String> {
        self.put_json(EMOJI_KEY, &emoji)
    }

    /// The directory custom-emoji images live in; created on demand.
    pub fn emoji_dir() -> Result<PathBuf, String> {
        let dir = store_path()?
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let dir = dir.join("emoji");
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        Ok(dir)
    }
}

fn store_path() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(base) => PathBuf::from(base),
        None => {
            let home = std::env::var("HOME")
                .map_err(|_| "cannot resolve the data directory: no XDG_DATA_HOME, no HOME")?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(base.join("rchat-blazar/store.redb"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    fn temp_store() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "rchat-blazar-test-{}-{:?}",
            std::process::id(),
            std::time::Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = redb::Database::create(dir.join("store.redb")).unwrap();
        let store = Store { db };
        let setup = store.db.begin_write().unwrap();
        setup.open_table(TABLE).unwrap();
        setup.commit().unwrap();
        store
    }

    #[test]
    fn session_roundtrip_and_clear() {
        let store = temp_store();
        assert!(store.load_session().is_none());

        let session = Session {
            user_id: "u1".to_owned(),
            auth_token: "tok".to_owned(),
            username: "tester".to_owned(),
        };
        store.save_session(&session).unwrap();
        assert_eq!(store.load_session().unwrap().auth_token, "tok");

        store.clear_session().unwrap();
        assert!(store.load_session().is_none());
    }

    #[test]
    fn rooms_roundtrip() {
        let store = temp_store();
        assert!(store.load_rooms().is_none());

        let rooms = vec![Room {
            id: "GENERAL".to_owned(),
            name: "general".to_owned(),
            kind: 'c',
            unread: 0,
        }];
        store.save_rooms(&rooms).unwrap();
        assert_eq!(store.load_rooms().unwrap(), rooms);
    }
}
