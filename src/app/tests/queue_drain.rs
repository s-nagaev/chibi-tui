use super::support::*;
use crate::app::*;

#[test]
fn fifo_queue_drains_in_order_on_terminal_events() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "one");
    for text in ["two", "three", "four"] {
        type_in(&mut app, text);
        assert!(app.take_input().is_none());
    }
    assert_eq!(app.active_queue_len(), 3);

    // Terminal event #1 → next queued prompt becomes the new request.
    finish_chat(&mut app, 0);
    let thread = app.chats[0].id.clone();
    let next = app.dequeue_next_for(&thread).expect("queued #1");
    assert_eq!(next.prompt, "two", "strict FIFO order");
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));
    assert_eq!(app.active_queue_len(), 2);

    // Terminal event #2 → next queued prompt.
    finish_chat(&mut app, 0);
    let next = app.dequeue_next_for(&thread).expect("queued #2");
    assert_eq!(next.prompt, "three");
    assert_eq!(app.active_queue_len(), 1);

    // Terminal event #3 → last queued prompt.
    finish_chat(&mut app, 0);
    let next = app.dequeue_next_for(&thread).expect("queued #3");
    assert_eq!(next.prompt, "four");
    assert_eq!(app.active_queue_len(), 0);

    // Terminal event #4 → idle, nothing left to dequeue.
    finish_chat(&mut app, 0);
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    assert!(app.dequeue_next_for(&thread).is_none());

    // Queued markers were swapped in place for pending rows: display
    // order stays user → answer pairs in strict FIFO sequence.
    let msgs = &app.chats[0].messages;
    assert_eq!(msgs.len(), 8, "no duplicate bubbles from the drain path");
    assert_eq!(msgs[0].markdown, "one");
    assert_eq!(msgs[1].markdown, "**done**");
    assert!(!msgs[1].pending);
    assert_eq!(msgs[2].markdown, "two");
    assert_eq!(msgs[3].markdown, "**done**");
    assert!(!msgs[3].pending);
    assert_eq!(msgs[4].markdown, "three");
    assert_eq!(msgs[5].markdown, "**done**");
    assert!(!msgs[5].pending);
    assert_eq!(msgs[6].markdown, "four");
    assert_eq!(msgs[7].markdown, "**done**");
    assert!(!msgs[7].pending);
}

#[test]
fn other_chat_submit_works_while_first_chat_is_running() {
    let mut app = app_with_chats(2);
    let first = submit_text(&mut app, "long running in A");

    // Switch to chat B and submit while A runs — never blocked.
    app.select_chat(idx(&app, "chat-1"));
    let second = submit_text(&mut app, "parallel in B");
    assert_ne!(first.thread_id, second.thread_id);
    assert!(matches!(
        app.chats[idx(&app, "chat-0")].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));
    assert!(matches!(
        app.chats[idx(&app, "chat-1")].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));
    assert!(
        app.chats[idx(&app, "chat-0")].queue.is_empty(),
        "A's queue untouched"
    );
    assert!(
        app.chats[idx(&app, "chat-1")].queue.is_empty(),
        "B started immediately"
    );

    // B's answer lands first and only touches B.
    let second_req = second.request_id.clone();
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&second_req),
        markdown: "B done".into(),
        thread_id: second.thread_id.clone(),
        model: None,
    });
    assert_eq!(
        app.chats[idx(&app, "chat-0")].messages.len(),
        2,
        "A still pending"
    );
    assert_eq!(
        app.chats[idx(&app, "chat-1")].messages[1].markdown,
        "B done"
    );
    assert_eq!(
        app.chats[idx(&app, "chat-1")].lifecycle,
        ChatLifecycle::Idle
    );
    assert!(
        matches!(
            app.chats[idx(&app, "chat-0")].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ),
        "A keeps running across B's completion"
    );
}

#[test]
fn background_chat_drains_its_own_queue_when_not_selected() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "A1");
    type_in(&mut app, "A2");
    assert!(app.take_input().is_none());

    // Leave for chat B; A finishes in the background and its queue moves.
    app.select_next();
    finish_chat(&mut app, 0);
    let thread_a = app.chats[0].id.clone();
    let next = app
        .dequeue_next_for(&thread_a)
        .expect("A's queued prompt survives being backgrounded");
    assert_eq!(next.prompt, "A2");
    assert_eq!(next.thread_id, app.chats[0].id);
}

#[test]
fn spinner_reflects_active_chat_only() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "background work"); // chat 0 goes busy

    // Switch to idle chat 1: spinner must hide even though chat 0 runs.
    app.select_next();
    assert!(!app.is_busy(), "active chat idle ⇒ not busy");
    assert!(app.any_busy(), "some background chat still runs");

    // Back to chat 0: busy again.
    app.select_prev();
    assert!(app.is_busy());
}

// ---- cancel semantics -------------------------------------------------

/// Creation-stamp identity through the queued → pending → resolved chain:
/// a prompt keeps the instant it was queued with across both handoffs (the
/// dequeue swap and the result resolution), never re-dating itself at
/// either step.
#[test]
fn queued_prompt_keeps_its_stamp_through_dequeue_and_resolution() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "one");
    type_in(&mut app, "two");
    assert!(app.take_input().is_none(), "the second prompt is queued");

    // The queued user bubble and its position marker share one instant,
    // captured at enqueue time.
    let msgs = &app.chats[0].messages;
    assert_eq!(msgs.len(), 4, "user+pending, then user+queued-marker");
    let queued_at = msgs[2].created_at.expect("the queued prompt is stamped");
    assert_eq!(
        msgs[3].created_at,
        Some(queued_at),
        "the queued marker shares the queued prompt's instant"
    );

    // Terminal event #1 resolves "one" and dequeues "two": the marker is
    // swapped IN PLACE for the pending placeholder, so it must inherit
    // the marker's stamp, not a fresh one taken at dequeue time.
    finish_chat(&mut app, 0);
    let thread = app.chats[0].id.clone();
    let next = app.dequeue_next_for(&thread).expect("queued prompt drains");
    assert_eq!(next.prompt, "two");
    assert_eq!(
        app.chats[0].messages[3].created_at,
        Some(queued_at),
        "the pending placeholder keeps the queued marker's stamp after the swap"
    );

    // Terminal event #2 resolves "two": the answer's clock must still be
    // the moment the prompt was queued, not the moment it was answered.
    finish_chat(&mut app, 0);
    assert_eq!(
        app.chats[0].messages[3].created_at,
        Some(queued_at),
        "resolution preserves the stamp instead of re-dating the answer"
    );
    assert!(!app.chats[0].messages[3].pending);
}
