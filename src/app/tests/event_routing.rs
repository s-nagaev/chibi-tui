use super::support::*;
use crate::app::*;

#[test]
fn stray_events_after_completion_are_ignored() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "one shot");
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "answer".into(),
        thread_id: submitted.thread_id.clone(),
        model: None,
    });
    // Late duplicate / stale event for an already-finished request:
    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&submitted.request_id),
        message: "late failure".into(),
        thread_id: Some(submitted.thread_id.clone()),
    });
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    let msgs = &app.chats[0].messages;
    assert_eq!(msgs[1].markdown, "answer", "result not overwritten");
}

#[test]
fn events_for_unknown_threads_are_ignored() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "mine");
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: 12345,
        markdown: "not mine".into(),
        thread_id: "00000000-0000-0000-0000-00000000dead".into(),
        model: None,
    });
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));
    assert_eq!(app.chats[0].messages.len(), 2);
}

/// per_thread_async tolerance: a terminal event addressed to a REMOVED
/// chat's thread id must be dropped silently — no panic, no corruption
/// of surviving chats (their in-flight requests keep routing normally).
#[test]
fn terminal_event_for_removed_thread_is_dropped_silently() {
    let mut app = app_with_chats(2);
    let bg = submit_text(&mut app, "background"); // chat 0 busy
    app.select_next(); // chat 1 (idle) active
    app.begin_delete_confirm();
    let removed_id = app.confirm_delete().expect("chat 1 removed");
    assert_ne!(removed_id, bg.thread_id, "busy chat still present");
    assert_eq!(app.chats.len(), 1);

    // Stale event for the REMOVED thread id: silently dropped.
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: 987,
        markdown: "ghost".into(),
        thread_id: removed_id.clone(),
        model: None,
    });
    assert_eq!(app.chats.len(), 1);
    assert!(
        matches!(app.chats[0].lifecycle, ChatLifecycle::Awaiting { .. }),
        "surviving chat's request untouched by the ghost event"
    );

    // A late event for the still-present busy chat still routes normally.
    let bg_req = bg.request_id.clone();
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&bg_req),
        markdown: "**done**".into(),
        thread_id: bg.thread_id.clone(),
        model: None,
    });
    assert_eq!(app.chats[0].messages[1].markdown, "**done**");
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
}

// popup state logic ----------------------
