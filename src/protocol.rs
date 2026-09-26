//! Serde types for Chibi IDE protocol v1 (`chibi stdio --tui`, JSONL frames).
//!
//! One JSON object per line; every message carries a string `type` tag
//! (serde internally-tagged enums, matching the canonical fixtures).
//!
//! Client → server: [`ClientMessage`] — `initialize` / `request` / `cancel` /
//! `shutdown`. Server → client: [`ServerMessage`] — `ready` / `status` /
//! `agent_event` / `result` / `error`.
//!
//! Protocol-version enforcement is part of the type contract:
//! [`ProtocolVersion`] only deserializes from the literal `1`, so a handshake
//! with any other version fails parsing (the backend answers
//! `unsupported_protocol_version`; that session logic lives in the client,
//! not here). Unknown fields inside known message types are ignored
//! (forward-compatible additions). Session-state rules (handshake-first,
//! cancel-targets-in-flight) are client concerns and intentionally absent.
//!
//! Spec: `.project/ide_integration/architecture_specification.md` §4.
//! Fixtures: `tests/fixtures/ide_protocol/`.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

/// Protocol version negotiated during the handshake.
///
/// Deliberately a single-variant type: deserialization rejects anything but
/// `1` with a precise error message, which is how an `initialize` frame with
/// e.g. `protocol_version: 99` is turned away at the type level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolVersion;

impl ProtocolVersion {
    /// The one supported protocol version.
    pub const CURRENT: Self = Self;
}

impl Serialize for ProtocolVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(1)
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = ProtocolVersion;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("protocol version 1")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v == 1 {
                    Ok(ProtocolVersion)
                } else {
                    Err(E::custom(format_args!(
                        "unsupported protocol version: {v} (supported: 1)"
                    )))
                }
            }
            // JSON numbers may arrive as f64 even when integral.
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                if v == 1.0 {
                    Ok(ProtocolVersion)
                } else {
                    Err(E::custom(format_args!(
                        "unsupported protocol version: {v} (supported: 1)"
                    )))
                }
            }
        }
        deserializer.deserialize_u64(Visitor)
    }
}

/// Client identification sent with `initialize`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// Server identification returned in `ready`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Slash-command set advertised by the backend in `ready`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub commands: Vec<String>,
}

/// Feature flags the client advertises during the handshake. The backend
/// gates each capability on an explicit `true`; absent fields mean "off".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    pub thoughts: bool,
    /// Opts in to mid-turn `agent_event` frames (subagent progress).
    pub subagents: bool,
    /// Opts in to out-of-band `message` frames: continuation answers the
    /// backend produces after a background tool result arrives (the parent
    /// request's lifecycle is already over, so there is no request id).
    pub background_messages: bool,
    /// Opts in to `cwd_update` frames: the backend reports the effective
    /// working directory of the agent state per thread (including runtime
    /// changes made by the `set_working_dir` tool). Old backends ignore the
    /// unknown flag, so the field is safe to always send.
    pub cwd_updates: bool,
}

/// Kind of a mid-turn `agent_event` frame: a subagent spawn began or ended
/// within the request's turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    Started,
    Finished,
}

impl AgentEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Finished => "finished",
        }
    }
}

/// Token accounting for the turn that produced a `result` frame.
/// `context_window` is absent/null when the backend cannot know it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub context_window: Option<u64>,
}

/// Zero-based half-open line range of the editor selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub start_line: u64,
    pub end_line: u64,
    pub text: String,
}

/// Zero-based cursor position inside the active file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorPosition {
    pub line: u64,
    pub character: u64,
}

