use super::support::*;
use crate::app::*;

#[test]
fn ctrl_d_opens_confirm_popup_on_idle_chat() {
    let mut app = app_with_chats(2);
    assert_eq!(app.mode, Mode::Normal);
    app.begin_delete_confirm();
    assert_eq!(app.mode, Mode::ConfirmDelete);
    assert!(
        app.status_message.is_none(),
        "no refusal toast on a deletable chat"
    );
}

#[test]
fn ctrl_d_refuses_busy_chat_with_status_toast() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "long running");
    assert!(app.is_busy());

    app.begin_delete_confirm();
    assert_eq!(app.mode, Mode::Normal, "popup must never open while busy");
    let (msg, _) = app.status_message.as_ref().expect("toast shown");
    assert!(msg.contains("busy"), "toast text: {msg:?}");
    assert_eq!(app.chats.len(), 1, "chat untouched");
}

#[test]
fn ctrl_d_refuses_chat_with_queued_prompts() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "first");
    type_in(&mut app, "second");
    assert!(app.take_input().is_none(), "busy chat enqueues");
    assert_eq!(app.active_queue_len(), 1);

    app.begin_delete_confirm();
    assert_eq!(app.mode, Mode::Normal, "queued prompts block deletion");
    assert!(app.status_message.is_some());
}

/// Deleting an idle chat while ANOTHER thread runs in the background is
/// allowed: the guard inspects only the active chat about to be deleted,
/// and the background request's lifecycle stays untouched.
#[test]
fn busy_background_chat_does_not_block_deleting_idle_active_chat() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "background work"); // chat 0 busy
    app.select_next(); // chat 1 (idle) becomes active
    assert!(!app.is_busy());

    app.begin_delete_confirm();
    assert_eq!(app.mode, Mode::ConfirmDelete, "idle chat is deletable");
    assert!(app.status_message.is_none());
    assert!(
        matches!(app.chats[0].lifecycle, ChatLifecycle::Awaiting { .. }),
        "background request untouched"
    );
}

#[test]
fn begin_delete_confirm_noop_outside_normal_mode_and_without_chats() {
    let mut app = App::new(Vec::new());
    app.begin_delete_confirm();
    assert_eq!(app.mode, Mode::Normal, "no active chat ⇒ no popup");

    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app); // rename session open
    app.begin_delete_confirm();
    assert!(
        matches!(app.mode, Mode::Renaming { .. }),
        "rename session owns the keyboard — no popup"
    );
}

#[test]
fn confirm_delete_removes_chat_and_selects_next_neighbour() {
    let mut app = app_with_chats(3);
    let removed_id = app.chats[0].id.clone();
    app.scroll_up(30); // detach from bottom before deleting
    assert!(!app.at_bottom());

    app.begin_delete_confirm();
    let removed = app.confirm_delete().expect("removed thread id");

    assert_eq!(removed, removed_id);
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.active, 0, "next chat slides into the slot");
    assert_eq!(app.chats[0].name, "chat-1");
    assert_eq!(app.mode, Mode::Normal, "popup closed by confirm");
    assert_eq!(
        app.pending_delete.as_deref(),
        Some(removed_id.as_str()),
        "file removal handed to the event loop"
    );
    assert_eq!(app.scroll, 0, "follow-bottom after delete");
    assert!(app.at_bottom());
}

#[test]
fn confirm_delete_middle_chat_selects_next() {
    let mut app = app_with_chats(3);
    app.active = 1;
    app.begin_delete_confirm();
    app.confirm_delete();
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.active, 1, "former chat-2 is the next neighbour");
    assert_eq!(app.chats[1].name, "chat-2");
}

#[test]
fn confirm_delete_last_chat_selects_prev_neighbour() {
    let mut app = app_with_chats(3);
    app.active = 2;
    app.begin_delete_confirm();
    app.confirm_delete();
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.active, 1, "previous chat selected after removing last");
    assert_eq!(app.chats[1].name, "chat-1");
    assert_eq!(app.scroll, 0);
}

/// Deleting the only chat reaches the clean empty state: zero chats,
/// active index parked at 0, follow-bottom, no stuck lifecycle.
#[test]
fn confirm_delete_last_chat_reaches_clean_empty_state() {
    let mut app = app_with_chats(1);
    app.begin_delete_confirm();
    let removed = app.confirm_delete();

    assert!(removed.is_some());
    assert!(app.chats.is_empty());
    assert_eq!(app.active, 0);
    assert_eq!(app.scroll, 0);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.at_bottom());
    assert!(!app.is_busy());
    assert!(app.active_request_id().is_none());
    assert!(app.pending_delete.is_some());
}

#[test]
fn cancel_delete_keeps_chat_and_draft_untouched() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "precious draft");
    app.begin_delete_confirm();

    assert!(app.cancel_delete());
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.chats.len(), 1);
    assert_eq!(app.chats[0].name, "chat-0");
    assert_eq!(app.input.lines().join(""), "precious draft");
    assert!(app.pending_delete.is_none());
    assert!(!app.cancel_delete(), "cancelling again is a no-op");
}

#[test]
fn confirm_delete_without_popup_is_noop() {
    let mut app = app_with_chats(1);
    assert!(app.confirm_delete().is_none());
    assert_eq!(app.chats.len(), 1);
    assert!(app.pending_delete.is_none());
}

// detection, guards, ack/error resolution -------
