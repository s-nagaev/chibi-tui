use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

//------------------------------------------------------------------------

pub(super) fn busy_app_with_commands(commands: &[&str]) -> chibi_tui::app::App {
    let mut app = app_with_chats(1);
    app.set_backend_commands(commands.iter().map(|c| (*c).to_owned()).collect());
    submit_text(&mut app, "in flight");
    app
}

#[test]
fn ctrl_l_busy_opens_stop_confirm_and_idle_is_noop() {
    // Busy chat: ^L opens the stop confirmation.
    let mut app = busy_app_with_commands(&["/stop", "/reset"]);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Stop
        }
    ));

    // Idle chat: silent no-op (retired wipe semantics stay retired).
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/stop".to_owned()]);
    type_in(&mut app, "draft survives");
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(app.mode.is_normal(), "idle ^L must not open the popup");
    assert_eq!(app.input.lines().join(""), "draft survives");
    assert!(app.take_control_submission().is_none());
}

#[test]
fn ctrl_l_without_backend_support_shows_toast() {
    let mut app = busy_app_with_commands(&["/reset"]);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(
        app.mode.is_normal(),
        "no popup without the advertised command"
    );
    assert!(app.status_message.is_some(), "transient toast explains");
}

#[test]
fn shift_ctrl_l_opens_reset_confirm_in_both_kitty_variants() {
    // Kitty protocol: shifted chord arrives as Char('L') + CONTROL.
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);
    press(
        &mut app,
        KeyCode::Char('L'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Reset
        }
    ));

    // Variant where the unshifted char rides with the SHIFT flag.
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);
    press(
        &mut app,
        KeyCode::Char('l'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Reset
        }
    ));

    // Plain ^L stays STOP even while the reset capability exists.
    let mut app = busy_app_with_commands(&["/stop", "/reset"]);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Stop
        }
    ));
}

#[test]
fn stop_confirm_keys_follow_the_delete_confirm_grammar() {
    let mut app = busy_app_with_commands(&["/stop", "/reset"]);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);

    // Esc cancels; 'n' cancels; Enter confirms; 'y' confirms.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert!(app.take_control_submission().is_none());

    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(app.mode.is_normal());

    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    // Deliberately n/y-free: plain n/y ARE the popup's decision keys.
    type_in(&mut app, "swallowed draft");
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
        "plain typing must not close the popup"
    );
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    let staged = app
        .take_control_submission()
        .expect("stop staged on confirm");
    assert_eq!(staged.prompt, "/stop");
    assert_eq!(staged.thread_id, app.chats[app.active].id);
    // The chat lifecycle stays on the RUNNING request, not the control.
    assert_ne!(
        app.chats[app.active].lifecycle.request_id(),
        Some(staged.request_id.as_str()),
        "the killed request keeps the chat's lifecycle until its own cancel resolves"
    );
}

#[test]
fn reset_confirm_stages_reset_command_and_cancels_cleanly() {
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);
    press(
        &mut app,
        KeyCode::Char('L'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    let staged = app
        .take_control_submission()
        .expect("reset staged on confirm");
    assert_eq!(staged.prompt, "/reset");
    assert_eq!(staged.thread_id, app.chats[app.active].id);
    assert!(app.mode.is_normal());
}

#[test]
fn same_chord_cancels_both_stop_and_reset_popups() {
    // Stop popup opened by ^L: the same chord closes it, nothing staged.
    let mut app = busy_app_with_commands(&["/stop", "/reset"]);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Stop
        }
    ));
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(app.mode.is_normal(), "same chord must cancel the popup");
    assert!(
        app.take_control_submission().is_none(),
        "cancelled stop must not stage /stop"
    );

    // Reset popup opened by Shift+Ctrl+L (kitty variant): same chord cancels.
    let mut app = busy_app_with_commands(&["/stop", "/reset"]);
    press(
        &mut app,
        KeyCode::Char('L'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Reset
        }
    ));
    press(
        &mut app,
        KeyCode::Char('L'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(app.mode.is_normal(), "same chord must cancel the popup");
    assert!(
        app.take_control_submission().is_none(),
        "cancelled reset must not stage /reset"
    );

    // Variant where the unshifted char rides with SHIFT: same-chord cancel too.
    let mut app = busy_app_with_commands(&["/stop", "/reset"]);
    press(
        &mut app,
        KeyCode::Char('l'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Reset
        }
    ));
    press(
        &mut app,
        KeyCode::Char('l'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(app.mode.is_normal());
    assert!(app.take_control_submission().is_none());
}

#[test]
fn stop_and_reset_are_gated_on_the_handshake_command_list() {
    // Mocks and offline sessions never advertise the commands.
    let mut app = app_with_chats(1);
    press(
        &mut app,
        KeyCode::Char('L'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(app.mode.is_normal(), "no popup on an older backend");
    assert!(app.take_control_submission().is_none());
}
