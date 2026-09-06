//! Local chat history: persist chats as JSON under the app config dir.
//!
//! Storage layout (one JSON document per chat, named by its stable thread
//! id — never by user-visible title, so renames cannot orphan files; plus a
//! pointer recording the thread that was last on screen):
//!
//! ```text
//! $XDG_DATA_HOME/chibi-tui/           (default: ~/.local/share/chibi-tui)
//! ├── threads/
//! │   └── <thread_uuid>.json
//! │       { "name": "...", "id": "<thread_uuid>", "messages": [ … ] }
//! └── last-thread.json
//!     { "thread_id": "<thread_uuid>" }
//! ```
//!
//! The pointer is rewritten atomically on every thread activation, so even
//! a hard crash remembers what the user was reading. On startup the app
//! re-opens that thread when both the pointer and the thread's snapshot
//! survive; a missing, unreadable or dangling pointer silently falls back
//! to the default startup selection.
//!
//! Threads also carry their last known turn usage and model label as the
//! optional `last_usage` / `last_model` keys once a snapshot records them;
//! snapshots from before those keys existed (and threads with nothing
//! recorded yet) omit them entirely and parse unchanged, so the format is
//! strictly additive. There is no backfill: nothing is ever invented for
//! threads whose history predates the fields.
//!
//! `dirs::data_dir()` is chosen over `config_dir()` because this is
//! regenerated state, not configuration. Save failures are non-fatal and only
//! reported on stderr: losing a history snapshot must never take down the TUI.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::Chat;
use crate::model::Message;
use crate::protocol::Usage;

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

/// Resolve the storage root: explicit override → `$CHIBI_TUI_HOME` →
/// platform data dir. Test seam via env var. Everything the app persists
/// (thread snapshots, the last-active-thread pointer) lives under this root.
fn resolve_root(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    if let Ok(home) = std::env::var("CHIBI_TUI_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    default_root().unwrap_or_else(|| PathBuf::from(".chibi-tui"))
}

/// Threads directory under the storage root.
fn resolve_dir(override_dir: Option<&Path>) -> PathBuf {
    resolve_root(override_dir).join("threads")
}

/// One persisted chat document (`<thread_id>.json`).
#[derive(Serialize, Deserialize)]
struct StoredChat {
    name: String,
    id: String,
    messages: Vec<Message>,
    /// Thread's last known turn usage, restored into the ctx segment on the
    /// next launch. Strictly additive optional field: `#[serde(default)]`
    /// keeps snapshots from before the field parsing (a missing key loads as
    /// `None`, never a thread lost from the sidebar) and
    /// `skip_serializing_if` keeps snapshots with nothing recorded
    /// byte-compatible with the legacy format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_usage: Option<Usage>,
    /// Thread's last known model label, restored into the panel readout on
    /// the next launch. Same additive compatibility contract as
    /// [`StoredChat::last_usage`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_model: Option<String>,
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
            last_usage: chat.last_usage,
            last_model: chat.last_model.clone(),
        }
    }
}

