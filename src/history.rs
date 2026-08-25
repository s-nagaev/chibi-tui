//! Local chat history: persist chats as JSON under the app config dir.
//!
//! Storage layout (one JSON document per chat, named by its stable thread
//! id — never by user-visible title, so renames cannot orphan files):
//!
//! ```text
//! $XDG_DATA_HOME/chibi-tui/threads/   (default: ~/.local/share/chibi-tui/threads)
//! └── <thread_uuid>.json
//!     { "name": "...", "id": "<thread_uuid>", "messages": [ … ] }
//! ```
//!
//! `dirs::data_dir()` is chosen over `config_dir()` because this is
//! regenerated state, not configuration. Save failures are non-fatal and only
//! reported on stderr: losing a history snapshot must never take down the TUI.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::Chat;
use crate::model::Message;

/// Fresh stable thread id for a new chat (UUID v4).
pub fn new_thread_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Fresh protocol `request_id` (UUID v4, client-chosen per protocol v1).
pub fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Default storage root: `<data dir>/chibi-tui`.
fn default_root() -> Option<PathBuf> {
    dirs::data_dir().map(|p| p.join("chibi-tui"))
}

/// Resolve the threads directory: explicit override → `$CHIBI_TUI_HOME` →
/// platform data dir. Test seam via env var.
fn resolve_dir(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.join("threads");
    }
    if let Ok(home) = std::env::var("CHIBI_TUI_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join("threads");
        }
    }
    default_root()
        .map(|root| root.join("threads"))
        .unwrap_or_else(|| PathBuf::from(".chibi-tui/threads"))
}

/// One persisted chat document (`<thread_id>.json`).
#[derive(Serialize, Deserialize)]
struct StoredChat {
    name: String,
    id: String,
    messages: Vec<Message>,
}

impl From<&Chat> for StoredChat {
    fn from(chat: &Chat) -> Self {
        Self {
            name: chat.name.clone(),
            id: chat.id.clone(),
            // Pending placeholders are never persisted as pending: a chat
            // saved mid-request reloads with a resolved (empty) answer row.
            messages: chat
                .messages
                .iter()
                .map(|m| m.normalized_for_storage())
                .collect(),
        }
    }
}

impl From<StoredChat> for Chat {
    fn from(stored: StoredChat) -> Self {
        Self {
            name: stored.name,
            id: stored.id,
            messages: stored.messages,
        }
    }
}

/// Save one chat as `<threads>/<chat.id>.json`. Creates directories on
/// demand; overwrites any previous snapshot of the same chat.
pub fn save_chat(chat: &Chat) -> std::io::Result<PathBuf> {
    save_chat_in(None, chat)
}

/// [`save_chat`] with an explicit directory override (tests / custom layout).
pub fn save_chat_in(override_dir: Option<&Path>, chat: &Chat) -> std::io::Result<PathBuf> {
    let dir = resolve_dir(override_dir);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", sanitize_file_name(&chat.id)));
    let json = serde_json::to_string_pretty(&StoredChat::from(chat))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    fs::write(&path, json)?;
    Ok(path)
}

/// Load every persisted chat, ordered by file name (UUIDs are random, so this
/// is not recency order — it is merely deterministic). Corrupt or unreadable
/// files are skipped silently: one bad snapshot must not hide the rest.
/// Chats without an id get one assigned so they can be re-saved later.
pub fn load_chats() -> Vec<Chat> {
    load_chats_from(None)
}

/// [`load_chats`] with an explicit directory override (tests / custom layout).
pub fn load_chats_from(override_dir: Option<&Path>) -> Vec<Chat> {
    let dir = resolve_dir(override_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new(); // first run: nothing stored yet
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|path| {
            let raw = fs::read_to_string(path).ok()?;
            match serde_json::from_str::<StoredChat>(&raw) {
                Ok(mut stored) => {
                    if stored.id.trim().is_empty() {
                        stored.id = new_thread_id();
                    }
                    Some(Chat::from(stored))
                }
                Err(_) => None,
            }
        })
        .collect()
}

