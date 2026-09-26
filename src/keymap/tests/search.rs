use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

// key routing ----------------------------------

#[test]
fn ctrl_f_opens_search_popup() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
    assert_eq!(app.search_query(), Some(""));
}

/// Typing while the search popup is open must go to the QUERY buffer,
/// never into the message draft (modal-ish isolation).
#[test]
fn search_typing_goes_to_query_not_input() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "precious draft");
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    type_in(&mut app, "needle");

    assert_eq!(app.search_query(), Some("needle"));
    assert_eq!(
        app.input.lines().join(""),
        "precious draft",
        "message draft untouched while searching"
    );
}

#[test]
fn search_up_down_navigate_and_enter_jumps() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::user("needle one"));
    app.chats[0].messages.push(Message::assistant("needle two"));
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    type_in(&mut app, "needle");
    assert_eq!(app.search_matches().len(), 2);
    assert_eq!(app.search_selected(), 0);

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.search_selected(), 1);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.search_selected(), 0);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal, "Enter closes popup");
    assert!(app.pending_search_jump.is_some(), "jump recorded");
    assert_eq!(app.chats[0].messages.len(), 2, "messages untouched");
}

/// Esc closes the search popup WITHOUT a jump; the chat view stays put.
#[test]
fn search_esc_closes_without_jumping() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::user("needle"));
    app.scroll_up(12);
    let scroll_before = app.scroll;
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    type_in(&mut app, "needle");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert!(app.pending_search_jump.is_none(), "Esc must not jump");
    assert_eq!(app.scroll, scroll_before, "view unchanged by Esc");
}

/// PgUp/PgDn are disabled inside the search popup — the chat scroll is
/// driven by the jump, never by page keys.
#[test]
fn search_popup_swallows_pgup_pgdn_and_arrows() {
    let mut app = app_with_chats(2);
    app.scroll_up(30);
    let scroll_before = app.scroll;
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    // the Alt synonym is swallowed by the popup too.
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE); // navigation, not caret
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    // the Alt synonym is swallowed by the popup too.
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);

    assert!(
        matches!(app.mode, chibi_tui::app::Mode::Searching { .. }),
        "popup must stay open"
    );
    assert_eq!(app.active, 0, "thread switch must not fire");
    assert_eq!(app.chats.len(), 2, "Ctrl+N must not fire");
    assert_eq!(app.scroll, scroll_before, "PgUp/PgDn must not scroll");
    assert!(
        !matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
        "Ctrl+L must not fire"
    );
    assert!(!app.should_quit);
    assert!(app.pending_search_jump.is_none());
}

/// Ctrl+C opens the quit confirmation from the search popup (same
/// class as the other popups); the second Ctrl+C IS the confirmation.
#[test]
fn search_popup_ctrl_c_opens_quit_confirmation() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm, "first Ctrl+C opens the confirmation");
    assert!(!app.should_quit);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.should_quit, "second Ctrl+C confirms the quit");
    assert_eq!(app.chats.len(), 1, "quit must not mutate chats");
}

/// The confirm-delete popup and the search popup cannot coexist: Ctrl+F
/// is swallowed while the delete confirmation is open, and Ctrl+D is
/// swallowed while searching.
#[test]
fn search_and_delete_popups_cannot_coexist() {
    // Ctrl+F over the delete confirm popup: swallowed.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete));
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete),
        "Ctrl+F must not open search over the delete popup"
    );

    // Ctrl+D over the search popup: swallowed.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::Searching { .. }),
        "Ctrl+D must not open delete popup over search"
    );
    assert_eq!(app.chats.len(), 1, "nothing deleted");
}

/// The search popup's Enter must never submit the message draft — the
/// loop-level `enter_consumed_by_search` gate mirrors the delete-popup
/// handling (asserted via handle_key: mode closes, jump recorded, and
/// the draft survives untouched; submission is loop-gated identically to
/// `enter_consumed_by_delete`).
#[test]
fn search_enter_never_touches_draft_or_messages() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "draft text");
    app.chats[0].messages.push(Message::user("needle"));
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    type_in(&mut app, "needle");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert!(app.pending_search_jump.is_some());
    assert_eq!(
        app.input.lines().join(""),
        "draft text",
        "draft must survive the search Enter"
    );
    assert_eq!(app.chats[0].messages.len(), 1, "no message appended");
    assert!(app.active_request_id().is_none());
    assert_eq!(app.active_queue_len(), 0);
}

/// Search works on a BUSY chat (read-only): opening the popup and
/// jumping leaves the in-flight request untouched.
#[test]
fn search_opens_while_busy_and_leaves_lifecycle_alone() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::user("needle"));
    let submitted = submit_text(&mut app, "in flight");
    assert!(app.is_busy());

    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    type_in(&mut app, "needle");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(
        app.chats[0].lifecycle.request_id(),
        Some(submitted.request_id.as_str()),
        "in-flight request untouched by search"
    );
}
