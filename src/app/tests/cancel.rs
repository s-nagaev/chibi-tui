use super::support::*;
use crate::app::*;

#[test]
fn cancel_active_reports_inflight_request_only() {
    let mut app = app_with_chats(1);
    assert!(app.cancel_active().is_none(), "nothing in flight yet");

    let submitted = submit_text(&mut app, "to cancel");
    let (request_id, thread_id) = app.cancel_active().expect("cancel target present");
    assert_eq!(request_id, submitted.request_id);
    assert_eq!(thread_id, app.chats[0].id);
    // UI stays busy: only the backend's cancelled-error resolves it.
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));
}

#[test]
fn cancel_keeps_queue_and_drains_it_afterwards() {
    let mut app = app_with_chats(1);
    let inflight = submit_text(&mut app, "in flight");
    type_in(&mut app, "queued behind cancel");
    assert!(app.take_input().is_none());
    assert_eq!(app.active_queue_len(), 1);

    // Cancel targets ONLY the in-flight request id.
    let (cancel_id, _) = app.cancel_active().expect("cancel target");
    assert_eq!(cancel_id, inflight.request_id);

    // Simulate the cancelled-error terminal event for that request.
    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&inflight.request_id),
        message: "Cancelled".into(),
        thread_id: Some(app.chats[0].id.clone()),
    });
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    // Queue survived the cancel…
    assert_eq!(app.active_queue_len(), 1);
    // …and drains normally.
    let thread = app.chats[0].id.clone();
    let next = app.dequeue_next_for(&thread).expect("survivor");
    assert_eq!(next.prompt, "queued behind cancel");
    assert_eq!(
        app.chats[0].messages[1].markdown, "**Error:** Cancelled",
        "cancelled placeholder resolved by the backend error frame"
    );
}

#[test]
fn cancel_targets_active_chat_not_background_chats() {
    let mut app = app_with_chats(2);
    let bg = submit_text(&mut app, "background"); // chat 0 busy

    app.select_next();
    assert!(
        app.cancel_active().is_none(),
        "active chat B is idle: Ctrl+C means quit, never touches A"
    );
    // A's request remains intact and cancellable from its own view.
    app.select_prev();
    let (request_id, thread_id) = app.cancel_active().expect("A's cancel target");
    assert_eq!(request_id, bg.request_id);
    assert_eq!(thread_id, app.chats[0].id);
}

#[test]
fn resolve_cancel_locally_resolves_pending_and_returns_to_idle() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "long running");
    assert!(app.chats[0].messages[1].pending);
    assert!(app.is_busy());

    app.resolve_cancel_locally();

    assert!(!app.chats[0].messages[1].pending);
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    assert!(app.cancel_active().is_none());
}

// transport-failure escalation --------------------------
