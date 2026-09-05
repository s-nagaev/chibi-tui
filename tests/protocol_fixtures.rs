//! Fixture-driven tests for the protocol v1 serde types.
//!
//! The fixtures under `tests/fixtures/ide_protocol/` are canonical copies of
//! `/Users/sergio/Develop/personal/chibi/tests/fixtures/ide_protocol/` — do
//! not edit them here; fix them upstream and re-copy.

use std::fs;
use std::path::Path;

use chibi_tui::{ClientMessage, ProtocolVersion, ServerMessage, StatusState};

const FIXTURES: &str = "tests/fixtures/ide_protocol";

fn read_lines(name: &str) -> Vec<String> {
    let path = Path::new(FIXTURES).join(name);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parse a JSONL line as a `serde_json::Value` (for semantic comparison).
fn value(line: &str) -> serde_json::Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("fixture line is not JSON: {e}"))
}

#[test]
fn valid_session_input_roundtrip() {
    for line in read_lines("valid_session_input.jsonl") {
        let msg: ClientMessage = serde_json::from_str(&line).unwrap();
        let back = serde_json::to_value(&msg).unwrap();
        // Semantic identity (key order is irrelevant in JSON objects).
        assert_eq!(back, value(&line), "round-trip mismatch for: {line}");
    }
}

#[test]
fn valid_session_output_roundtrip() {
    for line in read_lines("valid_session_output.jsonl") {
        let msg: ServerMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            value(&line),
            "round-trip mismatch for: {line}"
        );
    }
}

#[test]
fn valid_cancel_input_roundtrip() {
    for line in read_lines("valid_cancel_input.jsonl") {
        let msg: ClientMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            value(&line),
            "round-trip mismatch for: {line}"
        );
    }
}

#[test]
fn valid_cancel_output_roundtrip() {
    for line in read_lines("valid_cancel_output.jsonl") {
        let msg: ServerMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            value(&line),
            "round-trip mismatch for: {line}"
        );
    }
}

#[test]
fn valid_shutdown_input_roundtrip() {
    for line in read_lines("valid_shutdown_input.jsonl") {
        let msg: ClientMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            value(&line),
            "round-trip mismatch for: {line}"
        );
    }
}

#[test]
fn valid_shutdown_output_roundtrip() {
    for line in read_lines("valid_shutdown_output.jsonl") {
        let msg: ServerMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            value(&line),
            "round-trip mismatch for: {line}"
        );
    }
}

#[test]
fn session_fixture_decodes_into_expected_variants() {
    // Stronger than round-trip: assert the exact variants and key payloads.
    let input = read_lines("valid_session_input.jsonl");
    let output = read_lines("valid_session_output.jsonl");

    match serde_json::from_str::<ClientMessage>(&input[0]).unwrap() {
        ClientMessage::Initialize {
            protocol_version,
            client,
            ..
        } => {
            assert_eq!(protocol_version, ProtocolVersion);
            assert_eq!(client.unwrap().name, "chibi-vscode");
        }
        other => panic!("expected Initialize, got {other:?}"),
    }
    match serde_json::from_str::<ClientMessage>(&input[1]).unwrap() {
        ClientMessage::Request {
            request_id,
            thread_id,
            selection,
            cursor_position,
            language_id,
            ..
        } => {
            assert_eq!(request_id, "01HXY9K1ABCDEFGH");
            assert_eq!(thread_id, 3);
            assert_eq!(selection.unwrap().start_line, 10);
            assert_eq!(cursor_position.unwrap().character, 8);
            assert_eq!(language_id.as_deref(), Some("python"));
        }
        other => panic!("expected Request, got {other:?}"),
    }

    match serde_json::from_str::<ServerMessage>(&output[0]).unwrap() {
        ServerMessage::Ready {
            capabilities,
            server,
            ..
        } => {
            assert_eq!(server.unwrap().name, "chibi");
            assert!(!capabilities.unwrap().commands.is_empty());
        }
        other => panic!("expected Ready, got {other:?}"),
    }
    assert!(matches!(
        serde_json::from_str::<ServerMessage>(&output[1]).unwrap(),
        ServerMessage::Status {
            state: StatusState::Running,
            ..
        }
    ));
    match serde_json::from_str::<ServerMessage>(&output[2]).unwrap() {
        ServerMessage::Result { content, model, .. } => {
            assert!(!content.is_empty());
            assert_eq!(model.as_deref(), Some("gpt-example"));
        }
        other => panic!("expected Result, got {other:?}"),
    }
}

