use super::support::*;
use crate::app::*;

#[test]
fn error_popup_shown_replaced_and_dismissed() {
    let mut app = app_with_chats(1);
    assert!(app.error_popup.is_none());

    app.show_error("spawn failed");
    assert_eq!(app.error_popup.as_ref().unwrap().message, "spawn failed");

    // A newer failure replaces the stale one.
    app.show_error("broken pipe");
    assert_eq!(app.error_popup.as_ref().unwrap().message, "broken pipe");

    app.dismiss_error();
    assert!(app.error_popup.is_none());
    // Dismissing twice is a no-op.
    app.dismiss_error();
    assert!(app.error_popup.is_none());
}

// clear input (Ctrl+L) ---------------------------------

#[test]
fn transport_failures_open_error_popup_and_flip_connection() {
    let mut app = App::new(vec![Chat::new("t")]);
    app.connection = Connection::Connected;
    let submitted = submit_text(&mut app, "go");

    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&submitted.request_id),
        message: "backend connection lost".into(),
        thread_id: Some(app.chats[0].id.clone()),
    });
    assert_eq!(app.connection, Connection::Disconnected);
    let popup = app.error_popup.as_ref().expect("popup shown");
    assert_eq!(popup.message, "backend connection lost");
    // The chat row still records the failure inline.
    assert!(app.chats[0].messages[1]
        .markdown
        .contains("connection lost"));
}

#[test]
fn per_request_errors_stay_inline_without_popup() {
    let mut app = App::new(vec![Chat::new("t")]);
    app.connection = Connection::Connected;
    let submitted = submit_text(&mut app, "go");

    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&submitted.request_id),
        message: "Request failed (InvalidArgument): prompt too long".into(),
        thread_id: Some(app.chats[0].id.clone()),
    });
    assert!(
        app.error_popup.is_none(),
        "per-request errors must not open the popup"
    );
    assert_eq!(app.connection, Connection::Connected);
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
}

#[test]
fn disconnected_event_flips_indicator_only() {
    let mut app = app_with_chats(1);
    app.connection = Connection::Connected;
    app.apply_backend_event(BackendEvent::Disconnected);
    assert_eq!(app.connection, Connection::Disconnected);
    assert!(app.error_popup.is_none(), "no popup without a request");
    assert!(!app.is_busy());
}

#[test]
fn is_transport_failure_covers_known_phrases() {
    for msg in [
        "I/O error talking to backend: Broken pipe (os error 32)",
        "backend connection lost",
        "handshake failed: unexpected frame",
        "failed to spawn `chibi`: program not found",
        "cancel failed: backend stdin already closed",
    ] {
        assert!(is_transport_failure(msg), "{msg} must be transport");
    }
    for msg in ["Request failed (InvalidArgument): bad prompt", "Cancelled"] {
        assert!(!is_transport_failure(msg), "{msg} is per-request");
    }
}

//------------------------------------------------------------------------
