//! System clipboard access for pasting into the input field.
//!
//! `arboard` is used lazily: the clipboard handle is opened on demand and
//! dropped right away, because on some platforms (notably macOS) a long-lived
//! handle can interfere with other applications' pasteboards. Failures are
//! non-fatal — a missing/locked clipboard must never take down the TUI.

/// Read text from the system clipboard. Returns `None` when the clipboard is
/// unavailable, empty, or holds non-text content.
pub fn get_text() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get_text().ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: reading the clipboard must never panic; it may legitimately
    /// return either side depending on the host environment.
    #[test]
    fn get_text_never_panics() {
        let _ = get_text();
    }
}
