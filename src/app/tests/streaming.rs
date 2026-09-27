use super::support::*;
use crate::app::*;
use crate::model::Message;

// ---- streaming delta application (plan D4/D5/D6) ------------------------

/// Numeric Delta frame for `index`'s chat and the given protocol request id.
fn delta_event(app: &App, index: usize, request_id: &str, text: &str) -> BackendEvent {
    BackendEvent::Delta {
        request_id: event_id_of(request_id),
        thread_id: app.chats[index].id.clone(),
        text: text.to_owned(),
    }
}

/// Inline Error frame for `index`'s chat and the given protocol request id.
fn error_event(app: &App, index: usize, request_id: &str, message: &str) -> BackendEvent {
    BackendEvent::Error {
        request_id: event_id_of(request_id),
        message: message.to_owned(),
        thread_id: Some(app.chats[index].id.clone()),
    }
}

/// Deltas append to the live pending placeholder, in order, in memory only.
#[test]
fn delta_appends_to_the_live_pending_row() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");

    app.apply_backend_event(delta_event(&app, 0, &submitted.request_id, "Hel"));
    app.apply_backend_event(delta_event(&app, 0, &submitted.request_id, "lo"));

    let msgs = &app.chats[0].messages;
    assert_eq!(msgs[1].markdown, "Hello", "chunks append in wire order");
    assert!(msgs[1].pending, "the row stays pending mid-stream");
    assert!(msgs[1].model.is_none(), "no model label mid-stream");
    assert!(
        app.chats[0].streaming,
        "accepted deltas arm the plain-text render"
    );
    assert_eq!(
        app.active_lifecycle().request_id(),
        Some(submitted.request_id.as_str()),
        "a delta never resolves the lifecycle"
    );
}

/// THE late-delta drop rule: after the terminal result the lifecycle is
/// Idle, so a delta racing the result through the shared mpsc is dropped
/// instead of corrupting the resolved row (it would never self-heal).
#[test]
fn delta_after_result_is_dropped() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    app.apply_backend_event(delta_event(&app, 0, &submitted.request_id, "par"));
    finish_chat(&mut app, 0);
    assert_eq!(app.chats[0].messages[1].markdown, "**done**");
    assert!(!app.chats[0].streaming, "terminal result ends streaming");

    // The late chunk must NOT append to the resolved row.
    app.apply_backend_event(delta_event(&app, 0, &submitted.request_id, "TIAL"));
    assert_eq!(
        app.chats[0].messages[1].markdown, "**done**",
        "delta-after-result must never touch the final row"
    );
    assert!(!app.chats[0].streaming);
}

/// A delta whose numeric request id does not match the tracked lifecycle
/// id belongs to another (unknown/finished) request: dropped.
#[test]
fn delta_for_another_request_is_dropped() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "hello");

    app.apply_backend_event(delta_event(&app, 0, "some-other-request", "junk"));

    assert_eq!(app.chats[0].messages[1].markdown, "");
    assert!(!app.chats[0].streaming);
}

/// A delta with no live pending row to attach to (placeholder already
/// gone) is dropped rather than resurrecting content.
#[test]
fn delta_with_no_live_row_is_dropped() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    // Simulate a hidden/plumbing exchange: lifecycle tracks the request
    // but no visible placeholder row exists.
    app.chats[0]
        .messages
        .retain(|m| !(m.pending && !m.is_queued_marker()));

    app.apply_backend_event(delta_event(&app, 0, &submitted.request_id, "orphan"));

    assert!(
        !app.chats[0]
            .messages
            .iter()
            .any(|m| m.markdown.contains("orphan")),
        "nothing was created"
    );
    assert!(!app.chats[0].streaming);
}