impl From<StoredChat> for Chat {
    fn from(stored: StoredChat) -> Self {
        Self {
            name: stored.name,
            id: stored.id,
            messages: stored.messages,
            // Restored chats always start idle: a snapshot saved mid-request
            // has no live request to resume (see Message::normalized_for_storage).
            lifecycle: crate::model::ChatLifecycle::Idle,
            queue: std::collections::VecDeque::new(),
            // feat_sidebar_unread_marker: the marker is session-only and
            // stays out of the persisted format entirely.
            unread: false,
            last_usage: stored.last_usage,
            last_model: stored.last_model,
            // tui_subagents_b5: live subagent counters are session-only —
            // a restart starts every thread with an empty counter map.
            subagent_counts: std::collections::HashMap::new(),
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

/// Delete one chat's persisted history snapshot, keyed by its stable thread
/// id (feat_thread_delete). A missing file is treated as success — deleting
/// an already-deleted or never-saved chat is idempotent, never an error.
pub fn delete_chat_file(chat_id: &str) -> std::io::Result<()> {
    delete_chat_file_in(None, chat_id)
}

/// [`delete_chat_file`] with an explicit directory override (tests).
pub fn delete_chat_file_in(override_dir: Option<&Path>, chat_id: &str) -> std::io::Result<()> {
    let dir = resolve_dir(override_dir);
    let path = dir.join(format!("{}.json", sanitize_file_name(chat_id)));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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

/// File name of the last-active-thread pointer inside the storage root.
const LAST_THREAD_FILE: &str = "last-thread.json";

/// One persisted last-active-thread pointer (`last-thread.json`).
#[derive(Serialize, Deserialize)]
struct LastThreadPointer {
    thread_id: String,
}

/// Record the active thread id. Written on every activation — not only at
/// shutdown — so a crash still remembers the thread the user was reading;
/// the write is atomic (temp file + rename) so a torn write can never leave
/// a half pointer. An empty id is a silent no-op: nothing to remember.
pub fn save_last_thread(thread_id: &str) -> std::io::Result<()> {
    save_last_thread_in(None, thread_id)
}

/// [`save_last_thread`] with an explicit directory override (tests / custom
/// layout).
pub fn save_last_thread_in(override_dir: Option<&Path>, thread_id: &str) -> std::io::Result<()> {
    if thread_id.trim().is_empty() {
        return Ok(());
    }
    let root = resolve_root(override_dir);
    fs::create_dir_all(&root)?;
    let json = serde_json::to_string(&LastThreadPointer {
        thread_id: thread_id.to_owned(),
    })
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    let path = root.join(LAST_THREAD_FILE);
    let tmp = root.join(format!(
        ".{LAST_THREAD_FILE}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let stored = fs::write(&tmp, json).and_then(|()| fs::rename(&tmp, &path));
    if stored.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    stored
}

/// Load the last active thread id; `None` when the pointer is missing,
/// unreadable, malformed, or empty. A bad pointer must never disturb the
/// default startup behavior — restoration treats it as first-run silence.
pub fn load_last_thread() -> Option<String> {
    load_last_thread_in(None)
}

/// [`load_last_thread`] with an explicit directory override (tests / custom
/// layout).
pub fn load_last_thread_in(override_dir: Option<&Path>) -> Option<String> {
    let raw = fs::read_to_string(resolve_root(override_dir).join(LAST_THREAD_FILE)).ok()?;
    let pointer: LastThreadPointer = serde_json::from_str(&raw).ok()?;
    let id = pointer.thread_id.trim();
    (!id.is_empty()).then(|| id.to_owned())
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

    /// tui_thoughts_b1: LLM reasoning is session-only and must NEVER reach
    /// storage. The document is inspected RAW (not just via the loader) so
    /// the test would catch even a serde-visible thoughts field appearing
    /// in the file format; the exact key set proves the structure can carry
    /// nothing else.
    #[test]
    fn thoughts_never_persist_to_history_files() {
        let root = temp_root("thoughts");
        let mut chat = sample_chat("reasoned");
        chat.messages.push(Message::assistant("plain answer"));

        let path = save_chat_in(Some(&root), &chat).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read snapshot");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        let mut keys: Vec<_> = doc
            .as_object()
            .expect("top-level object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["id", "messages", "name"],
            "persisted format carries nothing beyond id/name/messages: {raw}"
        );
        assert!(
            !raw.contains("thought"),
            "persisted history must not contain thoughts: {raw}"
        );

        // Restart semantics: a reload restores messages only — no thoughts
        // anywhere.
        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].messages.len(), 3);
        assert!(
            loaded[0]
                .messages
                .iter()
                .all(|m| !m.markdown.contains("thought")),
            "no thought text survives a restart load"
        );
    }

    /// A thread with nothing recorded yet keeps the legacy key set exactly:
    /// the optional last-known usage/model keys are absent until real data
    /// arrives, and no backfill ever invents them.
    #[test]
    fn fresh_chat_persists_without_last_known_fields() {
        let root = temp_root("fresh-last");
        let path = save_chat_in(Some(&root), &sample_chat("fresh")).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read snapshot");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        let keys: Vec<_> = doc
            .as_object()
            .expect("top-level object")
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            vec!["id", "messages", "name"],
            "a thread with nothing recorded keeps the legacy key set: {raw}"
        );
    }

    /// Backward compatibility contract: a snapshot written before the
    /// last-known usage/model fields existed parses unchanged, loads with
    /// neither field, and renders the same display state as ever (no ctx
    /// segment, no panel model, plain answer headers).
    #[test]
    fn old_format_file_without_last_known_fields_parses_and_displays_as_today() {
        let root = temp_root("old-format");
        let id = new_thread_id();
        let legacy = format!(
            r#"{{"name":"legacy","id":"{id}","messages":[
                {{"role":"User","markdown":"question","pending":false}},
                {{"role":"Assistant","markdown":"answer","pending":false}}]}}"#
        );
        std::fs::create_dir_all(root.join("threads")).expect("threads dir");
        std::fs::write(root.join("threads").join(format!("{id}.json")), legacy)
            .expect("write legacy snapshot");

        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded.len(), 1, "old files must keep parsing");
        assert_eq!(loaded[0].name, "legacy");
        assert_eq!(loaded[0].messages.len(), 2);
        assert_eq!(loaded[0].last_usage, None, "no invented usage");
        assert_eq!(loaded[0].last_model, None, "no invented model");

