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
    /// Unix seconds when the message was created, rendered as the local
    /// `HH:MM` next to the author name.
    ///
    /// Stamped once, when the bubble appears — a user prompt at submit time,
    /// an assistant row when its placeholder or out-of-band answer is
    /// created — and PRESERVED from there on: resolution keeps the pending
    /// placeholder's instant and the queued→pending handoff keeps the queued
    /// marker's, so a bubble never changes its clock while the answer
    /// arrives. Additive on disk: snapshots written before the stamp existed
    /// lack the key and parse as `None`, which renders without a time
    /// instead of inventing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
}

impl Message {
    pub fn user(markdown: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            markdown: markdown.into(),
            pending: false,
            model: None,
            created_at: None,
        }
    }

    pub fn assistant(markdown: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            markdown: markdown.into(),
            pending: false,
            model: None,
            created_at: None,
        }
    }

    /// Assistant reply that knows which model produced it.
    pub fn assistant_with_model(markdown: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            markdown: markdown.into(),
            pending: false,
            model: Some(model.into()),
            created_at: None,
        }
    }

    /// Placeholder for a future assistant reply.
    pub fn assistant_pending() -> Self {
        Self {
            role: Role::Assistant,
            markdown: String::new(),
            pending: true,
            model: None,
            created_at: None,
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
            created_at: None,
        }
    }

    /// Stamp the message with the unix-seconds instant it was created.
    ///
    /// Production callers pass [`crate::history::now_unix`]; taking the
    /// instant as a parameter keeps every construction site explicit (nothing
    /// is stamped behind the caller's back) and keeps the tests
    /// deterministic. Constructors leave the stamp empty: a row built without
    /// one — restored pre-timestamp history — simply renders no time.
    pub fn at(mut self, created_at: u64) -> Self {
        self.created_at = Some(created_at);
        self
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
    ///
    /// The creation stamp travels along too: a snapshot saved mid-request
    /// must keep the moment the bubble appeared, not re-date it at
    /// normalization time.
    pub fn normalized_for_storage(&self) -> Self {
        if self.pending {
            let mut stored = Message::assistant("");
            stored.created_at = self.created_at;
            stored
        } else {
            self.clone()
        }
    }

    /// Display label for the role header: `Some("glm-5.2")` renders as
    /// `● Chibi (glm-5.2)`; `None` keeps the plain `● Chibi`.
    pub fn model_label(&self) -> Option<&str> {
        self.model.as_deref().filter(|m| !m.trim().is_empty())
    }

    /// Marker predicate for the visible "⏳ queued (#n)" placeholder rows
    /// (prompts waiting in the chat's FIFO queue). The renderer paints them
    /// as an empty line, and terminal-event resolution must never touch
    /// them — they are owned by the queue drain, not by a live request.
    pub fn is_queued_marker(&self) -> bool {
        self.pending && self.markdown.starts_with("\u{23f3} queued")
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

    // per-message creation stamp ---------------

    /// Rows built without a stamp carry none — that is the shape of restored
    /// pre-timestamp history, and the renderer must not invent a time for it.
    #[test]
    fn created_at_is_absent_by_default_and_set_by_the_builder() {
        for message in [
            Message::user("q"),
            Message::assistant("a"),
            Message::assistant_with_model("a", "glm-5.2"),
            Message::assistant_pending(),
            Message::assistant_queued(1),
        ] {
            assert_eq!(message.created_at, None);
        }
        assert_eq!(
            Message::user("q").at(1_700_000_000).created_at,
            Some(1_700_000_000)
        );
    }

    /// Additive on disk, both ways: a stamp-less row serializes without the
    /// key (pre-timestamp snapshots stay byte-compatible) and a snapshot
    /// written before the stamp existed still parses.
    #[test]
    fn created_at_is_skipped_when_absent_and_survives_a_roundtrip() {
        let bare = serde_json::to_string(&Message::assistant("a")).unwrap();
        assert!(
            !bare.contains("created_at"),
            "unstamped rows must not grow the key: {bare}"
        );
        let legacy = r#"{"role":"User","markdown":"q","pending":false}"#;
        let parsed: Message = serde_json::from_str(legacy).expect("legacy message parses");
        assert_eq!(parsed.created_at, None);

        let stamped = serde_json::to_string(&Message::user("q").at(1_700_000_000)).unwrap();
        assert!(
            stamped.contains(r#""created_at":1700000000"#),
            "stamped rows carry the instant: {stamped}"
        );
        let back: Message = serde_json::from_str(&stamped).expect("stamped message parses");
        assert_eq!(back.created_at, Some(1_700_000_000));
    }

    /// The stamp is part of the message's identity on disk: a snapshot saved
    /// mid-request keeps the instant the bubble appeared instead of
    /// re-dating it at normalization time.
    #[test]
    fn storage_normalization_keeps_the_creation_stamp() {
        let pending = Message::assistant_pending().at(1_700_000_000);
        let stored = pending.normalized_for_storage();
        assert_eq!(stored.created_at, Some(1_700_000_000));
        assert!(!stored.pending);

        let finished = Message::assistant("answer").at(1_699_999_000);
        assert_eq!(
            finished.normalized_for_storage().created_at,
            Some(1_699_999_000)
        );
    }

    /// A stamp-less pending row (built by hand, no production site does this)
    /// normalizes without inventing a time.
    #[test]
    fn storage_normalization_of_an_unstamped_pending_row_stays_unstamped() {
        assert_eq!(
            Message::assistant_pending()
                .normalized_for_storage()
                .created_at,
            None
        );
    }
}
