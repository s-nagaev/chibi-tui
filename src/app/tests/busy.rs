use super::support::*;

#[test]
fn is_busy_tracks_active_chat_lifecycle() {
    let mut app = app_with_chats(1);
    assert!(!app.is_busy());
    submit_text(&mut app, "work");
    assert!(app.is_busy());
    finish_chat(&mut app, 0);
    assert!(!app.is_busy());
}

#[test]
fn any_busy_spans_all_chats() {
    let mut app = app_with_chats(2);
    assert!(!app.any_busy());
    submit_text(&mut app, "work");
    assert!(app.any_busy());
    finish_chat(&mut app, 0);
    assert!(!app.any_busy());
}

// local cancel fallback ---------------------------------