        let app = crate::app::App::new(loaded);
        assert_eq!(
            app.last_turn_usage, None,
            "ctx segment unchanged for old files"
        );
        assert_eq!(
            app.active_model_label(),
            None,
            "panel model unchanged for old files"
        );
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

    // ---- feat_rename_thread: rename persistence ----------------------------

    /// The whole point of the rename feature's persistence contract: a saved
    /// renamed chat must come back with the NEW title on the next launch.
    #[test]
    fn renamed_chat_title_survives_save_load_roundtrip() {
        let root = temp_root("rename");
        let mut chat = sample_chat("old title");

        // Simulate commit_rename: trim, then assign.
        chat.name = "  fresh title  ".trim().to_owned();
        save_chat_in(Some(&root), &chat).expect("save renamed chat");

        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].name, "fresh title",
            "renamed title must survive restart"
        );
        // Identity is untouched: same thread id, messages intact.
        assert_eq!(loaded[0].id, chat.id);
        assert_eq!(loaded[0].messages.len(), 2);

        // A second rename overwrites in place (same <thread_id>.json file).
        chat.name = "even newer".to_owned();
        save_chat_in(Some(&root), &chat).expect("resave renamed chat");
        let files: Vec<_> = std::fs::read_dir(root.join("threads"))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(files.len(), 1, "still one file per thread id after rename");
        assert_eq!(load_chats_from(Some(&root))[0].name, "even newer");
    }

    /// Files are keyed by stable thread id — a rename can never orphan the
    /// history snapshot under a new file name.
    #[test]
    fn renaming_keeps_history_file_keyed_by_thread_id() {
        let root = temp_root("rename-key");
        let mut chat = sample_chat("before");
        let first_path = save_chat_in(Some(&root), &chat).expect("initial save");
        let initial_file_name = first_path.file_name().unwrap().to_owned();

        chat.name = "after".to_owned();
        let second_path = save_chat_in(Some(&root), &chat).expect("save after rename");

        assert_eq!(
            first_path.file_name().unwrap(),
            second_path.file_name().unwrap(),
            "rename must not move or duplicate the snapshot file"
        );
        assert_eq!(second_path.file_name().unwrap(), initial_file_name);
        assert_eq!(load_chats_from(Some(&root))[0].name, "after");
    }

    // ---- feat_thread_delete: history file removal ------------------------

    /// Deleting a chat removes its persisted snapshot: the file is gone and
    /// a subsequent load yields nothing.
    #[test]
    fn delete_chat_file_removes_snapshot() {
        let root = temp_root("delete");
        let chat = sample_chat("doomed");
        save_chat_in(Some(&root), &chat).expect("save before delete");
        assert!(!load_chats_from(Some(&root)).is_empty());

        delete_chat_file_in(Some(&root), &chat.id).expect("delete succeeds");
        assert!(
            load_chats_from(Some(&root)).is_empty(),
            "snapshot must be gone after delete"
        );
        // The threads directory itself may remain — harmless.
    }

    /// Idempotency: deleting an already-deleted chat or a thread id that
    /// was never saved is success, never an error.
    #[test]
    fn delete_chat_file_is_idempotent_and_missing_is_ok() {
        let root = temp_root("delete-idem");
        let chat = sample_chat("twice");
        save_chat_in(Some(&root), &chat).expect("save");

        delete_chat_file_in(Some(&root), &chat.id).expect("first delete");
        // File already gone — still Ok.
        delete_chat_file_in(Some(&root), &chat.id).expect("idempotent re-delete");
        // A thread id that was never saved is also a silent no-op.
        delete_chat_file_in(Some(&root), "00000000-0000-0000-0000-00000000dead")
            .expect("never-saved id is a no-op");
    }

    /// Deleting one chat never touches the snapshots of its neighbours.
    #[test]
    fn delete_chat_file_leaves_other_chats_intact() {
        let root = temp_root("delete-others");
        let keep = sample_chat("keeper");
        let doomed = sample_chat("doomed");
        save_chat_in(Some(&root), &keep).expect("save keeper");
        save_chat_in(Some(&root), &doomed).expect("save doomed");

        delete_chat_file_in(Some(&root), &doomed.id).expect("delete doomed");

        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, keep.id, "keeper snapshot untouched");
        assert_eq!(loaded[0].name, "keeper");
    }

    // ---- remember_last_thread: last-active pointer -------------------------

    /// The whole point of the pointer's persistence contract: the recorded
    /// id round-trips through the state file, and every new activation
    /// overwrites the previous one in place.
    #[test]
    fn last_thread_pointer_round_trips_and_overwrites() {
        let root = temp_root("last-thread");
        let a = new_thread_id();
        let b = new_thread_id();

        save_last_thread_in(Some(&root), &a).expect("save pointer");
        assert_eq!(
            load_last_thread_in(Some(&root)).as_deref(),
            Some(a.as_str())
        );

        save_last_thread_in(Some(&root), &b).expect("resave pointer");
        assert_eq!(
            load_last_thread_in(Some(&root)).as_deref(),
            Some(b.as_str()),
            "the latest activation wins"
        );

        // One state file next to the threads directory — never one file per
        // thread — and no temp-file leftovers after the atomic rename.
        let entries: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["last-thread.json".to_owned()],
            "pointer file only, nothing else: {entries:?}"
        );
    }

    /// An empty id records nothing: there is no thread to remember, and a
    /// later load must not invent one.
    #[test]
    fn empty_thread_id_save_is_a_silent_no_op() {
        let root = temp_root("last-thread-empty");
        save_last_thread_in(Some(&root), "   ").expect("no-op save");
        assert!(load_last_thread_in(Some(&root)).is_none());
        assert!(
            !root.join("last-thread.json").exists(),
            "nothing may be written"
        );
    }

    /// Missing, corrupt, wrong-shaped, and empty-id pointers all degrade to
    /// `None` — a bad pointer must be indistinguishable from a first run and
    /// must never panic the startup path.
    #[test]
    fn missing_corrupt_or_empty_pointer_loads_as_none_without_panicking() {
        let root = temp_root("last-thread-bad");
        assert!(load_last_thread_in(Some(&root)).is_none(), "missing file");

        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("last-thread.json"), "{not valid json")
            .expect("write corrupt pointer");
        assert!(load_last_thread_in(Some(&root)).is_none(), "corrupt JSON");

        std::fs::write(root.join("last-thread.json"), r#"{"id":"nope"}"#)
            .expect("write wrong-shape pointer");
        assert!(load_last_thread_in(Some(&root)).is_none(), "wrong shape");

        std::fs::write(root.join("last-thread.json"), r#"{"thread_id":"  "}"#)
            .expect("write empty-id pointer");
        assert!(load_last_thread_in(Some(&root)).is_none(), "empty id");
    }
}
