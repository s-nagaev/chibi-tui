use super::support::*;
use crate::app::*;

pub(super) fn clone_capable_app(n: usize) -> App {
    let mut app = app_with_chats(n);
    app.set_backend_commands(vec![
        "/reset".to_owned(),
        "/new_thread_with_current_context".to_owned(),
    ]);
    app
}

#[test]
fn clone_without_capability_shows_informative_popup() {
    let mut app = app_with_chats(1);
    assert!(!app.supports_thread_clone(), "nothing advertised yet");
    app.begin_clone_thread();

    let popup = app.error_popup.as_ref().expect("informative popup");
    assert!(
        popup.message.contains("does not support thread cloning"),
        "popup must explain the missing capability: {popup:?}"
    );
    assert!(app.take_clone_submission().is_none(), "nothing staged");
    assert!(app.pending_clone.is_none(), "no orphan flight state");
    assert_eq!(app.chats.len(), 1, "sidebar unchanged");
}

#[test]
fn clone_gate_ignores_unrelated_commands() {
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/reset".to_owned(), "/model".to_owned()]);
    assert!(!app.supports_thread_clone());
    app.begin_clone_thread();

    assert!(app.error_popup.is_some(), "still the unsupported path");
    assert!(app.take_clone_submission().is_none());
}

#[test]
fn clone_stages_request_with_wire_shape_and_copied_name() {
    let mut app = clone_capable_app(1);
    app.chats[0].name = "My thread".to_owned();
    let source_id = app.active_thread_id().unwrap().to_owned();
    app.begin_clone_thread();

    let submitted = app.take_clone_submission().expect("staged");
    let pending = app.pending_clone.as_ref().expect("clone in flight");
    // Name defaulting: "<name> (copy)".
    assert_eq!(pending.chat.name, "My thread (copy)");
    // Fresh identity: a new UUID, never the source's.
    assert_ne!(pending.chat.id, source_id);
    assert_eq!(pending.chat.lifecycle, ChatLifecycle::Idle);
    assert!(pending.chat.queue.is_empty(), "queue is never copied");
    // The request rides ON the new chat (its UUID is the frame thread
    // id); the args carry the source wire id and the clone title.
    assert_eq!(submitted.thread_id, pending.chat.id);
    assert_eq!(
        submitted.prompt,
        format!(
            "{} {} My thread (copy)",
            App::CLONE_COMMAND,
            crate::live::wire_thread_id(&source_id)
        )
    );
    assert_eq!(submitted.request_id, pending.request_id);

    // Single-flight guard: a re-press while in flight must not mint a
    // second clone.
    app.begin_clone_thread();
    assert!(app.take_clone_submission().is_none());
    assert!(app.status_message.is_some(), "second press gets a toast");
}

#[test]
fn clone_refused_while_source_has_turn_in_flight() {
    let mut app = clone_capable_app(1);
    submit_text(&mut app, "long running");
    app.begin_clone_thread();

    assert!(
        app.error_popup.is_none(),
        "refusal follows the delete precedent: toast, not popup"
    );
    assert!(app.status_message.is_some(), "busy toast shown");
    assert!(app.take_clone_submission().is_none());
    assert!(app.pending_clone.is_none());
    assert_eq!(app.chats.len(), 1);
}

#[test]
fn clone_refused_while_source_has_queued_prompts() {
    let mut app = clone_capable_app(1);
    app.chats[0].queue.push_back("queued prompt".to_owned());
    app.begin_clone_thread();

    assert!(app.status_message.is_some(), "busy toast covers the queue");
    assert!(app.take_clone_submission().is_none());
}

#[test]
fn clone_noop_outside_normal_mode() {
    let mut app = clone_capable_app(1);
    app.mode = Mode::ConfirmDelete;
    app.begin_clone_thread();

    assert!(app.take_clone_submission().is_none());
    assert!(app.error_popup.is_none());
    assert!(app.status_message.is_none());
}

#[test]
fn clone_ack_inserts_on_top_selects_and_copies_mirror() {
    let mut app = clone_capable_app(3);
    app.select_next(); // active = 1 (chat-1)
    app.chats[1].messages.push(Message::user("question"));
    app.chats[1].messages.push(Message::assistant("**answer**"));
    app.begin_clone_thread();
    let submitted = app.take_clone_submission().expect("staged");
    assert_eq!(app.chats.len(), 3, "clone stays unlisted before the ack");

    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "Thread cloned: chat-1 (copy) (ID: 42). 2 messages copied.".to_owned(),
        thread_id: submitted.thread_id,
        model: None,
    });

    assert_eq!(app.chats.len(), 4);
    assert_eq!(
        app.chats[0].name, "chat-1 (copy)",
        "clone listed on top (newest-first sidebar)"
    );
    assert_eq!(app.chats[0].messages.len(), 2, "display mirror copied");
    assert_eq!(app.active, 0, "clone selected");
    assert_eq!(app.scroll, 0, "follow-bottom for the new chat");
    assert!(app.pending_clone.is_none(), "flight state cleared");
}

