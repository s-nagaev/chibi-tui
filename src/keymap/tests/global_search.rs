use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

// key routing ----------------------------

/// Ctrl+Shift+F opens the GLOBAL search popup (kitty-protocol chord:
/// Char('f') + CONTROL|SHIFT).
#[test]
fn ctrl_shift_f_opens_global_search_popup() {
    let mut app = app_with_chats(2);
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::SearchingAll { .. }
    ));
    assert_eq!(app.search_all_query(), Some(""));
}

/// Regression guard: plain Ctrl+F must STILL open the in-thread search —
/// the new Shift chord must not steal it.
#[test]
fn ctrl_f_still_opens_in_thread_search_not_global() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
    assert!(!matches!(
        app.mode,
        chibi_tui::app::Mode::SearchingAll { .. }
    ));
}

/// Typing while the global search popup is open must go to the QUERY
/// buffer, never into the message draft (modal-ish isolation).
#[test]
fn global_search_typing_goes_to_query_not_input() {
    let mut app = app_with_chats(2);
    type_in(&mut app, "precious draft");
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    type_in(&mut app, "needle");

    assert_eq!(app.search_all_query(), Some("needle"));
    assert_eq!(
        app.input.lines().join(""),
        "precious draft",
        "message draft untouched while searching"
    );
}

#[test]
fn global_search_up_down_navigate_and_enter_jumps() {
    let mut app = app_with_chats(2);
    app.chats[0].messages.push(Message::user("needle one"));
    app.chats[1].messages.push(Message::assistant("needle two"));
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    type_in(&mut app, "needle");
    assert_eq!(app.search_all_matches().len(), 2);
    assert_eq!(app.search_all_selected(), 0);

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.search_all_selected(), 1);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.search_all_selected(), 0);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal, "Enter closes popup");
    assert_eq!(app.active, 0, "first match belongs to chat 0");
    assert!(app.pending_global_search_jump.is_some(), "jump recorded");
    assert!(
        app.pending_search_jump.is_none(),
        "in-thread jump state untouched"
    );
    assert_eq!(app.chats[0].messages.len(), 1, "messages untouched");
}

/// Enter switches the active chat to the match's thread — including a
/// NON-active one (the Ctrl+↑/↓ selection mechanics).
#[test]
fn global_search_enter_switches_to_non_active_thread() {
    let mut app = app_with_chats(3);
    app.chats[2]
        .messages
        .push(Message::user("needle in chat two"));
    app.active = 0;
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    type_in(&mut app, "needle");
    assert_eq!(app.search_all_matches()[0].chat_index, 2);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(app.active, 2, "target thread activated");
    assert_eq!(app.scroll, 0, "thread switch resets chat scroll");
}

/// Esc closes the global search popup WITHOUT switching threads or
/// jumping; the view stays put.
#[test]
fn global_search_esc_closes_without_switching_or_jumping() {
    let mut app = app_with_chats(2);
    app.chats[1].messages.push(Message::user("needle"));
    app.active = 0;
    app.scroll_up(12);
    let scroll_before = app.scroll;
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    type_in(&mut app, "needle");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(app.active, 0, "no thread switch on Esc");
    assert_eq!(app.scroll, scroll_before, "view unchanged by Esc");
    assert!(
        app.pending_global_search_jump.is_none(),
        "Esc must not jump"
    );
    assert!(app.pending_search_jump.is_none());
}

/// The global search popup swallows every other key: no textarea
/// leakage, no global bindings (Ctrl+N/R/L/D/F, arrows nav, PgUp/PgDn),
/// no accidental quit via q.
#[test]
fn global_search_popup_swallows_global_bindings() {
    let mut app = app_with_chats(2);
    app.scroll_up(30);
    let scroll_before = app.scroll;
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );

    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    // the Alt synonym is swallowed by the popup too.
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    // Ctrl+F must not open the in-thread search over the global one.
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);

    assert!(
        matches!(app.mode, chibi_tui::app::Mode::SearchingAll { .. }),
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
    assert!(app.pending_global_search_jump.is_none());
}

/// Ctrl+C opens the quit confirmation from the global search popup
/// (same class as the other popups); the second Ctrl+C confirms.
#[test]
fn global_search_popup_ctrl_c_opens_quit_confirmation() {
    let mut app = app_with_chats(1);
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm, "first Ctrl+C opens the confirmation");
    assert!(!app.should_quit);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.should_quit, "second Ctrl+C confirms the quit");
    assert_eq!(app.chats.len(), 1, "quit must not mutate chats");
}

/// The confirm-delete popup and the global search popup cannot coexist:
/// Ctrl+Shift+F is swallowed while the delete confirmation is open, and
/// Ctrl+D is swallowed while searching globally.
#[test]
fn global_search_and_delete_popups_cannot_coexist() {
    // Ctrl+Shift+F over the delete confirm popup: swallowed.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete));
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete),
        "Ctrl+Shift+F must not open global search over the delete popup"
    );

    // Ctrl+D over the global search popup: swallowed.
    let mut app = app_with_chats(1);
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::SearchingAll { .. }),
        "Ctrl+D must not open delete popup over global search"
    );
    assert_eq!(app.chats.len(), 1, "nothing deleted");
}

/// The global search popup's Enter must never submit the message draft —
/// the loop-level `enter_consumed_by_search_all` gate mirrors the
/// in-thread search handling (asserted via handle_key: mode closes, the
/// target thread activates, jump recorded, draft survives untouched).
#[test]
fn global_search_enter_never_touches_draft_or_messages() {
    let mut app = app_with_chats(2);
    type_in(&mut app, "draft text");
    app.chats[1].messages.push(Message::user("needle"));
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    type_in(&mut app, "needle");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(app.active, 1, "target thread activated");
    assert!(app.pending_global_search_jump.is_some());
    assert_eq!(
        app.input.lines().join(""),
        "draft text",
        "draft must survive the global search Enter"
    );
    assert_eq!(app.chats[1].messages.len(), 1, "no message appended");
    assert!(app.active_request_id().is_none());
    assert_eq!(app.active_queue_len(), 0);
}
