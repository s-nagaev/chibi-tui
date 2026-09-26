use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

#[test]
fn esc_clears_input_or_is_ignored_but_never_quits() {
    let mut app = app_with_chats(1);

    // Esc while busy: ignored (no quit, no clear — nothing to clear).
    submit_text(&mut app, "in flight");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.should_quit, "Esc during a request must not quit");

    // After cancel resolves, still idle: Esc is still ignored.
    app.resolve_cancel_locally();
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        !app.should_quit,
        "Esc in idle must never quit — only a confirmed Ctrl+C does"
    );

    // Non-empty input: Esc clears it.
    for ch in "hello".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        app.input.lines().iter().all(|l| l.is_empty()),
        "Esc clears non-empty input"
    );
    assert!(!app.should_quit, "Esc after clearing input must not quit");
}

// Single-letter commands (q/j/k/N) were removed — they conflicted with the
// first character of typed words. Navigation uses ↑/↓; new chat uses Ctrl+N;
// quit uses Ctrl+C only.

#[test]
fn ctrl_n_creates_new_chat() {
    let mut app = app_with_chats(1);
    assert_eq!(app.chats.len(), 1);
    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert_eq!(app.chats.len(), 2, "Ctrl+N creates a new chat");
}
