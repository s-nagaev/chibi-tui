use super::support::*;
use crate::app::*;

// ---- id-keyed pending row lifecycle ---------------------------------------
//
// A pending placeholder row is bound to the request that created it
// (`Message::for_request`), so ANY terminal frame — result, error, late
// arrival after a lifecycle switch — resolves exactly that row, quietly,
// exactly once. No new user-facing error surface, no watchdog.

/// A terminal Result for R1 arriving AFTER the chat's lifecycle moved on to
/// a newer request resolves R1's own row, never R2's placeholder — the
/// owner report "an answer rendered above an earlier one" came from the
/// old newest-pending-row resolution crossing requests.
#[test]
fn result_after_lifecycle_switch_resolves_its_own_pending_row() {
    let mut app = app_with_chats(1);
    let r1 = submit_text(&mut app, "first");
    // The lifecycle moves on to a newer request with its own placeholder,
    // exactly like a queue drain would leave the chat after R1's row was
    // (in the failure mode this regression guards) left stranded.
    let r2_id = "message-lifecycle-r2".to_owned();
    app.chats[0].lifecycle = ChatLifecycle::Awaiting {
        request_id: r2_id.clone(),
    };
    app.chats[0]
        .messages
        .push(Message::assistant_pending().for_request(&r2_id));

    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&r1.request_id),
        markdown: "late answer".into(),
        thread_id: r1.thread_id.clone(),
        model: None,
    });

    let msgs = &app.chats[0].messages;
    assert_eq!(
        msgs[1].markdown, "late answer",
        "R1's own row is filled with R1's answer"
    );
    assert!(!msgs[1].pending, "R1's row resolves exactly once");
    let r2_row = msgs.last().unwrap();
    assert!(r2_row.pending, "R2's placeholder must stay untouched");
    assert_eq!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { request_id: r2_id },
        "a dead request's frame must not disturb the live lifecycle"
    );
}

/// An Error frame arriving while the chat is Idle (lifecycle already
/// released) resolves the pending row quietly: the error text lands in the
/// bubble, and nothing else moves — no modal popup, no connection flip, no
/// spinner resurrection.
#[test]
fn error_frame_resolves_pending_quietly_when_idle() {
    let mut app = app_with_chats(1);
    let r1 = submit_text(&mut app, "go");
    app.chats[0].lifecycle = ChatLifecycle::Idle;

    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&r1.request_id),
        message: "boom".into(),
        thread_id: Some(r1.thread_id.clone()),
    });

    let msgs = &app.chats[0].messages;
    assert!(
        msgs[1].markdown.contains("**Error:** boom"),
        "the pending row finalizes with the error text: {:#?}",
        msgs[1].markdown
    );
    assert!(!msgs[1].pending);
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
    // quiet: no user-facing error surface, connection state untouched
    assert!(app.error_popup.is_none(), "no modal popup");
    assert!(app.mode.is_normal(), "no mode change");
    assert_ne!(
        app.connection,
        Connection::Disconnected,
        "an untracked error frame must not flip the connection state"
    );
}

/// The lifecycle gate can never strand a pending row: a terminal frame
/// arriving while the chat tracks NOTHING resolves its row, and a duplicate
/// of the same frame resolves nothing again (exactly once).
#[test]
fn idle_gate_cannot_strand_a_pending_row() {
    let mut app = app_with_chats(1);
    let r1 = submit_text(&mut app, "stranded");
    app.chats[0].lifecycle = ChatLifecycle::Idle;

    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&r1.request_id),
        markdown: "finally".into(),
        thread_id: r1.thread_id.clone(),
        model: None,
    });
    let snapshot: Vec<(bool, String)> = app.chats[0]
        .messages
        .iter()
        .map(|m| (m.pending, m.markdown.clone()))
        .collect();
    assert!(!snapshot[1].0, "the stranded row resolved");
    assert_eq!(snapshot[1].1, "finally");

    // A duplicate terminal frame is a no-op: no second bubble, no rewrite.
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&r1.request_id),
        markdown: "finally".into(),
        thread_id: r1.thread_id.clone(),
        model: None,
    });
    let after: Vec<(bool, String)> = app.chats[0]
        .messages
        .iter()
        .map(|m| (m.pending, m.markdown.clone()))
        .collect();
    assert_eq!(after, snapshot, "duplicate frame resolves nothing again");
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
}

/// The reasoning payload of a terminal frame attaches to the message that
/// produced it — never to pending or queued rows (screenshot regression: a
/// subagent's grey block stood alone between two assistant messages).
#[test]
fn thoughts_attach_only_to_their_producing_message() {
    let mut app = app_with_chats(1);
    let thread_id = app.chats[0].id.clone();
    let r1 = submit_text(&mut app, "turn one");
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: Some("turn one reasoning".into()),
        request_id: event_id_of(&r1.request_id),
        markdown: "answer one".into(),
        thread_id: thread_id.clone(),
        model: None,
    });
    // Next turn starts (pending row), another prompt queues behind it —
    // the screenshot order: resolved message, pending row, queued marker.
    let r2 = submit_text(&mut app, "turn two");
    type_in(&mut app, "turn three");
    assert!(
        app.take_input().is_none(),
        "a busy chat queues the prompt instead of sending it"
    );

    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: Some("turn two reasoning".into()),
        request_id: event_id_of(&r2.request_id),
        markdown: "answer two".into(),
        thread_id: thread_id.clone(),
        model: None,
    });

    let msgs = &app.chats[0].messages;
    assert_eq!(
        msgs[1].thoughts.as_deref(),
        Some("turn one reasoning"),
        "the first answer keeps its own block"
    );
    assert_eq!(
        msgs[3].markdown, "answer two",
        "the pending row resolves with its own turn's answer"
    );
    assert_eq!(
        msgs[3].thoughts.as_deref(),
        Some("turn two reasoning"),
        "the block rides the message that produced it"
    );
    let queued_marker = msgs.last().unwrap();
    assert!(queued_marker.pending && queued_marker.is_queued_marker());
    assert!(
        queued_marker.thoughts.is_none(),
        "a queued row never carries a thoughts block"
    );
}
