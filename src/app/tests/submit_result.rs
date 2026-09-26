use super::support::*;
use crate::app::*;

#[test]
fn empty_input_is_never_submitted_and_state_stays_idle() {
    let mut app = app_with_chats(1);
    app.input.input(crate::input::Input::default());
    assert!(app.take_input().is_none(), "whitespace-only is rejected");
    assert!(!app.is_busy());
    assert!(app.active_request_id().is_none());
    assert!(app.chats[0].messages.is_empty());
}

// ---- per-thread async: same chat enqueues, other chats stay free -----

#[test]
fn same_chat_second_submit_enqueues_fifo_bubble() {
    let mut app = app_with_chats(1);
    let first = submit_text(&mut app, "first");

    // Second Enter mid-flight enqueues instead of rejecting…
    type_in(&mut app, "second");
    assert!(app.take_input().is_none(), "busy chat returns None");
    // …the buffer was consumed and the queue holds the prompt…
    assert!(app.input.lines().iter().all(|l| l.is_empty()));
    assert_eq!(app.active_queue_len(), 1);
    // …with visible bubbles: user message + queued marker.
    let msgs = &app.chats[0].messages;
    assert_eq!(msgs.len(), 4, "user+pending, then user+queued marker");
    assert_eq!(msgs[2].markdown, "second");
    assert_eq!(msgs[2].role, Role::User);
    assert!(msgs[3].pending, "queued marker is a pending row");
    assert_eq!(msgs[3].markdown, "\u{23f3} queued (#1)");
    // Dequeue swaps the marker for the real pending placeholder.
    let thread = app.chats[0].id.clone();
    finish_chat(&mut app, 0);
    let _next = app.dequeue_next_for(&thread).expect("drain");
    // NOTE: no begin_request here — dequeue_next_for already started the
    // request (lifecycle + pending row); the loop only sends the bundle.
    assert_eq!(app.chats[0].messages[3].markdown, "");
    assert!(app.chats[0].messages[3].pending);
    // In-flight request moved to the drained prompt's id.
    assert_ne!(app.active_request_id(), Some(first.request_id.as_str()));
}

/// Terminal events must resolve ONLY the live pending placeholder —
/// never the "⏳ queued" markers belonging to prompts still waiting in
/// the FIFO queue (those are swapped by [`App::dequeue_next_for`]).
#[test]
fn resolve_live_placeholder_skips_queued_markers() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "first");
    type_in(&mut app, "second");
    assert!(app.take_input().is_none(), "busy chat enqueues");

    // Force a queued-marker row BEHIND the live pending placeholder to
    // prove resolution targets the placeholder, not the newest pending
    // row (the regression this helper was written for).
    let chat = &mut app.chats[0];
    chat.messages.push(Message::user("third"));
    chat.queue.push_back("third".into());
    chat.messages.push(Message::assistant_queued(2));
    assert_eq!(chat.messages.len(), 6);
    // [user, pending(live), user, marker#1, user, marker#2]
    assert!(chat.messages[3].markdown.starts_with("\u{23f3} queued"));

    // Deliver the terminal Result for the live request.
    finish_chat(&mut app, 0);

    let msgs = &app.chats[0].messages;
    assert!(!msgs[1].pending, "live placeholder resolved");
    assert_eq!(msgs[1].markdown, "**done**");
    assert!(
        msgs[3].pending && msgs[3].markdown == "\u{23f3} queued (#1)",
        "queued marker #1 must survive a terminal event"
    );
    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    assert!(
        msgs[5].pending && msgs[5].markdown == "\u{23f3} queued (#2)",
        "queued marker #2 must survive a terminal event"
    );
}

// per-message labelling --------------------

/// THE per-message contract: two consecutive replies produced by
/// DIFFERENT models each carry their own label — the second resolution
/// must never overwrite the first message's metadata (future
/// per-message model switching must not mislabel earlier answers).
#[test]
fn consecutive_results_label_their_own_messages() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "first prompt");
    finish_chat_with_model(&mut app, 0, Some("glm-5.2"));

    // Second round-trip in the same chat, different model this time.
    submit_text(&mut app, "second prompt");
    finish_chat_with_model(&mut app, 0, Some("kimi-k2.7"));

    let msgs = &app.chats[0].messages;
    assert_eq!(msgs.len(), 4);
    assert_eq!(
        msgs[1].model.as_deref(),
        Some("glm-5.2"),
        "first answer keeps its own label"
    );
    assert_eq!(
        msgs[3].model.as_deref(),
        Some("kimi-k2.7"),
        "second answer carries the new label"
    );
}

/// Fallback: a fieldless result (model = None) resolves to a message
/// with NO metadata — rendered later as the plain `● Chibi` header.
#[test]
fn fieldless_result_resolves_to_plain_unlabelled_message() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "old backend");
    finish_chat_with_model(&mut app, 0, None);

    let msgs = &app.chats[0].messages;
    assert!(!msgs[1].pending);
    assert_eq!(msgs[1].markdown, "**done**");
    assert_eq!(msgs[1].model_label(), None);
}

// ^S toggle ----------------------------------------
