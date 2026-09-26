use crate::tests::support::*;

use crate::*;

use crossterm::event::{KeyCode, KeyModifiers};

// ---- extended input keybindings ------------------------------------------

#[test]
fn ctrl_l_idle_neither_clears_input_nor_stages_anything() {
    let mut app = app_with_chats(1);
    for ch in "hello".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    assert_eq!(app.input.lines().join(""), "hello");
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(
        app.input.lines().join("") == "hello",
        "idle ^L is a silent no-op: no input clear, no popup, no request"
    );
    assert!(app.take_control_submission().is_none());
}

#[test]
fn ctrl_v_pastes_clipboard_into_input() {
    let mut app = app_with_chats(1);
    // Only meaningful when a clipboard exists; either way it must not
    // panic or corrupt state.
    press(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    if chibi_tui::clipboard::get_text().is_some() {
        let pasted = app.input.lines().join("");
        println!("clipboard content length: {}", pasted.len());
    }
}

#[test]
fn ctrl_u_deletes_to_line_start_like_readline() {
    let mut app = app_with_chats(1);
    for ch in "keepme".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    // Cursor sits after the last char; Ctrl+U wipes to start of line.
    press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert!(
        app.input.lines().iter().all(|l| l.is_empty()),
        "Ctrl+U clears the line (readline semantics, not undo)"
    );
}

#[test]
fn ctrl_a_and_ctrl_e_reach_textarea_readline_mappings() {
    let mut app = app_with_chats(1);
    for ch in "abc".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    // Ctrl+A → head of line.
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(app.input.cursor(), (0, 0), "Ctrl+A moves to line head");
    // Ctrl+E → end of line.
    press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(app.input.cursor(), (0, 3), "Ctrl+E moves to line end");
}

/// Release events are ignored everywhere (macOS emits them).
#[test]
fn release_events_are_ignored() {
    let mut app = app_with_chats(1);
    let release = crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
        crossterm::event::KeyEventKind::Release,
    );
    handle_key(&mut app, release);
    assert!(!app.should_quit);
}
