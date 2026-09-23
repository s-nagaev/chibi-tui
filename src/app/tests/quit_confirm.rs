use crate::app::*;

// ---- quit confirmation -------------------------------

/// The shared quit-confirm grammar (in-app popup, splash, setup screen
/// all route through [`quit_decision`]): y/Enter/Ctrl+C confirm,
/// Esc/n/q dismiss, everything else is swallowed.
#[test]
fn quit_decision_grammar_pins_the_shared_keymap() {
    use crossterm::event::KeyCode as K;
    let e = |code, m| KeyEvent::new(code, m);
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;

    for (code, mods, want) in [
        (K::Char('y'), none, QuitDecision::Confirm),
        (K::Char('Y'), none, QuitDecision::Confirm),
        (K::Enter, none, QuitDecision::Confirm),
        (K::Char('c'), ctrl, QuitDecision::Confirm),
        (K::Esc, none, QuitDecision::Dismiss),
        (K::Char('n'), none, QuitDecision::Dismiss),
        (K::Char('N'), none, QuitDecision::Dismiss),
        (K::Char('q'), none, QuitDecision::Dismiss),
        (K::Char('x'), none, QuitDecision::Swallow),
        (K::Char('y'), ctrl, QuitDecision::Swallow),
        (K::Up, none, QuitDecision::Swallow),
        (K::Backspace, none, QuitDecision::Swallow),
    ] {
        assert_eq!(quit_decision(e(code, mods)), want, "{code:?} {mods:?}");
    }
}

/// begin/cancel/confirm lifecycle: opening is cancel-safe (touches
/// nothing else), confirm arms `should_quit` — the exact transition
/// the direct quit paths used to perform.
#[test]
fn quit_confirm_lifecycle_begin_cancel_confirm() {
    let mut app = App::new(Vec::new());
    app.input.insert_str("draft");
    assert!(!app.quit_confirm);

    app.begin_quit_confirm();
    assert!(app.quit_confirm);
    assert!(!app.should_quit, "opening must not quit");
    assert_eq!(app.input.lines().join(""), "draft", "cancel-safe");

    assert!(app.cancel_quit_confirm(), "dismiss reports it was open");
    assert!(!app.quit_confirm);
    assert!(!app.should_quit);
    assert!(!app.cancel_quit_confirm(), "second dismiss is a no-op");

    app.begin_quit_confirm();
    app.confirm_quit();
    assert!(app.should_quit, "confirm arms the shutdown path");
    assert!(!app.quit_confirm, "popup closed by the confirm");
}