#[test]
fn clone_progress_frames_are_consumed_while_unlisted() {
    let mut app = clone_capable_app(1);
    app.begin_clone_thread();
    let submitted = app.take_clone_submission().expect("staged");

    app.apply_backend_event(BackendEvent::Queued {
        request_id: event_id_of(&submitted.request_id),
        thread_id: submitted.thread_id.clone(),
    });
    app.apply_backend_event(BackendEvent::Running {
        request_id: event_id_of(&submitted.request_id),
        thread_id: submitted.thread_id.clone(),
    });

    assert!(app.pending_clone.is_some(), "still awaiting the ack");
    assert!(
        app.chats.iter().all(|c| c.lifecycle == ChatLifecycle::Idle),
        "no listed chat tracks the clone request"
    );
}

#[test]
fn clone_ack_with_stray_ids_is_ignored() {
    let mut app = clone_capable_app(1);
    app.begin_clone_thread();
    let submitted = app.take_clone_submission().expect("staged");

    // Wrong request id, right thread: not ours.
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of("unrelated-request"),
        markdown: "ack".to_owned(),
        thread_id: submitted.thread_id.clone(),
        model: None,
    });
    // Right request id, wrong thread: not ours either.
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "ack".to_owned(),
        thread_id: "unrelated-thread".to_owned(),
        model: None,
    });

    assert_eq!(app.chats.len(), 1, "no listing without a matching ack");
    assert!(app.pending_clone.is_some(), "clone stays in flight");
    assert!(app.error_popup.is_none());
}

#[test]
fn clone_error_drops_clone_and_surfaces_backend_text() {
    let mut app = clone_capable_app(1);
    app.begin_clone_thread();
    let submitted = app.take_clone_submission().expect("staged");

    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&submitted.request_id),
        message: "Source thread 123 is busy. Wait for it to finish before cloning.".to_owned(),
        thread_id: Some(submitted.thread_id),
    });

    assert!(app.pending_clone.is_none(), "clone dropped, no orphan");
    assert_eq!(app.chats.len(), 1, "sidebar unchanged");
    let popup = app.error_popup.as_ref().expect("backend error surfaced");
    assert!(
        popup.message.contains("Wait for it to finish"),
        "the backend's own text is shown: {popup:?}"
    );
    assert!(app.take_clone_submission().is_none());
}

#[test]
fn clone_error_for_other_requests_never_touches_the_clone() {
    let mut app = clone_capable_app(1);
    app.begin_clone_thread();
    let _submitted = app.take_clone_submission().expect("staged");

    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of("unrelated-request"),
        message: "some other failure".to_owned(),
        thread_id: None,
    });

    assert!(app.pending_clone.is_some(), "clone untouched");
    assert!(app.error_popup.is_none(), "no popup for a foreign error");
}

#[test]
fn clone_ack_lists_on_top_even_if_source_deleted() {
    let mut app = clone_capable_app(2);
    app.begin_clone_thread();
    let submitted = app.take_clone_submission().expect("staged");
    // Source removed while the clone request is in flight.
    app.chats.remove(0);

    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "ack".to_owned(),
        thread_id: submitted.thread_id,
        model: None,
    });

    assert_eq!(app.chats.len(), 2);
    assert_eq!(
        app.chats[0].name, "chat-0 (copy)",
        "source gone: clone still lists on top (newest-first)"
    );
    assert_eq!(app.active, 0);
}

#[test]
fn clone_persists_via_existing_history_layer() {
    let mut app = clone_capable_app(1);
    app.chats[0].messages.push(Message::user("question"));
    app.begin_clone_thread();
    let submitted = app.take_clone_submission().expect("staged");
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "ack".to_owned(),
        thread_id: submitted.thread_id,
        model: None,
    });

    // Event-loop seam: the loop persists the selected chat after the
    // event; the clone must survive a restart through the same layer.
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-clone-restart-{}-{}",
        std::process::id(),
        crate::history::new_thread_id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    crate::history::save_chat_in(Some(&dir), &app.chats[app.active]).expect("save");

    let restored = crate::history::load_chats_from(Some(&dir));
    assert_eq!(restored.len(), 1, "exactly one snapshot file");
    assert_eq!(restored[0].name, "chat-0 (copy)");
    assert_eq!(restored[0].messages.len(), 1, "mirror survives restart");
    assert_eq!(restored[0].lifecycle, ChatLifecycle::Idle);
    assert!(restored[0].queue.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}
