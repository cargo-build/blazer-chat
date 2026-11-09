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
const MESSAGES_PREFIX: &str = "messages:";
const EMOJI_KEY: &str = "emoji";
const DRAFTS_KEY: &str = "drafts";

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
        Self::open_at(store_path()?)
    }

    /// Opens the store at an explicit path — the seam the permission
    /// regression test uses.
    fn open_at(path: PathBuf) -> Result<Self, String> {
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
        // The store keeps the session token in plaintext: owner-only on
        // the file and the directory, every open, so a store created by
        // an older build is tightened too (audit sec-local F2).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot chmod 600 {}: {error}", path.display()))?;
        if let Some(dir) = path.parent() {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("cannot chmod 700 {}: {error}", dir.display()))?;
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

    /// The cached newest history slice for a room: instant room opens
    /// paint this before the network refresh lands.
    pub fn load_messages(&self, room_id: &str) -> Option<Vec<crate::rc::Msg>> {
        self.get_json(&format!("{MESSAGES_PREFIX}{room_id}"))
    }

    pub fn save_messages(&self, room_id: &str, messages: &[crate::rc::Msg]) -> Result<(), String> {
        self.put_json(&format!("{MESSAGES_PREFIX}{room_id}"), &messages)
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

    /// Directory for downloaded attachment previews.
    pub fn attachments_dir() -> Result<PathBuf, String> {
        let dir = store_path()?
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let dir = dir.join("attachments");
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        Ok(dir)
    }

    /// Cached avatar image for a username, if fetched before. No expiry:
    /// avatars change rarely; a fresh fetch happens when the file is
    /// absent.
    pub fn avatar_path(&self, username: &str) -> Result<Option<PathBuf>, String> {
        let dir = Self::attachments_dir()?
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default()
            .join("avatars");
        let path = dir.join(sanitize(username));
        Ok(path.is_file().then_some(path))
    }

    /// Persists an avatar's bytes; returns the path for the UI.
    pub fn save_avatar(&self, username: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let dir = Self::attachments_dir()?
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default()
            .join("avatars");
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let path = dir.join(sanitize(username));
        std::fs::write(&path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        Ok(path)
    }

    /// Persists one room's draft; empty text removes the entry (m12
    /// F13).
    pub fn save_draft(&self, room_id: &str, text: &str) -> Result<(), String> {
        let mut drafts = self.load_drafts()?;
        if text.is_empty() {
            drafts.remove(room_id);
        } else {
            drafts.insert(room_id.to_owned(), text.to_owned());
        }
        let bytes = serde_json::to_vec(&drafts).map_err(|error| error.to_string())?;
        let write = self.db.begin_write().map_err(|error| error.to_string())?;
        {
            let mut table = write.open_table(TABLE).map_err(|error| error.to_string())?;
            table
                .insert(DRAFTS_KEY, bytes.as_slice())
                .map_err(|error| error.to_string())?;
        }
        write.commit().map_err(|error| error.to_string())
    }

    /// All persisted drafts (room → text), loaded once at session
    /// start (m12 F13).
    pub fn load_drafts(&self) -> Result<std::collections::HashMap<String, String>, String> {
        let read = self.db.begin_read().map_err(|error| error.to_string())?;
        let table = read.open_table(TABLE).map_err(|error| error.to_string())?;
        match table.get(DRAFTS_KEY).map_err(|error| error.to_string())? {
            Some(value) => serde_json::from_slice(value.value()).map_err(|error| error.to_string()),
            None => Ok(std::collections::HashMap::new()),
        }
    }

    /// Persists an attachment's bytes under the cache; returns the path
    /// for the UI. Sanitized `{file_id}-{name}` keeps rooms separate.
    pub fn save_attachment(
        &self,
        room_id: &str,
        file_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        let dir = Self::attachments_dir()?.join(sanitize(room_id));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let safe_name = sanitize(name);
        let path = dir.join(format!("{file_id}-{safe_name}"));
        std::fs::write(&path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        Ok(path)
    }
}

/// Filesystem-safe component (one owner: room ids, file names, emoji
/// cache names).
/// Never empty and never a dot segment: `dir.join("..")` would escape
/// the directory (audit sec-flow #6).
pub(crate) fn sanitize(value: &str) -> String {
    let mapped: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match mapped.as_str() {
        "" | "." | ".." => "default".to_owned(),
        _ => mapped,
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
    Ok(base.join("blazer-chat/store.redb"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    fn temp_store() -> Store {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "blazer-chat-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    fn attachments_persist_under_their_room() {
        let store = temp_store();
        let path = store
            .save_attachment("GENERAL", "abc123", "probe image.png", b"png-bytes")
            .unwrap();
        assert!(
            path.to_string_lossy()
                .contains("GENERAL/abc123-probe_image.png")
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"png-bytes");
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
            lm: None,
            preview: None,
            topic: None,
            mentions: 0,
        }];
        store.save_rooms(&rooms).unwrap();
        assert_eq!(store.load_rooms().unwrap(), rooms);
    }

    #[test]
    fn messages_roundtrip_per_room() {
        let store = temp_store();
        assert!(store.load_messages("GENERAL").is_none());
        let messages = vec![crate::rc::Msg {
            id: "m1".to_owned(),
            room_id: "GENERAL".to_owned(),
            sender: "tester".to_owned(),
            text: "cached".to_owned(),
            ts: "2026-11-07T20:00:00Z".to_owned(),
            tmid: None,
            tcount: None,
            tlm: None,
            file: None,
            reactions: Default::default(),
            starred_by_me: false,
            system: false,
        }];
        store.save_messages("GENERAL", &messages).unwrap();
        assert_eq!(
            store.load_messages("GENERAL").as_deref(),
            Some(&messages[..])
        );
    }
    #[test]
    fn sanitize_never_yields_dot_segments() {
        assert_eq!(sanitize(".."), "default");
        assert_eq!(sanitize("."), "default");
        assert_eq!(sanitize(""), "default");
        assert_eq!(sanitize("a/b"), "a_b");
        // Dots inside a longer name are a plain filename, not a segment.
        assert_eq!(sanitize("../../etc/passwd"), ".._.._etc_passwd");
        assert!(!sanitize("../../etc/passwd").contains('/'));
    }

    #[test]
    fn store_file_and_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "blazer-chat-perms-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is past the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir creates");
        let path = dir.join("store.redb");
        Store::open_at(path.clone()).expect("store opens at the temp path");
        let mode = std::fs::metadata(&path)
            .expect("store file exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "store.redb is owner-only");
        let dir_mode = std::fs::metadata(&dir)
            .expect("store dir exists")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "the store directory is owner-only");
    }

    #[test]
    fn drafts_roundtrip_and_delete() {
        let store = temp_store();
        assert!(store.load_drafts().unwrap().is_empty());
        store.save_draft("GENERAL", "half-typed").unwrap();
        store.save_draft("dm1", "other").unwrap();
        let drafts = store.load_drafts().unwrap();
        assert_eq!(
            drafts.get("GENERAL").map(String::as_str),
            Some("half-typed")
        );
        store.save_draft("GENERAL", "").unwrap();
        let drafts = store.load_drafts().unwrap();
        assert!(!drafts.contains_key("GENERAL"), "empty text deletes");
        assert_eq!(drafts.get("dm1").map(String::as_str), Some("other"));
    }
}
