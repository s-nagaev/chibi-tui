use super::support::*;

use crate::*;

// bootstrap restore + pointer updates --------

#[test]
fn bootstrap_reopens_the_last_active_thread_with_its_sticky_state() {
    let dir = temp_history_dir("restore");
    let first = persisted_chat("first", None);
    let second = persisted_chat(
        "second",
        Some(chibi_tui::protocol::Usage {
            input_tokens: 900_000,
            output_tokens: 1,
            context_window: Some(1_000_000),
        }),
    );
    history::save_chat_in(Some(&dir), &first).expect("save first");
    history::save_chat_in(Some(&dir), &second).expect("save second");
    history::save_last_thread_in(Some(&dir), &second.id).expect("save pointer");

    let app = bootstrap_app(Some(&dir));
    assert_eq!(
        app.active_thread_id(),
        Some(second.id.as_str()),
        "startup re-opens the remembered thread"
    );
    assert_eq!(app.chat_title(), "second");
    assert_eq!(
        app.last_turn_usage.map(|u| u.input_tokens),
        Some(900_000),
        "sticky ctx state is seeded from the restored thread's snapshot"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn bootstrap_with_missing_dangling_or_corrupt_pointer_uses_default_startup() {
    // Missing pointer: plain default startup (first sorted snapshot).
    let dir = temp_history_dir("no-pointer");
    let a = persisted_chat("a", None);
    let b = persisted_chat("b", None);
    history::save_chat_in(Some(&dir), &a).expect("save a");
    history::save_chat_in(Some(&dir), &b).expect("save b");
    let app = bootstrap_app(Some(&dir));
    assert_eq!(app.active, 0, "no pointer: default startup selection");
    assert_eq!(app.chats.len(), 2, "nothing lost either");
    std::fs::remove_dir_all(&dir).ok();

    // Dangling pointer: the remembered id has no thread file.
    let dir = temp_history_dir("dangling");
    let only = persisted_chat("only", None);
    history::save_chat_in(Some(&dir), &only).expect("save only");
    history::save_last_thread_in(Some(&dir), &chibi_tui::history::new_thread_id())
        .expect("dangling pointer");
    let app = bootstrap_app(Some(&dir));
    assert_eq!(app.active, 0, "dangling pointer: default startup");
    assert_eq!(app.active_thread_id(), Some(only.id.as_str()));
    std::fs::remove_dir_all(&dir).ok();

    // Corrupt pointer: unreadable JSON must never panic or disturb the
    // startup selection.
    let dir = temp_history_dir("corrupt");
    let only = persisted_chat("only", None);
    history::save_chat_in(Some(&dir), &only).expect("save only");
    std::fs::write(dir.join("last-thread.json"), "{broken").expect("corrupt pointer");
    let app = bootstrap_app(Some(&dir));
    assert_eq!(app.active, 0, "corrupt pointer: default startup");
    assert_eq!(app.active_thread_id(), Some(only.id.as_str()));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn switching_threads_updates_the_persisted_pointer() {
    let dir = temp_history_dir("switch");
    let app = app_with_chats(2);
    let first_id = app.chats[0].id.clone();
    let second_id = app.chats[1].id.clone();

    // The run loop's exact seam: observe, then switch, then observe.
    let mut tracked: Option<String> = None;
    note_active_thread(&mut tracked, &app, Some(&dir));
    assert_eq!(
        history::load_last_thread_in(Some(&dir)).as_deref(),
        Some(first_id.as_str()),
        "the startup selection is recorded"
    );

    let mut app = app;
    app.select_next();
    note_active_thread(&mut tracked, &app, Some(&dir));
    assert_eq!(
        history::load_last_thread_in(Some(&dir)).as_deref(),
        Some(second_id.as_str()),
        "a thread switch rewrites the pointer"
    );

    app.select_prev();
    note_active_thread(&mut tracked, &app, Some(&dir));
    assert_eq!(
        history::load_last_thread_in(Some(&dir)).as_deref(),
        Some(first_id.as_str()),
        "switching back is recorded too"
    );
    std::fs::remove_dir_all(&dir).ok();
}
