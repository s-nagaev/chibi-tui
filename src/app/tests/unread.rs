use super::clone::clone_capable_app;
use super::model_picker::{open_picker, CAPTURED_LISTING};
use super::support::*;
use crate::app::*;

/// A visible reply landing in a BACKGROUND chat flags it unread; the
/// selected chat's own reply never flags anything. The editor draft of
/// the other thread stays untouched by the arrival.
#[test]
fn background_result_marks_chat_unread_and_active_result_does_not() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "background work"); // chat 0 goes busy
    app.select_next(); // user reads chat 1 while chat 0 works
    type_in(&mut app, "draft in flight, do not disturb");
    assert!(!app.chats[0].unread, "nothing arrived yet");

    finish_chat(&mut app, 0);
    assert!(app.chats[0].unread, "background reply must flag the thread");
    assert!(!app.chats[1].unread);
    assert_eq!(
        app.input.lines().join(""),
        "draft in flight, do not disturb",
        "mid-typing draft untouched by the background arrival"
    );

    // The active chat's own reply is on screen: no flag. (Submitting
    // lifts chat-1 to the top, so look it up by name, not position.)
    submit_text(&mut app, "foreground work");
    {
        let i = idx(&app, "chat-1");
        finish_chat(&mut app, i);
    }
    assert!(
        !app.chats[idx(&app, "chat-1")].unread,
        "selected chat never flags itself"
    );
    assert!(
        app.chats[idx(&app, "chat-0")].unread,
        "other flag survives unrelated traffic"
    );
}

/// Selecting the flagged thread clears its marker (the single
/// select_chat point behind both arrows), and passing a flagged thread
/// by without entering it keeps the flag.
#[test]
fn selecting_the_thread_clears_its_unread_marker() {
    let mut app = app_with_chats(3);
    submit_text(&mut app, "work in chat 0");
    app.select_next(); // active = 1
    app.select_next(); // active = 2
    finish_chat(&mut app, 0);
    assert!(app.chats[0].unread);

    app.select_prev(); // into chat 1, not the flagged one
    assert!(app.chats[0].unread, "passing by must not clear the flag");
    app.select_prev(); // INTO chat 0
    assert!(
        !app.chats[0].unread,
        "entering the thread clears its marker"
    );

    // Re-flag, then clear via the other arrow direction.
    submit_text(&mut app, "work again in chat 0");
    app.select_next(); // chat 1
    finish_chat(&mut app, 0);
    assert!(app.chats[0].unread);
    app.select_prev(); // back into chat 0
    assert!(!app.chats[0].unread);
}

/// The global-search jump activates the target thread through the same
/// selection point, so its marker clears on entry too.
#[test]
fn global_search_jump_clears_the_target_thread_marker() {
    let mut app = app_with_chats(2);
    app.chats[1]
        .messages
        .push(Message::assistant("needle here"));
    // Flag chat 1 the way a background reply would (white-box: the
    // routing that sets the flag is covered by the dedicated tests).
    app.chats[1].unread = true;

    app.begin_search_all();
    for ch in "needle".chars() {
        app.search_all_push(ch);
    }
    assert_eq!(app.search_all_matches()[0].chat_index, 1);
    assert!(app.jump_to_selected_all());
    assert_eq!(app.active, 1, "target thread activated");
    assert!(
        !app.chats[1].unread,
        "entering via the search jump clears the flag"
    );
}

/// Only readable content flags a background thread: a blank/pure-ACK
/// absorb and a hidden plumbing exchange stay unflagged, while an
/// inline error reply does flag.
#[test]
fn unread_flag_follows_visible_content_only() {
    // Blank/ACK absorb: nothing to read, no flag.
    let mut app = app_with_chats(2);
    submit_text(&mut app, "bg blank");
    app.select_next();
    finish_chat_with_content(&mut app, 0, ACK_MARKER);
    assert!(!app.chats[0].unread, "absorbed ACK must not flag");

    // Hidden exchange (model listing fetch) started on chat 0, then the
    // user switches away before it resolves: plumbing nobody reads,
    // no flag, the picker still resolves.
    let mut app = app_with_chats(2);
    open_picker(&mut app);
    let req = app.chats[0].lifecycle.request_id().unwrap().to_owned();
    app.select_next();
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&req),
        markdown: CAPTURED_LISTING.into(),
        thread_id: app.chats[0].id.clone(),
        model: None,
    });
    assert!(
        !app.chats[0].unread,
        "hidden plumbing exchange must not flag"
    );
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);

    // An inline error reply IS content the user has not seen.
    let mut app = app_with_chats(2);
    let submitted = submit_text(&mut app, "bg doomed");
    app.select_next();
    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&submitted.request_id),
        message: "backend exploded".into(),
        thread_id: Some(app.chats[0].id.clone()),
    });
    assert!(app.chats[0].unread, "background error reply flags");
}

/// A clone ack lists and selects the new thread through the single
/// selection point; the fresh copy starts with no marker.
#[test]
fn clone_insert_starts_without_unread_marker() {
    let mut app = clone_capable_app(1);
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
    assert_eq!(app.active, 0, "clone selected");
    assert!(!app.chats[0].unread, "fresh clone carries no marker");
}
