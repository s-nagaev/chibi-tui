use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

// ^O toggle ---------------------------------------

/// ^O toggles the status strip, default visible, round-trip.
#[test]
fn ctrl_o_toggles_status_strip_round_trip() {
    let mut app = app_with_chats(1);
    assert!(
        app.status_strip_visible,
        "strip must start visible (task contract)"
    );
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(!app.status_strip_visible);
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(app.status_strip_visible);
}

/// View state like Focus: the strip survives modal open/close, and the
/// modal branch swallows ^O while a popup is open (no toggle leaks).
#[test]
fn ctrl_o_survives_modals_and_is_swallowed_while_one_is_open() {
    let mut app = app_with_chats(1);
    assert!(app.status_strip_visible);

    // Open the search popup: strip stays visible underneath it.
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
    assert!(app.status_strip_visible);

    // ^O inside the popup is swallowed — mode and visibility unchanged.
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
    assert!(
        app.status_strip_visible,
        "swallowed ^O must not toggle the strip"
    );

    // Closing the modal keeps the visibility flag (no reset on close).
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert!(app.status_strip_visible);
}

/// Sidebar-focus parity contract: service chords behave identically
/// under both panes — ^O toggles the strip from the sidebar too.
#[test]
fn ctrl_o_toggles_strip_under_sidebar_focus() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(!app.status_strip_visible);
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(app.status_strip_visible);
}

// ^S toggle -----------------------------------------

/// ^S toggles the thoughts block, default ON, round-trip; the toggle is
/// render-only and never clears the retained reasoning.
#[test]
fn ctrl_s_toggles_thoughts_round_trip() {
    let mut app = app_with_chats(1);
    assert!(app.thoughts_visible, "thoughts must start visible (ON)");
    app.chats[0].last_thoughts = Some("chain of thought".into());
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(!app.thoughts_visible);
    assert_eq!(
        app.chats[0].last_thoughts.as_deref(),
        Some("chain of thought"),
        "toggle must not clear the retained thoughts"
    );
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(app.thoughts_visible);
}

/// Modal branches swallow ^S like every other chord (no toggle leaks
/// while a popup is open), and closing keeps the flag.
#[test]
fn ctrl_s_survives_modals_and_is_swallowed_while_one_is_open() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(!app.thoughts_visible);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(
        !app.thoughts_visible,
        "swallowed ^S must not toggle while a modal is open"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert!(!app.thoughts_visible, "modal close must keep the flag");
}

/// Sidebar-focus parity contract: service chords behave identically
/// under both panes — ^S toggles thoughts from the sidebar too.
#[test]
fn ctrl_s_toggles_thoughts_under_sidebar_focus() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(!app.thoughts_visible);
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(app.thoughts_visible);
}
