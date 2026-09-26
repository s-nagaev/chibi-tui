use super::support::*;
use crate::app::*;

#[test]
fn clear_input_empties_buffer_and_restores_placeholder() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "to be cleared");
    app.clear_input();
    assert!(
        app.input.lines().iter().all(|l| l.is_empty()),
        "buffer must be empty after Ctrl+L"
    );
    assert_eq!(
        app.input.placeholder_text(),
        Some("Type a message…  (\u{23ce} send)")
    );
}

#[test]
fn clear_input_on_empty_buffer_is_a_no_op() {
    let mut app = app_with_chats(1);
    app.clear_input();
    assert!(app.input.lines().iter().all(|l| l.is_empty()));
}

#[test]
fn input_lines_height_collapses_to_one_when_empty_or_cleared() {
    let mut app = app_with_chats(1);
    assert_eq!(app.input_lines_height(), 1, "empty input ⇒ single row");

    app.input.insert_str("one\ntwo\nthree");
    assert_eq!(app.input_lines_height(), 3);

    app.clear_input();
    assert_eq!(
        app.input_lines_height(),
        1,
        "cleared input must collapse back to exactly one row"
    );
}

#[test]
fn input_lines_height_caps_at_max() {
    let mut app = app_with_chats(1);
    for _ in 0..19 {
        app.input.insert_str("\n");
    }
    assert_eq!(
        app.input.lines().len(),
        20,
        "boundary: exactly MAX lines stays uncapped"
    );
    assert_eq!(app.input_lines_height(), crate::app::MAX_INPUT_LINES as u16);

    for _ in 0..40 {
        app.input.insert_str("\n");
    }
    assert_eq!(
        app.input_lines_height(),
        20,
        ">MAX buffer lines clamp to cap"
    );
}

#[test]
fn input_lines_height_counts_rename_draft_lines_too() {
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);
    assert_eq!(app.input_lines_height(), 1);

    // Renaming inserts `\n`s into its own draft via modified Enters /
    // paste (routing covered by main.rs tests); here the helper must
    // count THAT active buffer, not the hidden message draft.
    app.mode = Mode::Renaming {
        buf: "ab\ncd\nef".to_owned(),
    };
    assert_eq!(app.input_lines_height(), 3);
}

// confirm popup state logic --------------------

#[test]
fn input_lines_height_counts_draft_while_searching() {
    let mut app = app_with_chats(1);
    app.input.insert_str("one\ntwo");
    assert_eq!(app.input_lines_height(), 2);
    app.begin_search();
    assert_eq!(
        app.input_lines_height(),
        2,
        "search overlays the editor; draft height unchanged"
    );
    app.search_push('n');
    assert_eq!(app.input_lines_height(), 2, "query lives in the popup");
}