#[test]
fn invalid_cases_are_rejected_with_meaningful_errors() {
    for case_line in read_lines("invalid_cases.jsonl") {
        let case: serde_json::Value =
            serde_json::from_str(&case_line).expect("invalid_cases entry must be JSON");
        let name = case["name"].as_str().expect("case has a name").to_owned();

        // The test input may be a structured frame or a raw malformed string.
        let input_repr = if case["input"].is_null() {
            continue;
        } else {
            case["input"].to_string()
        };

        let client_attempt: Result<ClientMessage, _> = serde_json::from_str(&input_repr);
        let server_attempt: Result<ServerMessage, _> = serde_json::from_str(&input_repr);

        // Sanity: every case still yields a well-formed `error` frame on the
        // wire per the manifest's `expected_output`.
        let expected = case["expected_output"]
            .as_array()
            .expect("expected_output list");
        let terminal = expected.last().expect("at least one output frame");
        assert_eq!(terminal["type"], "error", "case `{name}`");

        match name.as_str() {
            // Well-formed at the type level: their rejections are session
            // state (`not_initialized` = handshake-first, `unknown_request` =
            // cancel targets an unknown id), owned by the future
            // backend_client task, not by serde types. Here we only assert
            // the manifest stays coherent.
            "request_before_ready" | "cancel_unknown_request" => {
                assert!(client_attempt.is_ok());
            }
            _ => {
                assert!(
                    client_attempt.is_err() && server_attempt.is_err(),
                    "case `{name}` was expected to be rejected but deserialized"
                );
                // Rejection must carry a meaningful message.
                let err_text = client_attempt
                    .as_ref()
                    .err()
                    .map(|e| e.to_string())
                    .or_else(|| server_attempt.as_ref().err().map(|e| e.to_string()))
                    .unwrap_or_default();
                assert!(!err_text.trim().is_empty(), "case `{name}`: empty error");
            }
        }

        match name.as_str() {
            "unsupported_protocol_version" => {
                let err = format!("{client_attempt:?}");
                assert!(
                    err.contains("unsupported protocol version"),
                    "error should mention the unsupported version: {err}"
                );
                assert_eq!(terminal["code"], "unsupported_protocol_version");
                assert_eq!(terminal["server_protocol_version"], 1);
            }
            "unknown_message_type" => {
                assert_eq!(terminal["code"], "unknown_message");
            }
            "missing_required_fields" | "request_missing_request_id" => {
                assert_eq!(terminal["code"], "malformed_request");
            }
            "request_before_ready" => {
                assert_eq!(terminal["code"], "not_initialized");
            }
            "cancel_unknown_request" => {
                assert_eq!(terminal["code"], "unknown_request");
            }
            "malformed_json" => {
                // Raw non-JSON string: nothing can deserialize it at all.
            }
            other => panic!("unhandled invalid case: {other}"),
        }
    }
}

#[test]
fn unknown_fields_are_ignored_forward_safety() {
    // Spec §3: unknown fields inside known message types MUST be ignored.
    let msg: ClientMessage = serde_json::from_str(
        r#"{"type": "shutdown", "future_field": {"a": 1}, "another": [true, null]}"#,
    )
    .unwrap();
    assert_eq!(msg, ClientMessage::Shutdown {});

    let msg: ServerMessage = serde_json::from_str(
        r#"{"type": "status", "request_id": "R1", "state": "queued", "progress": 0.42}"#,
    )
    .unwrap();
    assert_eq!(
        msg,
        ServerMessage::Status {
            request_id: "R1".into(),
            state: StatusState::Queued
        }
    );
}

#[test]
fn optional_context_fields_can_be_omitted_or_null() {
    // Both spellings mean "no context".
    let with_nulls: ClientMessage = serde_json::from_str(
        r#"{"type": "request", "request_id": "A", "thread_id": 1, "prompt": "p",
            "workspace_root": "/w", "active_file": null, "selection": null,
            "cursor_position": null, "language_id": null}"#,
    )
    .unwrap();
    let omitted: ClientMessage = serde_json::from_str(
        r#"{"type": "request", "request_id": "A", "thread_id": 1, "prompt": "p",
            "workspace_root": "/w"}"#,
    )
    .unwrap();
    assert_eq!(with_nulls, omitted);

    // Serialization keeps the editor-context contract: fields present as
    // explicit `null` when empty (spec §4.2 "sent as null").
    let ser = serde_json::to_value(&omitted).unwrap();
    assert_eq!(ser.get("active_file"), Some(&serde_json::Value::Null));
    assert_eq!(ser.get("selection"), Some(&serde_json::Value::Null));
    assert_eq!(ser.get("cursor_position"), Some(&serde_json::Value::Null));
    assert_eq!(ser.get("language_id"), Some(&serde_json::Value::Null));
}

#[test]
fn status_state_serializes_to_lowercase_tags() {
    assert_eq!(serde_json::to_value(StatusState::Queued).unwrap(), "queued");
    assert_eq!(
        serde_json::to_value(StatusState::Running).unwrap(),
        "running"
    );
    assert_eq!(StatusState::Queued.to_string(), "queued");
}

// ---- feat_agent_model_label: optional model/provider on result frames -----

/// Backward compatibility at the type level: a `result` frame WITHOUT the
/// optional `model`/`provider` keys (old backend, fieldless variant) parses
/// leniently into `None`, and re-serializes without inventing the keys —
/// so the TUI keeps the plain `● Chibi` header and the wire shape stays
/// forward/backward compatible.
#[test]
fn result_frame_without_model_fields_roundtrips_without_them() {
    let fieldless = r#"{"type": "result", "request_id": "01NOMODEL", "content": "answer"}"#;
    let msg: ServerMessage = serde_json::from_str(fieldless).expect("fieldless result parses");
    match &msg {
        ServerMessage::Result {
            model, provider, ..
        } => {
            assert_eq!(model, &None);
            assert_eq!(provider, &None);
        }
        other => panic!("expected Result, got {other:?}"),
    }
    let back = serde_json::to_value(&msg).unwrap();
    assert!(back.get("model").is_none(), "no model key invented: {back}");
    assert!(
        back.get("provider").is_none(),
        "no provider key invented: {back}"
    );
}