/// Messages the IDE writes to the backend's stdin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Handshake; must be the first client message.
    Initialize {
        protocol_version: ProtocolVersion,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<ClientInfo>,
        /// Feature flags ({"thoughts": true, "subagents": true}). Old
        /// backends tolerate unknown handshake fields; a missing field
        /// parses as `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<ClientCapabilities>,
    },
    /// Start LLM work for a prompt with editor context.
    Request {
        request_id: String,
        thread_id: i64,
        prompt: String,
        workspace_root: String,
        /// Editor context is part of every request: absent on parse
        /// (lenient), serialized as explicit `null` when empty, per spec §4.2.
        #[serde(default)]
        active_file: Option<String>,
        #[serde(default)]
        selection: Option<Selection>,
        #[serde(default)]
        cursor_position: Option<CursorPosition>,
        #[serde(default)]
        language_id: Option<String>,
    },
    /// Lightweight catch-up: ask the backend to re-emit the effective
    /// working directory of a thread as a `cwd_update` frame (opt-in via
    /// `capabilities.cwd_updates`; the backend answers `unknown_message`
    /// otherwise, which the reader tolerates). Lets a client resync without
    /// the backend tracking re-emission state.
    GetCwd { thread_id: i64 },
    /// Cancel one in-flight request, targeted by id.
    Cancel { request_id: String },
    /// Graceful termination.
    Shutdown {},
}

/// Coarse request lifecycle state (`status` messages carry no content).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    Queued,
    Running,
}

impl StatusState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
        }
    }
}

impl fmt::Display for StatusState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Messages the backend writes to stdout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Handshake accepted; the negotiated version is echoed back.
    Ready {
        protocol_version: ProtocolVersion,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<ServerInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<Capabilities>,
    },
    /// Coarse lifecycle update for a request (`queued`/`running` only).
    Status {
        request_id: String,
        state: StatusState,
    },
    /// Mid-turn subagent progress for a request (opt-in via
    /// `capabilities.subagents` at handshake). NON-terminal: it must never
    /// end the request lifecycle — it flows through the reader like any
    /// server message and only updates frontend state. `name` is optional
    /// and currently unused by consumers; unknown extra fields are ignored.
    AgentEvent {
        request_id: String,
        event: AgentEventKind,
        #[serde(default)]
        active: u64,
        #[serde(default)]
        total: u64,
        #[serde(default)]
        name: Option<String>,
    },
    /// Final successful answer; terminal for its `request_id`.
    Result {
        request_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Token accounting for the turn; missing → `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Raw LLM reasoning for the turn (≤64KB backend-capped);
        /// missing → `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thoughts: Option<String>,
    },
    /// Out-of-band continuation answer for a background tool task (opt-in via
    /// `capabilities.background_messages` at handshake). It carries no request
    /// id: the parent request that spawned the background work has already
    /// ended with its own `result` frame, so this frame routes by `thread_id`
    /// alone and must never touch any request lifecycle. `model`/`provider`
    /// label the reply when the continuation model is known; `thoughts`
    /// carries the continuation turn's reasoning when the backend captured
    /// it (both optional, absent → `None`).
    Message {
        thread_id: i64,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thoughts: Option<String>,
    },
    /// Effective working directory of the agent state for a thread (opt-in
    /// via `capabilities.cwd_updates` at handshake). NON-terminal,
    /// thread-scoped, pure frontend-state update: like `status`/`agent_event`
    /// it must never affect a request lifecycle. `cwd` is the value the
    /// backend's `get_effective_working_dir` chain resolves to (thread
    /// override → user default → application default, expanduser-normalized).
    CwdUpdate { thread_id: i64, cwd: String },
    /// Failure for a request or a global failure (`request_id: null`).
    Error {
        /// `None` serializes as explicit `request_id: null` (global error),
        /// matching the fixtures; `default` keeps parsing lenient.
        #[serde(default)]
        request_id: Option<String>,
        code: ErrorCode,
        /// Present only on `code = unsupported_protocol_version`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_protocol_version: Option<u64>,
        message: String,
    },
}

