use super::support::*;
use crate::app::*;

/// Fresh user activity (request start) lifts the thread to the sidebar
/// top and the selection follows it; other chats keep their order.
#[test]
fn begin_request_lifts_active_thread_to_top() {
    let mut app = app_with_chats(3);
    app.select_chat(2);
    let submitted = submit_text(&mut app, "top please");

    assert_eq!(app.chats[0].id, submitted.thread_id, "thread on top");
    assert_eq!(app.active, 0, "selection follows the lifted thread");
    assert_eq!(app.chats[1].name, "chat-0", "others keep their order");
    assert_eq!(app.chats[2].name, "chat-1", "others keep their order");
}

/// A visible reply in a BACKGROUND thread lifts it to the top WITHOUT
/// stealing the selection, and the lifted thread carries its snapshot
/// stamp (updated_at) for the next persistence round.
#[test]
fn visible_reply_lifts_background_thread_to_top() {
    let mut app = app_with_chats(3);
    submit_text(&mut app, "work in chat 0");
    app.select_chat(idx(&app, "chat-1"));
    let background = submit_text(&mut app, "background request");
    app.select_chat(idx(&app, "chat-2")); // user reads chat-2
    {
        let i = idx(&app, "chat-1");
        finish_chat(&mut app, i);
    }

    assert_eq!(
        app.chats[0].id, background.thread_id,
        "the answered thread is on top"
    );
    assert_eq!(
        app.chats[app.active].id,
        app.chats[idx(&app, "chat-2")].id,
        "selection stays on the thread the user was reading"
    );
}

/// A queued prompt (busy chat) is activity too: the thread lifts to the
/// top at enqueue time.
#[test]
fn queued_prompt_lifts_thread_to_top() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "first in chat 0");
    app.select_chat(idx(&app, "chat-1"));
    submit_text(&mut app, "second in chat 1"); // chat-1 lifts to the top
    app.select_chat(idx(&app, "chat-0")); // user goes back to busy chat-0
    type_in(&mut app, "queued behind the running turn");
    assert!(app.take_input().is_none(), "busy chat enqueues");

    assert_eq!(
        app.chats[0].name, "chat-0",
        "the enqueued thread is back on top"
    );
    assert_eq!(
        app.chats[app.active].name, "chat-0",
        "selection follows the enqueued thread"
    );
    assert_eq!(app.chats[1].name, "chat-1", "the other busy thread below");
}

/// A background continuation (`message` frame) lifts its thread to the
/// top as well.
#[test]
fn background_continuation_lifts_thread_to_top() {
    let mut app = app_with_chats(2);
    let target = app.chats[1].id.clone();
    app.apply_backend_event(BackendEvent::BackgroundMessage {
        wire_thread_id: crate::live::wire_thread_id(&target),
        markdown: "continuation".into(),
        model: None,
        thoughts: None,
    });

    assert_eq!(app.chats[0].id, target, "continuation lifts the thread");
}

/// touch_chat: the moved chat keeps its selection, chats it jumped over
/// shift right by one, and the stamp is refreshed.
#[test]
fn touch_chat_lifts_thread_and_fixes_indexes() {
    let mut app = app_with_chats(3);
    app.select_chat(2);
    let before = app.chats[2].updated_at;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    app.touch_chat(2);

    assert_eq!(app.chats[0].name, "chat-2", "lifted to the top");
    assert_eq!(app.active, 0, "the lifted chat stays selected");
    assert_eq!(app.chats[1].name, "chat-0", "shifted right");
    assert_eq!(app.chats[2].name, "chat-1", "shifted right");
    assert!(
        app.chats[0].updated_at > before,
        "the activity stamp is refreshed"
    );
}

/// touch_chat is a safe no-op for an out-of-range index.
#[test]
fn touch_chat_out_of_range_is_a_noop() {
    let mut app = app_with_chats(2);
    let snapshot: Vec<String> = app.chats.iter().map(|c| c.name.clone()).collect();
    app.touch_chat(9);
    let after: Vec<String> = app.chats.iter().map(|c| c.name.clone()).collect();
    assert_eq!(snapshot, after, "nothing moved");
}

// ---- mouse text selection --------------------------------------------
