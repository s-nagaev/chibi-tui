use crate::tests::support::*;

use crate::*;

use crossterm::event::{KeyCode, KeyModifiers};

// ---- should_submit -------------------------------------------------------

#[test]
fn should_submit_enter_returns_true() {
    let enter = key_event(KeyCode::Enter, KeyModifiers::NONE);
    assert!(should_submit(&enter));
}

#[test]
fn should_submit_letter_returns_false() {
    for ch in ['a', 'q', 'j', 'k', 'n', 'x'] {
        let key = key_event(KeyCode::Char(ch), KeyModifiers::NONE);
        assert!(!should_submit(&key), "should_submit({ch:?}) must be false");
    }
}

#[test]
fn should_submit_ctrl_c_returns_false() {
    let ctrl_c = key_event(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!should_submit(&ctrl_c));
}

#[test]
fn should_submit_ctrl_n_returns_false() {
    let ctrl_n = key_event(KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert!(!should_submit(&ctrl_n));
}

#[test]
fn should_submit_arrow_keys_return_false() {
    for code in [KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right] {
        let key = key_event(code, KeyModifiers::NONE);
        assert!(
            !should_submit(&key),
            "should_submit({code:?}) must be false"
        );
    }
}

/// only a BARE Enter submits. Shift+Enter and
/// Alt+Enter must route into the textarea as newline inserts instead.
#[test]
fn should_submit_shift_and_alt_enter_return_false() {
    for mods in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
        let key = key_event(KeyCode::Enter, mods);
        assert!(
            !should_submit(&key),
            "should_submit(Enter+{mods:?}) must be false — it inserts a newline"
        );
    }
}

/// Enter+SHIFT / Enter+ALT reach the textarea's
/// newline insertion through the full `handle_key` path (bare Enter stays
/// swallowed — submission belongs to the event loop).
#[test]
fn shift_and_alt_enter_insert_newline_into_input() {
    for mods in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
        let mut app = app_with_chats(1);
        type_in(&mut app, "line one");
        press(&mut app, KeyCode::Enter, mods);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, mods);
        type_in(&mut app, "two");

        // "line one" ⏎ "l" ⏎ "two" — three buffer lines.
        assert_eq!(app.input.lines(), ["line one", "l", "two"]);
    }
}

/// Regression guard: bare Enter is still swallowed by `handle_key` (no
/// newline inserted) and remains the ONLY submitting variant.
#[test]
fn bare_enter_is_swallowed_by_handle_key() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "draft");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.input.lines(), ["draft"], "bare Enter must not insert");

    // And via should_submit it would submit (loop-level gate):
    assert!(should_submit(&key_event(
        KeyCode::Enter,
        KeyModifiers::NONE
    )));
}

/// Rename-mode interplay with Shift+Enter (chosen approach,
/// documented in README): while renaming, Shift+Enter / Alt+Enter insert
/// a literal newline into the rename draft; BARE Enter still saves.
/// The loop's `enter_consumed_by_rename` gate keeps every Enter away
/// from message submission either way.
#[test]
fn rename_mode_shift_enter_inserts_newline_bare_enter_saves() {
    let mut app = app_with_chats(1);
    app.begin_rename();
    assert!(matches!(app.mode, Mode::Renaming { .. }));

    // begin_rename() prefills the draft with the current title.
    type_in(&mut app, "multi");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_in(&mut app, "line");

    match &app.mode {
        Mode::Renaming { buf } => {
            assert_eq!(buf, "chat-0multi\nline", "Shift+Enter inserts \\n")
        }
        other => panic!("rename mode dropped by Shift+Enter: {other:?}"),
    }

    // Bare Enter saves; the trimmed multi-line title persists.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert_eq!(app.chat_title(), "chat-0multi\nline");
}

/// A rejected save (whitespace-only title) keeps the old name even when
/// the draft contained newlines — trimming still wins over `\n`s.
#[test]
fn rename_mode_whitespace_only_multiline_draft_rejected() {
    let mut app = app_with_chats(1);
    app.begin_rename();
    press(&mut app, KeyCode::Enter, KeyModifiers::ALT); // "\n"
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.mode.is_normal(), "save attempted");
    assert_eq!(app.chat_title(), "chat-0", "empty multiline draft rejected");
}
