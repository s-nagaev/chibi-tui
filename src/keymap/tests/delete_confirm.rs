use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

// key routing ----------------------------------

#[test]
fn ctrl_d_opens_confirm_popup_on_idle_chat() {
    let mut app = app_with_chats(2);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
}

#[test]
fn ctrl_d_on_busy_chat_shows_status_and_never_opens_popup() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "in flight");
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert!(app.status_message.is_some(), "refusal toast shown");
    assert_eq!(app.chats.len(), 1, "nothing deleted");
}

/// Enter and `y` confirm through the full key path; both remove the
/// chat and record the pending file deletion for the event loop.
#[test]
fn confirm_popup_enter_and_y_confirm_deletion() {
    for code in [KeyCode::Enter, KeyCode::Char('y')] {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
        press(&mut app, code, KeyModifiers::NONE);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(app.chats.len(), 2, "{code:?} confirmed deletion");
        assert!(app.pending_delete.is_some(), "{code:?} set the delete");
    }
}

/// Esc and `n` cancel through the full key path: nothing is deleted and
/// no file removal is requested.
#[test]
fn confirm_popup_esc_and_n_cancel_deletion() {
    for code in [KeyCode::Esc, KeyCode::Char('n')] {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        press(&mut app, code, KeyModifiers::NONE);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(app.chats.len(), 3, "{code:?} cancelled, nothing deleted");
        assert!(app.pending_delete.is_none());
        assert_eq!(app.chats[0].name, "chat-0");
    }
}

/// The confirm popup swallows every other key: no textarea leakage, no
/// global bindings (Ctrl+N/R/L/F, arrows nav), no accidental quit via q.
#[test]
fn confirm_popup_isolates_keystrokes_and_suspends_global_bindings() {
    let mut app = app_with_chats(3);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);

    // Typing must not reach the textarea (letters a/b/c avoid the
    // popup's own y/n confirm-cancel keys).
    type_in(&mut app, "abc");
    assert!(app.input.lines().iter().all(|l| l.is_empty()));
    // Global bindings suspended.
    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    // the Alt synonym is swallowed by the popup too.
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(app.chats.len(), 3, "Ctrl+N must not fire mid-popup");
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete),
        "popup must stay open"
    );
    assert!(
        !matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
        "Ctrl+L must not fire"
    );
    assert!(!app.should_quit);
    // q is deliberately unbound inside the popup — a stray q must not
    // quit while a destructive confirmation is on screen.
    press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(!app.should_quit, "q must not quit from the confirm popup");

    // The popup is still fully functional afterwards.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(app.chats.len(), 3);
}

/// Ctrl+C inside the delete confirm opens the quit confirmation (same
/// class as the error popup) — it must NOT delete the chat. Dismissing
/// the quit confirm restores the delete popup exactly (the confirm is
/// a flag, not a Mode — the popup below never moved).
#[test]
fn confirm_popup_ctrl_c_opens_quit_confirmation_without_deleting() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm);
    assert!(!app.should_quit);
    assert_eq!(app.chats.len(), 1, "quit confirm must not delete the chat");
    assert!(app.pending_delete.is_none());

    // n stays in the app AND in the delete popup.
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(!app.quit_confirm, "n dismisses the quit confirm");
    assert_eq!(
        app.mode,
        chibi_tui::app::Mode::ConfirmDelete,
        "the delete popup is exactly where it was"
    );
    assert!(!app.should_quit);

    // The restored popup still completes its own flow.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(app.chats.len(), 1);
}

/// Deleting the last chat reaches the clean empty state through the full
/// key path; the confirm Enter never submits a pre-typed draft.
#[test]
fn confirm_delete_last_chat_reaches_empty_state_via_keys() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "draft text");
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.chats.is_empty());
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert!(app.pending_delete.is_some());
    assert!(app.active_request_id().is_none());
    // The draft survives the deletion untouched (thread ops never clear
    // the prompt buffer in this app).
    assert_eq!(app.input.lines().join(""), "draft text");
}

/// With a neighbour present, the confirm Enter must never submit the
/// draft to it — the popup owns the Enter.
#[test]
fn confirm_enter_never_submits_draft_to_neighbour() {
    let mut app = app_with_chats(2);
    type_in(&mut app, "draft text");
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.chats.len(), 1);
    assert_eq!(app.chats[0].name, "chat-1", "neighbour selected");
    assert!(
        app.chats[0].messages.is_empty(),
        "the draft must not be submitted to the neighbour"
    );
    assert!(app.chats[0].queue.is_empty());
    assert!(app.active_request_id().is_none());
}
