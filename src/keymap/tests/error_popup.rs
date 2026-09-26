use crate::tests::support::*;

use crate::*;

use crossterm::event::{KeyCode, KeyModifiers};

// Single-letter commands (q/j/k/N) were removed — they conflicted with the
// first character of typed words. Navigation uses ↑/↓; new chat uses Ctrl+N;
// quit uses Ctrl+C only.

// ---- error popup key capture --------------------------------------------

#[test]
fn popup_r_requests_reconnect() {
    let mut app = app_with_chats(1);
    app.show_error("broken pipe");
    press(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);

    assert_eq!(app.reconnect_requested, Some(ReconnectRequest {}));
    assert!(
        app.error_popup.is_some(),
        "popup stays until reconnect resolves"
    );
    assert!(!app.should_quit);
}

#[test]
fn popup_esc_dismisses_but_stays_alive() {
    let mut app = app_with_chats(1);
    app.show_error("boom");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        app.error_popup.is_none(),
        "Esc must dismiss the error popup"
    );
    assert!(!app.should_quit, "Esc must not quit from the popup");
}

/// `q` / Ctrl+C from the error popup open the quit confirmation (the
/// exit itself still needs the confirm); the error popup stays open
/// underneath, so dismissing the confirm restores it exactly.
#[test]
fn popup_q_and_ctrl_c_open_quit_confirmation() {
    for (code, mods) in [
        (KeyCode::Char('q'), KeyModifiers::NONE),
        (KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut app = app_with_chats(1);
        app.show_error("boom");
        press(&mut app, code, mods);
        assert!(app.quit_confirm, "{code:?} must open the confirmation");
        assert!(!app.should_quit, "{code:?} must not quit directly");
        assert!(
            app.error_popup.is_some(),
            "the error popup stays open underneath"
        );
    }
}

/// Dismissing the quit confirm opened from the error popup restores
/// the popup state exactly (the confirm is a flag, not a Mode — the
/// error popup never went anywhere).
#[test]
fn dismissing_quit_confirm_over_error_popup_restores_it() {
    let mut app = app_with_chats(1);
    app.show_error("boom");
    press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(app.quit_confirm);

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.quit_confirm, "Esc dismisses the confirm");
    assert!(!app.should_quit, "dismiss keeps the app alive");
    assert!(
        app.error_popup.is_some(),
        "the error popup is exactly where it was"
    );
    assert!(app.reconnect_requested.is_none(), "no stray reconnect");
    // And the restored popup still works end to end.
    press(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    assert!(app.reconnect_requested.is_some(), "R still reconnects");
}

#[test]
fn popup_other_keys_dismiss_without_quitting() {
    let mut app = app_with_chats(1);
    app.show_error("boom");
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(app.error_popup.is_none(), "dismissed");
    assert!(!app.should_quit, "dismiss keeps the app alive");
    assert!(app.reconnect_requested.is_none());
}

#[test]
fn popup_captures_navigation_and_typing() {
    let mut app = app_with_chats(2);
    app.show_error("boom");

    // Any key other than R/Esc/q/Ctrl+C dismisses without side effects:
    // no chat switch happens even though Down normally navigates.
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active, 0, "chat navigation blocked by popup");
    assert!(app.error_popup.is_none(), "Down dismissed the popup");
    assert!(!app.should_quit);

    // Ctrl+↑/↓ (thread switching in Normal mode)
    // are just another dismiss key under the popup — no thread change.
    app.show_error("boom again");
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(app.active, 0, "Ctrl+Down must not switch chats via popup");
    assert!(app.error_popup.is_none(), "Ctrl+Down dismissed the popup");
    app.show_error("boom thrice");
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(app.active, 0, "Ctrl+Up must not switch chats via popup");

    // the Alt synonym is swallowed identically.
    app.show_error("boom quater");
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.active, 0, "Alt+Down must not switch chats via popup");
    assert!(app.error_popup.is_none(), "Alt+Down dismissed the popup");
    app.show_error("boom quinquies");
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(app.active, 0, "Alt+Up must not switch chats via popup");
}
