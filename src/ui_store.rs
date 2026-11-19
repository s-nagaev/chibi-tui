//! Tiny TUI-owned store for UI layout preferences.
//!
//! Today it holds exactly one user-tunable key: the sidebar width picked by
//! dragging the divider (`App::sidebar_width`), saved on drag end and
//! restored (clamped) at startup, so the layout the user shaped survives
//! restarts. The document is one small JSON file under the same root as
//! `backend.json` (`<data>/chibi-tui/ui.json`, `CHIBI_TUI_HOME` override,
//! same resolution as [`crate::backend_store`]). I/O failures degrade the
//! same way: save failures are non-fatal, an unreadable/corrupt file reads
//! as the default width.

use std::io;
use std::path::Path;

use crate::ui::DEFAULT_SIDEBAR_WIDTH;

/// The single persisted key: sidebar width in columns.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoreDoc {
    sidebar_width: u16,
}

/// Storage root: `CHIBI_TUI_HOME` override → platform data dir → local
/// fallback. Mirrors `backend_store::resolve_root`.
pub fn resolve_root() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("CHIBI_TUI_HOME") {
        if !home.trim().is_empty() {
            return std::path::PathBuf::from(home);
        }
    }
    dirs::data_dir()
        .map(|p| p.join("chibi-tui"))
        .unwrap_or_else(|| std::path::PathBuf::from(".chibi-tui"))
}

/// Store document path under an explicit root (test seam: keeps tests off
/// process env).
pub fn store_path(root: &Path) -> std::path::PathBuf {
    root.join("ui.json")
}

/// Read the persisted sidebar width from an explicit root. A missing,
/// unreadable, corrupt, or key-less file yields the DEFAULT (never an
/// error): the layout simply falls back to the built-in width.
pub fn load_sidebar_width_at(root: &Path) -> u16 {
    let text = std::fs::read_to_string(store_path(root)).ok();
    text.and_then(|t| serde_json::from_str::<StoreDoc>(&t).ok())
        .map(|doc| doc.sidebar_width)
        .unwrap_or(DEFAULT_SIDEBAR_WIDTH)
}

/// Persist the sidebar width under an explicit root. Atomic-ish: temp file
/// + rename, mirroring the backend store discipline.
pub fn save_sidebar_width_at(root: &Path, width: u16) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    let doc = StoreDoc {
        sidebar_width: width,
    };
    let body = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned());
    let target = store_path(root);
    let tmp = root.join("ui.json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, target)
}

/// Read the persisted sidebar width from the default root.
pub fn load_sidebar_width() -> u16 {
    load_sidebar_width_at(&resolve_root())
}

/// Persist the sidebar width under the default root. Best-effort: a failed
/// save must never take down the drag (the session keeps the width in
/// memory); only the NEXT launch loses the choice.
pub fn save_sidebar_width(width: u16) {
    let _ = save_sidebar_width_at(&resolve_root(), width);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-ui-store-test-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_the_sidebar_width() {
        let root = temp_root("roundtrip");
        assert_eq!(
            load_sidebar_width_at(&root),
            DEFAULT_SIDEBAR_WIDTH,
            "no file yet → default"
        );
        save_sidebar_width_at(&root, 44).unwrap();
        assert_eq!(load_sidebar_width_at(&root), 44);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn corrupt_or_key_less_store_reads_as_the_default() {
        let root = temp_root("corrupt");
        std::fs::write(store_path(&root), "not json at all").unwrap();
        assert_eq!(load_sidebar_width_at(&root), DEFAULT_SIDEBAR_WIDTH);
        std::fs::write(store_path(&root), "{}").unwrap();
        assert_eq!(
            load_sidebar_width_at(&root),
            DEFAULT_SIDEBAR_WIDTH,
            "valid JSON without the key → default"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn overwrite_replaces_the_previous_entry() {
        let root = temp_root("overwrite");
        save_sidebar_width_at(&root, 30).unwrap();
        save_sidebar_width_at(&root, 38).unwrap();
        assert_eq!(load_sidebar_width_at(&root), 38);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
