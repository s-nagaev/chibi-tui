use crate::tests::support::*;

use crate::*;

use crossterm::event::{KeyCode, KeyModifiers};

// key routing -----------------------------------

#[test]
fn ctrl_r_opens_rename_session() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(
        app.mode,
        chibi_tui::app::Mode::Renaming {
            buf: "chat-0".to_owned()
        }
    );
}

/// While renaming, plain keystrokes must land in the DRAFT, never in the
/// prompt textarea (key routing checks rename mode BEFORE normal input).
#[test]
fn keystrokes_go_to_draft_not_input_while_renaming() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "precious prompt");

    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    for ch in "X".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }

    assert_eq!(app.rename_buf(), Some("chat-0X"));
    assert_eq!(
        app.input.lines().join(""),
        "precious prompt",
        "prompt textarea untouched while renaming"
    );
}

/// Enter inside rename mode saves and must NEVER submit a message — even
/// when the draft is rejected (empty), nothing leaks into the chat.
#[test]
fn enter_while_renaming_saves_and_never_submits() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    // Wipe the prefilled draft to make the save REJECTED (empty name).
    while app.rename_buf().is_some_and(|b| !b.is_empty()) {
        app.rename_backspace();
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.mode, chibi_tui::app::Mode::Normal, "session closed");
    assert_eq!(app.chats[0].name, "chat-0", "rejected: old name kept");
    assert!(
        app.chats[0].messages.is_empty(),
        "no message bubble leaked from the rename Enter"
    );
    assert!(app.take_input().is_none(), "input buffer still empty");
}

#[test]
fn esc_while_renaming_cancels_and_restores_input_state() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "draft message");

    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    for ch in "junk".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(app.chats[0].name, "chat-0", "rename discarded");
    assert_eq!(
        app.input.lines().join(""),
        "draft message",
        "normal input state restored verbatim"
    );
}

/// Chat navigation is blocked mid-rename in ALL modifier flavors so the
/// active chat cannot silently change under the open editor
/// (plain ↑/↓ AND Ctrl+↑/↓).
#[test]
fn arrows_do_not_switch_chats_while_renaming() {
    let mut app = app_with_chats(3);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);

    // Plain arrows.
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active, 0, "navigation suppressed during rename");
    assert!(!matches!(app.mode, chibi_tui::app::Mode::Normal));

    // the new thread-switch bindings must not leak
    // into rename mode either.
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(app.active, 0, "Ctrl+arrows suppressed during rename");
    assert!(matches!(app.mode, Mode::Renaming { .. }), "session intact");

    // the Alt synonym is blocked mid-rename too.
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(app.active, 0, "Alt+arrows suppressed during rename");
    assert!(matches!(app.mode, Mode::Renaming { .. }), "session intact");
}

/// Ctrl+C keeps its global meaning in rename mode: with an open session
/// it first cancels the draft; only a second Ctrl+C acts on the request.
#[test]
fn ctrl_c_while_renaming_cancels_draft_first() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "in flight");
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(
        app.pending_cancel.is_none(),
        "first Ctrl+C drops the draft, not the request"
    );
    assert_eq!(
        app.mode,
        chibi_tui::app::Mode::Normal,
        "session closed by Ctrl+C"
    );
    assert!(app.chats[0].lifecycle.request_id().is_some());

    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    let (request_id, _) = app.pending_cancel.expect("second Ctrl+C cancels request");
    assert_eq!(request_id, submitted.request_id);
    assert!(
        !app.quit_confirm,
        "a cancel never doubles as a quit confirmation"
    );
}

/// The error popup captures keys BEFORE rename mode can be entered:
/// pressing Ctrl+R while a popup is open routes to popup handling.
#[test]
fn popup_blocks_ctrl_r_rename_entry() {
    let mut app = app_with_chats(1);
    app.show_error("boom");
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);

    assert!(
        matches!(app.mode, chibi_tui::app::Mode::Normal),
        "Ctrl+R must not open rename mode through an error popup"
    );
}

/// Ctrl+R is consumed as the hotkey in Normal mode — it must not insert
/// anything into the prompt buffer (regression guard for routing order).
#[test]
fn ctrl_r_in_normal_mode_never_touches_input_buffer() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    app.cancel_rename();
    assert!(
        app.input.lines().iter().all(|l| l.is_empty()),
        "Ctrl+R itself must leave the prompt empty"
    );
}
