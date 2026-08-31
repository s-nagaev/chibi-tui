//! Modal error popup state: one global slot that overlays the UI and captures
//! the recovery keys (`R` / `Esc` / `q` / `Ctrl+C`) until dismissed.

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