/// Deltas must never touch the "⏳ queued" markers of prompts still
/// waiting in the FIFO queue — only the live placeholder receives text.
#[test]
fn delta_never_touches_queued_markers() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "first");
    type_in(&mut app, "second");
    assert!(app.take_input().is_none(), "busy chat enqueues");
    let marker_index = app.chats[0].messages.len() - 1;

    app.apply_backend_event(delta_event(&app, 0, &submitted.request_id, "chunk"));

    assert_eq!(app.chats[0].messages[1].markdown, "chunk");
    assert_eq!(
        app.chats[0].messages[marker_index].markdown, "\u{23f3} queued (#1)",
        "the queued marker is untouched"
    );
}

/// The terminal result ends streaming: the plain-text render flag clears
/// and the authoritative full text replaces the accumulated partial.
#[test]
fn result_stops_streaming_and_restores_markdown() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    app.apply_backend_event(delta_event(&app, 0, &submitted.request_id, "par"));
    assert!(app.chats[0].streaming);

    finish_chat(&mut app, 0);

    let msgs = &app.chats[0].messages;
    assert!(!msgs[1].pending);
    assert_eq!(
        msgs[1].markdown, "**done**",
        "result overwrites the partial"
    );
    assert!(!app.chats[0].streaming, "full markdown render returns");
}

/// D5: a mid-stream failure keeps the partial text the user already saw,
/// with the error appended below it.
#[test]
fn error_keeps_streamed_partial_text() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    app.apply_backend_event(delta_event(
        &app,
        0,
        &submitted.request_id,
        "partial answer",
    ));

    app.apply_backend_event(error_event(&app, 0, &submitted.request_id, "boom"));

    let msgs = &app.chats[0].messages;
    assert!(!msgs[1].pending, "the row is resolved");
    assert_eq!(
        msgs[1].markdown, "partial answer\n\n**Error:** boom",
        "partial text survives with the error suffix"
    );
    assert!(!app.chats[0].streaming);
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
}

/// A failure with NO streamed partial degrades to the plain error bubble
/// (the pre-streaming shape is preserved).
#[test]
fn error_with_blank_partial_degrades_to_plain_error() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");

    app.apply_backend_event(error_event(&app, 0, &submitted.request_id, "boom"));

    assert_eq!(app.chats[0].messages[1].markdown, "**Error:** boom");
    assert!(!app.chats[0].messages[1].pending);
}

/// A queued marker is never mistaken for the live row by the D5 error
/// resolution either.
#[test]
fn error_resolution_skips_queued_markers() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "first");
    type_in(&mut app, "second");
    assert!(app.take_input().is_none(), "busy chat enqueues");

    app.apply_backend_event(error_event(&app, 0, &submitted.request_id, "boom"));

    let msgs = &app.chats[0].messages;
    assert_eq!(
        msgs[1].markdown, "**Error:** boom",
        "the live placeholder resolves with the error"
    );
    assert_eq!(msgs[3].markdown, "\u{23f3} queued (#1)", "marker survives");
    assert!(msgs[3].pending);
    assert!(!app.chats[0].streaming);
}

/// The streaming flag is session-only view state: a fresh chat starts
/// clean and a locally cancelled placeholder clears it.
#[test]
fn local_cancel_clears_streaming_state() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    app.apply_backend_event(delta_event(&app, 0, &submitted.request_id, "par"));
    assert!(app.chats[0].streaming);

    app.resolve_cancel_locally();

    assert!(!app.chats[0].streaming);
    assert_eq!(app.chats[0].messages[1].markdown, "_Cancelled._");
}

/// Placeholder row identity: the Message-level queued-marker predicate.
#[test]
fn queued_marker_predicate_matches_only_queued_rows() {
    assert!(Message::assistant_queued(1).is_queued_marker());
    assert!(Message::assistant_queued(2).is_queued_marker());
    assert!(!Message::assistant_pending().is_queued_marker());
    assert!(!Message::assistant("\u{23f3} queued (#1)").is_queued_marker());
    assert!(!Message::user("\u{23f3} queued (#1)").is_queued_marker());
}
