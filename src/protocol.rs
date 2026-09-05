//! Serde types for Chibi IDE protocol v1 (`chibi ide --stdio`, JSONL frames).
//!
//! One JSON object per line; every message carries a string `type` tag
//! (serde internally-tagged enums, matching the canonical fixtures).
//!
//! Client → server: [`ClientMessage`] — `initialize` / `request` / `cancel` /
//! `shutdown`. Server → client: [`ServerMessage`] — `ready` / `status` /
//! `result` / `error`.
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
        /// Wave-2 feature flags ({"thoughts": true}). Old backends tolerate
        /// unknown handshake fields; a missing field parses as `None`.
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
    /// Final successful answer; terminal for its `request_id`.
    Result {
        request_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Token accounting for the turn (wave-2); missing → `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Raw LLM reasoning for the turn (wave-2, ≤64KB backend-capped);
        /// missing → `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thoughts: Option<String>,
    },
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

    /// feat_thread_clone: the frontend-facing codes the real backend emits
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

    /// Wave-2 result frame: every field present parses with exact values.
    #[test]
    fn result_usage_thoughts_full_parse() {
        let frame = r#"{
            "type": "result",
            "request_id": "r1",
            "content": "answer",
            "usage": {"input_tokens": 120, "output_tokens": 45, "context_window": 200000},
            "thoughts": "step by step..."
        }"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("full wave-2 result parses") {
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

    /// Wave-2 result frame: partial shapes. `context_window` may be null and
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

    /// Wave-2 result frame: absent usage/thoughts keep parsing (old backend).
    #[test]
    fn result_usage_thoughts_absent_parse() {
        let frame =
            r#"{"type":"result","request_id":"r1","content":"a","model":"m","provider":"p"}"#;
        match serde_json::from_str::<ServerMessage>(frame).expect("absent wave-2 fields parse") {
            ServerMessage::Result {
                usage, thoughts, ..
            } => {
                assert_eq!(usage, None);
                assert_eq!(thoughts, None);
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// Wave-2 result frame: unknown extra fields are ignored.
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

    /// Wave-2 handshake: the client's initialize frame carries
    /// `capabilities: {"thoughts": true}` on the wire, protocol_version 1.
    #[test]
    fn initialize_serializes_client_capabilities() {
        let msg = ClientMessage::Initialize {
            protocol_version: ProtocolVersion::CURRENT,
            client: Some(ClientInfo {
                name: "chibi-tui".into(),
                version: "0.1.0".into(),
            }),
            capabilities: Some(ClientCapabilities { thoughts: true }),
        };
        let line = serde_json::to_string(&msg).expect("initialize serializes");
        let value: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["capabilities"]["thoughts"], true);

        // Deserializing it back keeps the capability (round-trip sanity).
        match serde_json::from_str::<ClientMessage>(&line).expect("round-trips") {
            ClientMessage::Initialize { capabilities, .. } => {
                assert_eq!(capabilities, Some(ClientCapabilities { thoughts: true }));
            }
            other => panic!("expected Initialize, got {other:?}"),
        }
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
}
