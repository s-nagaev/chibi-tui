//! Lazy `arboard` clipboard read for pasting; failures are non-fatal by design.

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
