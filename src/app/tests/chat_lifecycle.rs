use super::support::*;
use crate::app::*;

#[test]
fn lifecycle_idle_awaiting_running_result_idle() {
    let mut app = App::new(vec![Chat::new("lifecycle")]);
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);

    let submitted = submit_text(&mut app, "explain lifetimes");
    assert_eq!(submitted.prompt, "explain lifetimes");
    assert!(!submitted.thread_id.is_empty());
    assert!(!submitted.request_id.is_empty());
    assert_eq!(
        app.active_lifecycle(),
        &ChatLifecycle::Awaiting {
            request_id: submitted.request_id.clone()
        }
    );
    assert_eq!(app.chats[0].messages.len(), 2, "user + pending assistant");

    let req_event_id = event_id_of(&submitted.request_id);
    let thread_id = submitted.thread_id.clone();
    app.apply_backend_event(BackendEvent::Running {
        request_id: req_event_id,
        thread_id,
    });
    assert!(matches!(
        app.active_lifecycle(),
        ChatLifecycle::Running { .. }
    ));

    finish_chat(&mut app, 0);
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    let msgs = &app.chats[0].messages;
    assert_eq!(msgs.len(), 2);
    assert!(!msgs[1].pending);
    assert_eq!(msgs[1].markdown, "**done**");
}

#[test]
fn lifecycle_error_returns_to_idle_with_error_message() {
    let mut app = App::new(vec![Chat::new("errors")]);
    let submitted = submit_text(&mut app, "boom");
    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&submitted.request_id),
        message: "backend exploded".into(),
        thread_id: Some(app.chats[0].id.clone()),
    });
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    assert_eq!(
        app.chats[0].messages[1].markdown,
        "**Error:** backend exploded"
    );
}
