//! Modal error popup state and rendering-independent logic.
//!
//! A single global popup slot: when set, it overlays the whole UI, captures
//! the `R` / `Esc` / `q` / `Ctrl+C` keys (reconnect / dismiss / quit) and blocks normal input
//! handling until dismissed. Backend failures (spawn failure, broken pipe,
//! lost handshake) surface here instead of crashing or being silently
//! dropped.

/// One shown error: the message plus the recovery hint line.
#[derive(Clone, Debug)]
pub struct ErrorPopup {
    /// Human-readable failure description.
    pub message: String,
}

impl ErrorPopup {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Static recovery hint rendered under the message.
    pub fn hint() -> &'static str {
        "R reconnect \u{00b7} Esc dismiss \u{00b7} q/Ctrl+C quit"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_carries_message_and_hint() {
        let p = ErrorPopup::new("backend exploded");
        assert_eq!(p.message, "backend exploded");
        assert!(ErrorPopup::hint().contains('R'));
        assert!(ErrorPopup::hint().contains("quit"));
    }
}
