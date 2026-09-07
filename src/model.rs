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
    /// Model that produced an assistant answer.
    ///
    /// Stamped per-message at result-resolution time, because model
    /// switching must not mislabel earlier replies. Persisted with the
    /// snapshot additively — `#[serde(default, skip_serializing_if)]`
    /// keeps pre-label files parsing and label-less rows byte-compatible —
    /// so a restored transcript keeps the model that actually produced
    /// each answer instead of inheriting the thread's current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Message {
    pub fn user(markdown: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            markdown: markdown.into(),
            pending: false,
            model: None,
        }
    }

    pub fn assistant(markdown: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            markdown: markdown.into(),
            pending: false,
            model: None,
        }
    }

    /// Assistant reply that knows which model produced it.
    pub fn assistant_with_model(markdown: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            markdown: markdown.into(),
            pending: false,
            model: Some(model.into()),
        }
    }

    /// Placeholder for a future assistant reply.
    pub fn assistant_pending() -> Self {
        Self {
            role: Role::Assistant,
            markdown: String::new(),
            pending: true,
            model: None,
        }
    }

    /// Visible marker for a prompt waiting in the chat's FIFO queue
    /// (per-thread async). Rendered like a pending row; replaced by the real
    /// pending answer once the prompt leaves the queue.
    pub fn assistant_queued(position: usize) -> Self {
        Self {
            role: Role::Assistant,
            markdown: format!("\u{23f3} queued (#{position})"),
            pending: true,
            model: None,
        }
    }

    /// Storage view of a message: pending placeholders become empty assistant
    /// messages so a restored history never shows a stuck spinner row.
    ///
    /// The per-message model label travels along: a mid-chat switch must
    /// not re-label old answers, so each answer
    /// freezes the model that produced it. Additive on disk: the serde
    /// attributes on [`Self::model`] keep snapshots without a label
    /// byte-compatible with the pre-label format, and old files without
    /// the key reload plain.
    pub fn normalized_for_storage(&self) -> Self {
        if self.pending {
            Message::assistant("")
        } else {
            self.clone()
        }
    }

    /// Display label for the role header: `Some("glm-5.2")` renders as
    /// `● Chibi (glm-5.2)`; `None` keeps the plain `● Chibi`.
    pub fn model_label(&self) -> Option<&str> {
        self.model.as_deref().filter(|m| !m.trim().is_empty())
    }
}

/// Per-chat request lifecycle (feature: per-thread async).
///
/// Each [`crate::app::Chat`] owns exactly one lifecycle: at most one request
/// is in flight per chat (`Awaiting` = sent, backend has not confirmed
/// processing yet; `Running` = backend confirmed), while additional prompts
/// wait in the chat's FIFO queue. Chats never observe each other's
/// lifecycle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ChatLifecycle {
    /// Nothing in flight and nothing queued for this chat.
    #[default]
    Idle,
    /// Request handed to the backend; `running` status not seen yet.
    Awaiting { request_id: String },
    /// Backend confirmed it is processing the request.
    Running { request_id: String },
}

impl ChatLifecycle {
    /// Protocol request id tracked by this lifecycle, if any.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            ChatLifecycle::Idle => None,
            ChatLifecycle::Awaiting { request_id } | ChatLifecycle::Running { request_id } => {
                Some(request_id.as_str())
            }
        }
    }

    /// True while a request is in flight (either phase).
    pub fn is_busy(&self) -> bool {
        !matches!(self, ChatLifecycle::Idle)
    }
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
    fn lifecycle_default_is_idle() {
        assert_eq!(ChatLifecycle::default(), ChatLifecycle::Idle);
        assert!(!ChatLifecycle::Idle.is_busy());
        assert!(ChatLifecycle::Idle.request_id().is_none());
    }

    #[test]
    fn lifecycle_request_id_and_busy_flags() {
        let awaiting = ChatLifecycle::Awaiting {
            request_id: "r1".to_owned(),
        };
        let running = ChatLifecycle::Running {
            request_id: "r2".to_owned(),
        };
        for (lifecycle, id) in [(&awaiting, "r1"), (&running, "r2")] {
            assert!(lifecycle.is_busy());
            assert_eq!(lifecycle.request_id(), Some(id));
        }
    }

    #[test]
    fn role_survives_json_roundtrip() {
        let role = Role::Assistant;
        assert_eq!(
            role,
            serde_json::from_str::<Role>(&serde_json::to_string(&role).unwrap()).unwrap()
        );
    }

    // per-message model metadata ---------------

    #[test]
    fn model_label_is_absent_by_default_and_trim_guarded() {
        assert_eq!(Message::assistant("hi").model_label(), None);
        assert_eq!(
            Message::assistant_with_model("hi", "glm-5.2").model_label(),
            Some("glm-5.2")
        );
        // Whitespace-only metadata must not render as "()" — treated absent.
        assert_eq!(
            Message::assistant_with_model("hi", "   ").model_label(),
            None
        );
    }

    /// Backward compatibility: snapshots written before the label existed
    /// (no `model` key) must still deserialize.
    #[test]
    fn message_without_model_field_deserializes_from_legacy_json() {
        let legacy = r#"{"role":"Assistant","markdown":"**42**","pending":false}"#;
        let msg: Message = serde_json::from_str(legacy).expect("legacy message parses");
        assert_eq!(msg.model_label(), None);
        assert_eq!(msg.markdown, "**42**");
    }

    /// Storage keeps the label: the answering model is per-message history
    /// now — a mid-chat switch must not re-label old answers, and a restart
    /// must not erase who answered what. Additive on disk: the key only
    /// appears when a label exists, so pre-label snapshots stay
    /// byte-compatible.
    #[test]
    fn storage_normalization_keeps_model_label() {
        let live = Message::assistant_with_model("answer", "glm-5.2");
        let stored = live.normalized_for_storage();
        assert_eq!(
            stored.model_label(),
            Some("glm-5.2"),
            "label persists with the message"
        );
        assert_eq!(stored.markdown, "answer");
        assert!(!stored.pending);

        let json = serde_json::to_string(&stored).unwrap();
        assert!(
            json.contains(r#""model":"glm-5.2""#),
            "serialized storage form must carry the model key: {json}"
        );
    }
}
