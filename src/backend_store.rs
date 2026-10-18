//! Tiny TUI-owned store for the installed backend binary path.
//!
//! When the setup screen installs `chibi-bot` itself (pipx / TUI venv /
//! `pip --user`), the resulting binary usually lands in a directory that is
//! NOT on the TUI's `PATH` (`~/.local/bin`, an interpreter-specific user
//! scripts dir, a private venv). The spawn path must therefore be able to
//! find the binary again on the next launch, without the user exporting
//! `CHIBI_BACKEND_BIN` by hand.
//!
//! The store is one small JSON document under the history root
//! (`<data>/chibi-tui/backend.json`, `CHIBI_TUI_HOME` override, same
//! resolution as [`crate::history`]). It holds exactly one non-secret key:
//! the absolute path of the verified backend binary. I/O failures degrade:
//! save failures are non-fatal, an unreadable/corrupt file reads as "no
//! persisted path" (the setup screen then simply applies again).
//!
//! The persisted path is NOT `CHIBI_BACKEND_BIN`: the env var stays the
//! user's explicit escape hatch and keeps gating the setup screen; the
//! store only feeds the spawn resolution order
//! (`CHIBI_BACKEND_BIN` → persisted path → `PATH` lookup); see
//! [`crate::request_pipeline`] and [`crate::setup_screen::backend_override`].

use std::io;
use std::path::{Path, PathBuf};

/// The single persisted key: absolute path of the verified backend binary.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoreDoc {
    backend_path: String,
}

/// Storage root: `CHIBI_TUI_HOME` override → platform data dir → local
/// fallback. Mirrors `history.rs::resolve_root` (minus the `--history-dir`
/// CLI override, which this store intentionally does not follow: the
/// installed backend does not move with a redirected history dir).
pub fn resolve_root() -> PathBuf {
    if let Ok(home) = std::env::var("CHIBI_TUI_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    dirs::data_dir()
        .map(|p| p.join("chibi-tui"))
        .unwrap_or_else(|| PathBuf::from(".chibi-tui"))
}

/// Store document path under an explicit root (test seam: keeps tests off
/// process env).
pub fn store_path(root: &Path) -> PathBuf {
    root.join("backend.json")
}

/// Read the persisted binary path from an explicit root. `None` on missing,
/// unreadable, corrupt, or empty entries, never an error.
pub fn load_backend_path_at(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(store_path(root)).ok()?;
    let doc: StoreDoc = serde_json::from_str(&text).ok()?;
    let path = PathBuf::from(doc.backend_path);
    (!path.as_os_str().is_empty()).then_some(path)
}

/// Persist the binary path under an explicit root. Atomic-ish: temp file +
/// rename, mirroring the history snapshot discipline.
pub fn save_backend_path_at(root: &Path, path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    let doc = StoreDoc {
        backend_path: path.display().to_string(),
    };
    let body = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned());
    let target = store_path(root);
    let tmp = root.join("backend.json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, target)
}

/// Read the persisted binary path from the default root.
pub fn load_backend_path() -> Option<PathBuf> {
    load_backend_path_at(&resolve_root())
}

/// Persist the binary path under the default root. Best-effort: a failed
/// save must never take down the installer; the session still works (the
/// binary path was just resolved in memory); only the NEXT launch loses the
/// shortcut and sees the setup screen again.
pub fn save_backend_path(path: &Path) {
    let _ = save_backend_path_at(&resolve_root(), path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-store-test-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_the_backend_path() {
        let root = temp_root("roundtrip");
        assert_eq!(load_backend_path_at(&root), None, "no file yet");
        let bin = root.join("tools").join("chibi");
        save_backend_path_at(&root, &bin).unwrap();
        assert_eq!(load_backend_path_at(&root), Some(bin));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_or_empty_store_reads_as_no_path() {
        let root = temp_root("corrupt");
        std::fs::write(store_path(&root), "not json at all").unwrap();
        assert_eq!(load_backend_path_at(&root), None);
        std::fs::write(store_path(&root), r#"{"backend_path": ""}"#).unwrap();
        assert_eq!(load_backend_path_at(&root), None, "empty path is no path");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn overwrite_replaces_the_previous_entry() {
        let root = temp_root("overwrite");
        save_backend_path_at(&root, Path::new("/old/chibi")).unwrap();
        save_backend_path_at(&root, Path::new("/new/chibi")).unwrap();
        assert_eq!(
            load_backend_path_at(&root),
            Some(PathBuf::from("/new/chibi"))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
