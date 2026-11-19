use super::support::*;
use crate::app::*;

fn background_message(app: &mut App, thread_id: &str, markdown: &str) {
    app.apply_backend_event(BackendEvent::BackgroundMessage {
        wire_thread_id: crate::live::wire_thread_id(thread_id),
        markdown: markdown.into(),
        model: None,
        thoughts: None,
    });
}

// ---- continuation ordering --------------------------------------------

/// A continuation that completes while a stream is still pending must never
/// render above the stream answer: it is held in memory and spliced directly
/// below the resolved row (task DoD #1).
#[test]
fn continuation_during_stream_lands_below_the_stream_answer() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "one");
    let thread_id = submitted.thread_id.clone();

    // Stream pending: user bubble + live placeholder, nothing else.
    assert_eq!(app.chats[0].messages.len(), 2);

    background_message(&mut app, &thread_id, "continuation");
    // Held, NOT appended to the transcript: a straight append would park the
    // content below the live placeholder row, so the later answer would fill
    // its row above it.
    assert_eq!(app.chats[0].messages.len(), 2);
    assert_eq!(app.chats[0].pending_continuations.len(), 1);

    finish_chat(&mut app, 0);
    let msgs = &app.chats[0].messages;
    assert_eq!(msgs.len(), 3, "held continuation spliced below the answer");
    assert_eq!(msgs[0].markdown, "one");
    assert_eq!(msgs[1].markdown, "**done**");
    assert!(!msgs[1].pending);
    assert_eq!(msgs[2].markdown, "continuation");
    assert!(!msgs[2].pending, "held content resolves as a real answer");
    assert!(
        app.chats[0].pending_continuations.is_empty(),
        "buffer drained"
    );
}

/// The reported race end-to-end: a queued prompt below the live placeholder.
/// Without the fix the continuation appended at transcript end — BELOW the
/// queued marker — and the queued prompt's later answer filled its row ABOVE
/// the earlier continuation content. With the fix the continuation splices
/// right below the stream answer it was held behind, and the queued prompt's
/// answer resolves below it.
#[test]
fn queued_prompt_answer_never_renders_above_an_earlier_continuation() {
    let mut app = app_with_chats(1);
    let first = submit_text(&mut app, "one");
    let thread_id = first.thread_id.clone();
    type_in(&mut app, "two");
    assert!(app.take_input().is_none(), "second prompt queued");

    // Continuation completes while "one" is still streaming.
    background_message(&mut app, &thread_id, "continuation");
    assert_eq!(app.chats[0].pending_continuations.len(), 1);

    // "one" resolves: the continuation lands DIRECTLY below its answer,
    // above the queued prompt — never below the marker.
    finish_chat(&mut app, 0);
    assert_eq!(app.chats[0].messages[2].markdown, "continuation");

    // The queued prompt drains and answers BELOW the continuation. The
    // event loop sends the dequeued bundle directly: the user bubble and
    // the swapped placeholder already exist, so no begin_request here.
    let thread = app.chats[0].id.clone();
    let next = app.dequeue_next_for(&thread).expect("queued prompt drains");
    assert_eq!(next.prompt, "two");
    finish_chat(&mut app, 0);
    let msgs = &app.chats[0].messages;
    let order: Vec<&str> = msgs.iter().map(|m| m.markdown.as_str()).collect();
    assert_eq!(
        order,
        vec!["one", "**done**", "continuation", "two", "**done**"],
        "the later answer must render below the earlier continuation"
    );
}

/// Multiple continuations completing during one stream keep their arrival
/// order below the resolved answer.
#[test]
fn held_continuations_keep_arrival_order() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "one");
    let thread_id = submitted.thread_id.clone();
    background_message(&mut app, &thread_id, "first");
    background_message(&mut app, &thread_id, "second");
    assert_eq!(app.chats[0].pending_continuations.len(), 2);

    finish_chat(&mut app, 0);
    let msgs = &app.chats[0].messages;
    assert_eq!(msgs[2].markdown, "first");
    assert_eq!(msgs[3].markdown, "second");
}