/// Machine-readable error codes defined by protocol v1, plus the
/// frontend-facing codes the real backend emits for slash-command failures
/// (`invalid_request`, `backend_error`, `rate_limited`). Those three are not
/// in the frozen v1 set, but the backend already sends them on the wire, so
/// they must parse: an unparseable frame is dropped by the reader and the
/// request would never reach a terminal event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnsupportedProtocolVersion,
    NotInitialized,
    MalformedRequest,
    UnknownMessage,
    UnknownRequest,
    Cancelled,
    RequestFailed,
    /// Backend command or validation failure (frontend-facing form of the
    /// internal `invalid_argument`).
    InvalidRequest,
    /// Provider-side failure surfaced by the shared exception handler.
    BackendError,
    /// Provider rate limiting (the frame may carry a retry hint).
    RateLimited,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the frontend-facing codes the real backend emits
    /// for slash-command failures must parse, or the reader drops the frame
    /// and the request never terminates.
    #[test]
    fn frontend_facing_error_codes_parse() {
        let frame =
            r#"{"type":"error","request_id":"r1","code":"invalid_request","message":"bad args"}"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("invalid_request parses") {
            ServerMessage::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::InvalidRequest);
                assert_eq!(message, "bad args");
            }
            other => panic!("expected Error, got {other:?}"),
        }

        for (wire, expected) in [
            ("backend_error", ErrorCode::BackendError),
            ("rate_limited", ErrorCode::RateLimited),
        ] {
            let frame =
                format!(r#"{{"type":"error","request_id":"r1","code":"{wire}","message":"m"}}"#);
            match serde_json::from_str::<ServerMessage>(&frame).expect("code parses") {
                ServerMessage::Error { code, .. } => assert_eq!(code, expected),
                other => panic!("expected Error, got {other:?}"),
            }
        }
    }

    /// The frozen v1 codes keep parsing exactly as before.
    #[test]
    fn protocol_v1_error_codes_still_parse() {
        let frame =
            r#"{"type":"error","request_id":"r1","code":"request_failed","message":"boom"}"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("request_failed parses") {
            ServerMessage::Error { code, .. } => assert_eq!(code, ErrorCode::RequestFailed),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Result frame: every field present parses with exact values.
    #[test]
    fn result_usage_thoughts_full_parse() {
        let frame = r#"{
            "type": "result",
            "request_id": "r1",
            "content": "answer",
            "usage": {"input_tokens": 120, "output_tokens": 45, "context_window": 200000},
            "thoughts": "step by step..."
        }"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("full result parses") {
            ServerMessage::Result {
                usage, thoughts, ..
            } => {
                assert_eq!(
                    usage,
                    Some(Usage {
                        input_tokens: 120,
                        output_tokens: 45,
                        context_window: Some(200000),
                    })
                );
                assert_eq!(thoughts.as_deref(), Some("step by step..."));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// Result frame: partial shapes. `context_window` may be null and
    /// `thoughts` may be present without `usage` (and vice versa).
    #[test]
    fn result_usage_thoughts_partial_parse() {
        let null_window = r#"{
            "type": "result",
            "request_id": "r1",
            "content": "a",
            "usage": {"input_tokens": 1, "output_tokens": 2, "context_window": null}
        }"#;
        match serde_json::from_str::<ServerMessage>(null_window).expect("null window parses") {
            ServerMessage::Result {
                usage, thoughts, ..
            } => {
                assert_eq!(
                    usage,
                    Some(Usage {
                        input_tokens: 1,
                        output_tokens: 2,
                        context_window: None,
                    })
                );
                assert_eq!(thoughts, None);
            }
            other => panic!("expected Result, got {other:?}"),
        }

        let thoughts_only = r#"{
            "type": "result",
            "request_id": "r1",
            "content": "a",
            "thoughts": "hm"
        }"#;
        match serde_json::from_str::<ServerMessage>(thoughts_only).expect("thoughts-only parses") {
            ServerMessage::Result {
                usage, thoughts, ..
            } => {
                assert_eq!(usage, None);
                assert_eq!(thoughts.as_deref(), Some("hm"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// Result frame: absent usage/thoughts keep parsing (old backend).
    #[test]
    fn result_usage_thoughts_absent_parse() {
        let frame =
            r#"{"type":"result","request_id":"r1","content":"a","model":"m","provider":"p"}"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("absent fields parse") {
            ServerMessage::Result {
                usage, thoughts, ..
            } => {
                assert_eq!(usage, None);
                assert_eq!(thoughts, None);
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// Result frame: unknown extra fields are ignored.
    #[test]
    fn result_extra_unknown_fields_ignored() {
        let frame = r#"{
            "type": "result",
            "request_id": "r1",
            "content": "a",
            "usage": {"input_tokens": 3, "output_tokens": 4, "context_window": 8},
            "thoughts": "t",
            "future_thing": {"nested": [1, 2, 3]}
        }"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("extra fields ignored") {
            ServerMessage::Result {
                usage, thoughts, ..
            } => {
                assert_eq!(
                    usage,
                    Some(Usage {
                        input_tokens: 3,
                        output_tokens: 4,
                        context_window: Some(8),
                    })
                );
                assert_eq!(thoughts.as_deref(), Some("t"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// Handshake: the client's initialize frame carries
    /// `capabilities: {"thoughts": true, "subagents": true,
    /// "background_messages": true, "cwd_updates": true}` on the wire,
    /// protocol_version 1.
    #[test]
    fn initialize_serializes_client_capabilities() {
        let msg = ClientMessage::Initialize {
            protocol_version: ProtocolVersion::CURRENT,
            client: Some(ClientInfo {
                name: "chibi-tui".into(),
                version: "0.1.0".into(),
            }),
            capabilities: Some(ClientCapabilities {
                thoughts: true,
                subagents: true,
                background_messages: true,
                cwd_updates: true,
            }),
        };
        let line = serde_json::to_string(&msg).expect("initialize serializes");
        let value: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["capabilities"]["thoughts"], true);
        assert_eq!(value["capabilities"]["subagents"], true);
        assert_eq!(value["capabilities"]["background_messages"], true);
        assert_eq!(value["capabilities"]["cwd_updates"], true);

        // Deserializing it back keeps the capabilities (round-trip sanity).
        match serde_json::from_str::<ClientMessage>(&line).expect("round-trips") {
            ClientMessage::Initialize { capabilities, .. } => {
                assert_eq!(
                    capabilities,
                    Some(ClientCapabilities {
                        thoughts: true,
                        subagents: true,
                        background_messages: true,
                        cwd_updates: true,
                    })
                );
            }
            other => panic!("expected Initialize, got {other:?}"),
        }
    }

    /// A multi-line prompt travels the JSONL wire with its newlines intact:
    /// serialized as escaped `\\n` inside the one physical frame line (a raw
    /// newline would break the line framing the backend reads stdin with)
    /// and parsed back verbatim. Pins the prompt-corruption class of bugs:
    /// a draft shown as two lines must never reach the backend glued into
    /// one word-merged line.
    #[test]
    fn request_prompt_newlines_serialize_escaped_and_round_trip() {
        let msg = ClientMessage::Request {
            request_id: "req-multi-line".into(),
            thread_id: 7,
            prompt: "line one\nline two".into(),
            workspace_root: "/tmp".into(),
            active_file: None,
            selection: None,
            cursor_position: None,
            language_id: None,
        };
        let line = serde_json::to_string(&msg).expect("request serializes");
        assert!(
            !line.contains('\n'),
            "the wire frame must stay ONE physical line: {line}"
        );
        assert!(
            line.contains("line one\\nline two"),
            "the newline must travel as an escaped sequence, never dropped: {line}"
        );
        match serde_json::from_str::<ClientMessage>(&line).expect("frame parses") {
            ClientMessage::Request { prompt, .. } => {
                assert_eq!(
                    prompt, "line one\nline two",
                    "the backend parses back the same multi-line prompt"
                );
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    /// The out-of-band continuation frame parses leniently: only    /// The out-of-band continuation frame parses leniently: only
    /// `thread_id` and `content` are required, optional fields default to
    /// `None`, and serialization round-trips with `message` as the type tag.
    #[test]
    fn background_message_frame_parses_and_round_trips() {
        let full = r#"{"type":"message","thread_id":7,"content":"done","model":"m","provider":"p","thoughts":"t"}"#;
        match serde_json::from_str::<ServerMessage>(full).expect("full message frame parses") {
            ServerMessage::Message {
                thread_id,
                content,
                model,
                provider,
                thoughts,
            } => {
                assert_eq!(thread_id, 7);
                assert_eq!(content, "done");
                assert_eq!(model.as_deref(), Some("m"));
                assert_eq!(provider.as_deref(), Some("p"));
                assert_eq!(thoughts.as_deref(), Some("t"));
            }
            other => panic!("expected Message, got {other:?}"),
        }

        let bare = r#"{"type":"message","thread_id":9,"content":"hi"}"#;
        match serde_json::from_str::<ServerMessage>(bare).expect("bare message frame parses") {
            ServerMessage::Message {
                thread_id,
                content,
                model,
                provider,
                thoughts,
            } => {
                assert_eq!(thread_id, 9);
                assert_eq!(content, "hi");
                assert_eq!(model, None);
                assert_eq!(provider, None);
                assert_eq!(thoughts, None);
            }
            other => panic!("expected Message, got {other:?}"),
        }

        let msg = ServerMessage::Message {
            thread_id: 11,
            content: "answer".into(),
            model: None,
            provider: Some("openai".into()),
            thoughts: None,
        };
        let line = serde_json::to_string(&msg).expect("message serializes");
        let value: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(value["type"], "message");
        assert_eq!(value["thread_id"], 11);
        assert_eq!(value["provider"], "openai");
        assert!(
            value.get("model").is_none(),
            "None optionals stay off the wire"
        );
        assert!(
            value.get("thoughts").is_none(),
            "None optionals stay off the wire"
        );
    }

    /// An old fixture initialize without capabilities still parses.
    #[test]
    fn initialize_without_capabilities_parses() {
        let frame = r#"{"type":"initialize","protocol_version":1}"#;
        match serde_json::from_str::<ClientMessage>(frame).expect("legacy initialize parses") {
            ClientMessage::Initialize { capabilities, .. } => assert_eq!(capabilities, None),
            other => panic!("expected Initialize, got {other:?}"),
        }
    }

    /// agent_event frame: every field present parses with exact values.
    #[test]
    fn agent_event_full_parse() {
        let frame = r#"{
            "type": "agent_event",
            "request_id": "r1",
            "event": "started",
            "active": 2,
            "total": 5,
            "name": "researcher"
        }"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("full agent_event parses") {
            ServerMessage::AgentEvent {
                request_id,
                event,
                active,
                total,
                name,
            } => {
                assert_eq!(request_id, "r1");
                assert_eq!(event, AgentEventKind::Started);
                assert_eq!(active, 2);
                assert_eq!(total, 5);
                assert_eq!(name.as_deref(), Some("researcher"));
            }
            other => panic!("expected AgentEvent, got {other:?}"),
        }
    }

    /// agent_event frame: minimal shape without `name` (optional field).
    #[test]
    fn agent_event_minimal_parse() {
        let frame =
            r#"{"type":"agent_event","request_id":"r1","event":"finished","active":0,"total":3}"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("minimal agent_event parses") {
            ServerMessage::AgentEvent {
                request_id,
                event,
                active,
                total,
                name,
            } => {
                assert_eq!(request_id, "r1");
                assert_eq!(event, AgentEventKind::Finished);
                assert_eq!(active, 0);
                assert_eq!(total, 3);
                assert_eq!(name, None);
            }
            other => panic!("expected AgentEvent, got {other:?}"),
        }
    }

    /// agent_event frame: missing optional/optional-ish fields default
    /// tolerantly (`active`/`total` to 0, `name` to None).
    #[test]
    fn agent_event_defaults_tolerant_parse() {
        let frame = r#"{"type":"agent_event","request_id":"r1","event":"started"}"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("tolerant agent_event parses") {
            ServerMessage::AgentEvent {
                event,
                active,
                total,
                name,
                ..
            } => {
                assert_eq!(event, AgentEventKind::Started);
                assert_eq!(active, 0);
                assert_eq!(total, 0);
                assert_eq!(name, None);
            }
            other => panic!("expected AgentEvent, got {other:?}"),
        }
    }

    /// agent_event frame: unknown extra fields are ignored (forward
    /// compatibility), known fields keep their values.
    #[test]
    fn agent_event_extra_unknown_fields_ignored() {
        let frame = r#"{
            "type": "agent_event",
            "request_id": "r1",
            "event": "finished",
            "active": 1,
            "total": 2,
            "name": null,
            "future_thing": {"nested": [1, 2, 3]}
        }"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("extra fields ignored") {
            ServerMessage::AgentEvent {
                event,
                active,
                total,
                name,
                ..
            } => {
                assert_eq!(event, AgentEventKind::Finished);
                assert_eq!(active, 1);
                assert_eq!(total, 2);
                assert_eq!(name, None);
            }
            other => panic!("expected AgentEvent, got {other:?}"),
        }
    }

    /// agent_event frame: a bad `event` value fails the parse — strict enum
    /// policy, matching every other wire enum in this module. The reader's
    /// diag-warning path (task-1 hardening) catches the dropped line.
    #[test]
    fn agent_event_bad_event_value_fails_parse() {
        let frame = r#"{"type":"agent_event","request_id":"r1","event":"exploded"}"#;
        assert!(
            serde_json::from_str::<ServerMessage>(frame).is_err(),
            "unknown event kind must not parse"
        );
    }

    /// agent_event frames survive a serde round-trip with values intact.
    #[test]
    fn agent_event_round_trips() {
        let frame = r#"{"type":"agent_event","request_id":"r1","event":"started","active":2,"total":5,"name":"scout"}"#;
        let msg: ServerMessage = serde_json::from_str(frame).expect("parses");
        let line = serde_json::to_string(&msg).expect("serializes");
        match serde_json::from_str::<ServerMessage>(&line).expect("round-trips") {
            ServerMessage::AgentEvent {
                request_id,
                event,
                active,
                total,
                name,
            } => {
                assert_eq!(request_id, "r1");
                assert_eq!(event, AgentEventKind::Started);
                assert_eq!(active, 2);
                assert_eq!(total, 5);
                assert_eq!(name.as_deref(), Some("scout"));
            }
            other => panic!("expected AgentEvent, got {other:?}"),
        }
    }

    /// The `cwd_update` frame parses exactly (thread-scoped, non-terminal)
    /// and round-trips with `cwd_update` as the type tag.
    #[test]
    fn cwd_update_frame_parses_and_round_trips() {
        let frame = r#"{"type":"cwd_update","thread_id":42,"cwd":"/Users/dev/chibi"}"#;
        let msg: ServerMessage = serde_json::from_str(frame).expect("cwd_update parses");
        assert_eq!(
            msg,
            ServerMessage::CwdUpdate {
                thread_id: 42,
                cwd: "/Users/dev/chibi".into(),
            }
        );
        let line = serde_json::to_string(&msg).expect("serializes");
        assert_eq!(
            line,
            r#"{"type":"cwd_update","thread_id":42,"cwd":"/Users/dev/chibi"}"#
        );
    }

    /// The `get_cwd` catch-up frame serializes the exact wire shape and
    /// round-trips; unknown extra fields are tolerated on parse.
    #[test]
    fn get_cwd_frame_serializes_and_parses() {
        let msg = ClientMessage::GetCwd { thread_id: 42 };
        let line = serde_json::to_string(&msg).expect("get_cwd serializes");
        assert_eq!(line, r#"{"type":"get_cwd","thread_id":42}"#);
        let with_extra = r#"{"type":"get_cwd","thread_id":7,"future_field":1}"#;
        match serde_json::from_str::<ClientMessage>(with_extra).expect("parses") {
            ClientMessage::GetCwd { thread_id } => assert_eq!(thread_id, 7),
            other => panic!("expected GetCwd, got {other:?}"),
        }
    }
}
