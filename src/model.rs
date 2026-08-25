//! Core data model shared between the UI and the backend source.

use serde::{Deserialize, Serialize};

/// Role of a message author.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

/// A single chat message. `markdown` is raw CommonMark text.
#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub markdown: String,
    /// In-flight placeholder shown while the assistant answer is pending.
    ///
    /// Never persisted as `true`: a snapshot saved mid-request is reloaded
    /// with the placeholder resolved (see [`Self::normalized_for_storage`]).
    pub pending: bool,
}

impl Message {
    pub fn user(markdown: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            markdown: markdown.into(),
            pending: false,
        }
    }

    pub fn assistant(markdown: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            markdown: markdown.into(),
            pending: false,
        }
    }

    /// Placeholder for a future assistant reply.
    pub fn assistant_pending() -> Self {
        Self {
            role: Role::Assistant,
            markdown: String::new(),
            pending: true,
        }
    }

    /// Storage view of a message: pending placeholders become empty assistant
    /// messages so a restored history never shows a stuck spinner row.
    pub fn normalized_for_storage(&self) -> Self {
        if self.pending {
            Message::assistant("")
        } else {
            self.clone()
        }
    }
}

/// Request lifecycle as observed by the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatStatus {
    Idle,
    Queued,
    Running,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_placeholder_normalizes_to_empty_assistant_message() {
        let m = Message::assistant_pending();
        assert!(m.pending);
        let stored = m.normalized_for_storage();
        assert!(!stored.pending);
        assert_eq!(stored.role, Role::Assistant);
        assert_eq!(stored.markdown, "");
    }

    #[test]
    fn finished_messages_pass_through_unchanged() {
        let user = Message::user("hi");
        assert!(!user.normalized_for_storage().pending);
        let answer = Message::assistant("**bold**");
        let stored = answer.normalized_for_storage();
        assert!(!stored.pending);
        assert_eq!(stored.markdown, "**bold**");
    }

    #[test]
    fn role_and_status_survive_json_roundtrip() {
        let status = ChatStatus::Running;
        assert_eq!(
            status,
            serde_json::from_str::<ChatStatus>(&serde_json::to_string(&status).unwrap()).unwrap()
        );
        let role = Role::Assistant;
        assert_eq!(
            role,
            serde_json::from_str::<Role>(&serde_json::to_string(&role).unwrap()).unwrap()
        );
    }
}