/// An absorbed (invisible) stream result still ends the stream: the held
/// continuations splice into the dropped placeholder's slot.
#[test]
fn invisible_result_still_flushes_held_continuations() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "one");
    let thread_id = submitted.thread_id.clone();
    background_message(&mut app, &thread_id, "continuation");

    let request_id = app.chats[0].lifecycle.request_id().unwrap().to_owned();
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&request_id),
        markdown: "  ".into(),
        thread_id,
        model: None,
    });
    let msgs = &app.chats[0].messages;
    assert_eq!(
        msgs.len(),
        2,
        "the ACK result leaves no bubble, the continuation takes its slot"
    );
    assert_eq!(msgs[1].markdown, "continuation");
    assert!(!msgs[1].pending);
}

// ---- queued-marker swap hardening --------------------------------------

/// A QueueDrain that races a still-pending stream must not pop the queue,
/// displace the queued marker into a second live placeholder or flip the
/// lifecycle: `dequeue_next_for` refuses to advance over a busy chat (task
/// DoD #2). Without this guard the marker for "two" was swapped into a live
/// placeholder and a second request started while "one" was still streaming.
#[test]
fn queue_drain_cannot_displace_a_marker_over_a_live_stream() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "one");
    let thread_id = submitted.thread_id.clone();
    type_in(&mut app, "two");
    assert!(app.take_input().is_none());
    assert_eq!(app.chats[0].messages.len(), 4, "user+pending, user+marker");

    // Drain racing the live stream: refused, nothing moves.
    assert!(
        app.dequeue_next_for(&thread_id).is_none(),
        "a busy chat must not drain"
    );
    assert_eq!(app.chats[0].queue.len(), 1, "prompt stays queued");
    assert_eq!(app.chats[0].messages.len(), 4, "no second placeholder");
    let marker = &app.chats[0].messages[3];
    assert!(marker.is_queued_marker(), "marker not displaced");
    assert_eq!(marker.markdown, "\u{23f3} queued (#1)");
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));

    // After the stream resolves, the very same drain path works.
    finish_chat(&mut app, 0);
    let next = app.dequeue_next_for(&thread_id).expect("drains when idle");
    assert_eq!(next.prompt, "two");
    let swapped = &app.chats[0].messages[3];
    assert!(!swapped.is_queued_marker(), "marker swapped in place");
    assert!(swapped.pending);
    assert_eq!(
        swapped.request_id.as_deref(),
        Some(next.request_id.as_str()),
        "the swapped row is bound to the new request id"
    );
}

/// A duplicate drain AFTER a prompt already left the queue (the chat is
/// `Awaiting` the next request) is refused the same way: the queue cannot
/// advance twice on one drain, and the remaining marker stays put.
#[test]
fn duplicate_drain_between_requests_never_double_pops() {
    let mut app = app_with_chats(1);
    let first = submit_text(&mut app, "one");
    let thread_id = first.thread_id.clone();
    for text in ["two", "three"] {
        type_in(&mut app, text);
        assert!(app.take_input().is_none());
    }
    assert_eq!(app.chats[0].queue.len(), 2);

    finish_chat(&mut app, 0);
    let next = app
        .dequeue_next_for(&thread_id)
        .expect("first drain pops two");
    assert_eq!(next.prompt, "two");
    app.begin_request(&next);

    // Duplicate/reordered drain while "two" is in flight: refused.
    assert!(app.dequeue_next_for(&thread_id).is_none());
    assert_eq!(app.chats[0].queue.len(), 1, "three stays queued");
    let marker = &app.chats[0].messages[5];
    assert!(marker.is_queued_marker(), "three's marker untouched");
    assert_eq!(marker.markdown, "\u{23f3} queued (#2)");
}

/// A `/stop` reset wipes the held-continuation buffer together with the
/// transcript: held content has no row to splice below any more.
#[test]
fn reset_clears_held_continuations() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "one");
    let thread_id = submitted.thread_id.clone();
    background_message(&mut app, &thread_id, "continuation");
    assert_eq!(app.chats[0].pending_continuations.len(), 1);

    app.chats[0].messages.clear();
    app.chats[0].queue.clear();
    app.chats[0].pending_continuations.clear();
    app.chats[0].lifecycle = ChatLifecycle::Idle;
    assert!(app.chats[0].pending_continuations.is_empty());
}