/// File-name-safe form of an id (defensive: ids are UUIDs today, but a stray
/// path separator must never escape the threads directory).
fn sanitize_file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-history-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp root created");
        dir
    }

    fn sample_chat(name: &str) -> Chat {
        let mut chat = Chat::new(name);
        chat.messages.push(Message::user("question"));
        chat.messages.push(Message::assistant("**answer**"));
        chat
    }

    #[test]
    fn save_then_load_roundtrip_preserves_chats() {
        let root = temp_root("roundtrip");
        let mut chats = vec![sample_chat("alpha"), sample_chat("beta")];
        chats[1].messages.push(Message::assistant_pending());

        for chat in &chats {
            save_chat_in(Some(&root), chat).expect("save");
        }

        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded.len(), 2);
        // Files are keyed by random UUIDs, so look chats up by stable id
        // instead of relying on save/load order.
        let by_id = |id: &str| {
            loaded
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("chat {id} not restored"))
        };
        let alpha = by_id(&chats[0].id);
        assert_eq!(alpha.name, "alpha");
        assert_eq!(alpha.messages.len(), 2);
        assert_eq!(alpha.messages[1].markdown, "**answer**");
        let beta = by_id(&chats[1].id);
        assert_eq!(beta.name, "beta");
    }

    #[test]
    fn pending_placeholder_is_persisted_as_resolved_answer() {
        let root = temp_root("pending");
        let mut chat = sample_chat("mid-flight");
        chat.messages.push(Message::assistant_pending());
        save_chat_in(Some(&root), &chat).expect("save");

        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].name, "mid-flight",
            "pending placeholder not persisted"
        );
        let msgs = &loaded[0].messages;
        assert!(msgs.iter().all(|m| !m.pending), "no stuck spinners");
        // The placeholder becomes an empty assistant row, keeping alignment.
        assert_eq!(msgs.last().unwrap().role, Role::Assistant);
        assert_eq!(msgs.last().unwrap().markdown, "");
    }

    #[test]
    fn resave_overwrites_snapshot_for_same_thread_id() {
        let root = temp_root("overwrite");
        let mut chat = sample_chat("evolving");
        save_chat_in(Some(&root), &chat).expect("first save");
        chat.messages.push(Message::user("follow-up"));
        save_chat_in(Some(&root), &chat).expect("second save");

        let files: Vec<_> = std::fs::read_dir(root.join("threads"))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(files.len(), 1, "one file per thread id");

        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded[0].messages.len(), 3);
    }

    #[test]
    fn corrupt_files_are_skipped_not_fatal() {
        let root = temp_root("corrupt");
        save_chat_in(Some(&root), &sample_chat("good")).expect("good save");
        std::fs::write(root.join("threads").join("garbage.json"), "{not valid json")
            .expect("write garbage");

        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded.len(), 1, "only the healthy snapshot loads");
        assert_eq!(loaded[0].name, "good");
    }

    #[test]
    fn empty_or_missing_directory_loads_as_no_history() {
        let root = temp_root("missing");
        assert!(load_chats_from(Some(&root)).is_empty());

        let fresh_but_empty = temp_root("empty");
        assert!(load_chats_from(Some(&fresh_but_empty)).is_empty());
    }

    #[test]
    fn new_thread_ids_are_unique_and_uuid_shaped() {
        let a = new_thread_id();
        let b = new_thread_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36, "canonical UUID v4 string form");
        assert!(uuid::Uuid::parse_str(&a).is_ok());
    }

    #[test]
    fn file_names_cannot_escape_the_threads_directory() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "______etc_passwd");
        let root = temp_root("sanitize");
        let mut evil = sample_chat("evil");
        evil.id = "../../escape".to_owned();
        let path = save_chat_in(Some(&root), &evil).expect("saved safely");
        assert!(path.starts_with(root.join("threads")));
        assert_eq!(path.file_name().unwrap(), "______escape.json");
    }
}
