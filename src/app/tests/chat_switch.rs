use super::support::*;
use crate::app::*;

#[test]
fn new_chat_inserts_on_top_selects_and_gets_fresh_uuid() {
    let mut app = app_with_chats(2);
    let existing_ids: Vec<String> = app.chats.iter().map(|c| c.id.clone()).collect();
    app.new_chat();
    assert_eq!(app.chats.len(), 3);
    assert_eq!(app.active, 0, "the fresh chat must be selected");
    assert_eq!(app.chat_title(), "New chat 3");
    let fresh = &app.chats[0];
    assert!(!fresh.id.is_empty());
    assert!(!existing_ids.contains(&fresh.id), "thread ids are unique");
    assert!(fresh.messages.is_empty());
    assert_eq!(fresh.lifecycle, ChatLifecycle::Idle);
    assert!(fresh.queue.is_empty());
}

#[test]
fn switching_chats_keeps_history_and_selection() {
    let mut app = app_with_chats(3);
    app.select_next();
    assert_eq!(app.active, 1);
    assert_eq!(app.chats[0].name, "chat-0", "history untouched");
    app.select_prev();
    assert_eq!(app.active, 0);
    // saturating at the top edge
    app.select_prev();
    assert_eq!(app.active, 0);
}

// Chat ↔ Sidebar focus state ----------------------

#[test]
fn thread_ids_are_unique_across_many_new_chats() {
    let mut app = app_with_chats(0);
    let mut ids = std::collections::HashSet::new();
    for _ in 0..50 {
        app.new_chat();
        ids.insert(app.chats.first().unwrap().id.clone());
    }
    assert_eq!(ids.len(), 50, "every new chat gets a unique thread id");
}

// error popup ------------------------------------------
