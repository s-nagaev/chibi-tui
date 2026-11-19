use super::support::*;
use crate::app::*;
use ratatui::layout::Rect;

/// A standard 120×34 frame: text width `120-30-1-2 = 87` columns and a
/// growth cap of `min(34-4, 34*3/4) = 25` rows. Wide enough that the short
/// draft lines below each occupy exactly one visual row.
const FRAME: Rect = Rect {
    x: 0,
    y: 0,
    width: 120,
    height: 34,
};

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
    assert_eq!(app.input_lines_height(FRAME), 1, "empty input ⇒ single row");

    app.input.insert_str("one\ntwo\nthree");
    assert_eq!(app.input_lines_height(FRAME), 3);

    app.clear_input();
    assert_eq!(
        app.input_lines_height(FRAME),
        1,
        "cleared input must collapse back to exactly one row"
    );
}

#[test]
fn input_lines_height_caps_at_three_quarters_of_the_frame() {
    let mut app = app_with_chats(1);
    // A 40-line draft at the 34-row frame: cap = min(34-4, 34*3/4) = 25.
    let lines: Vec<String> = (0..40).map(|i| format!("line-{i:02}")).collect();
    app.input.insert_str(&lines.join("\n"));
    assert_eq!(app.input.lines().len(), 40);
    assert_eq!(
        app.input_lines_height(FRAME),
        25,
        "40 buffer lines clamp to 3/4 of a 34-row frame"
    );

    // Exactly at the cap stays uncapped.
    let mut at_cap = app_with_chats(1);
    let lines: Vec<String> = (0..25).map(|i| format!("line-{i:02}")).collect();
    at_cap.input.insert_str(&lines.join("\n"));
    assert_eq!(at_cap.input_lines_height(FRAME), 25);
}

#[test]
fn input_lines_height_cap_uses_the_tiny_frame_guard_below_sixteen_rows() {
    let mut app = app_with_chats(1);
    for _ in 0..30 {
        app.input.insert_str("x\n");
    }
    // h = 16 ⇒ 3/4 = 12 = 16-4, tie on the guard.
    let frame_16 = Rect::new(0, 0, 120, 16);
    assert_eq!(app.input_lines_height(frame_16), 12);
    // h = 12 ⇒ min(8, 9) = 8: the tiny-frame guard wins over 3/4.
    let frame_12 = Rect::new(0, 0, 120, 12);
    assert_eq!(app.input_lines_height(frame_12), 8);
    // Never below one row, even on an absurd frame.
    let frame_1 = Rect::new(0, 0, 120, 1);
    assert_eq!(app.input_lines_height(frame_1), 1);
}

#[test]
fn input_lines_height_counts_soft_wrapped_rows_not_logical_lines() {
    let mut app = app_with_chats(1);
    // A single logical line far longer than the text width (91 cols at the
    // 120-wide frame): 200 chars wraps to ceil(200/91) = 3 visual rows.
    app.input.insert_str(&"x".repeat(200));
    assert_eq!(app.input.lines().len(), 1, "one logical line");
    assert_eq!(
        app.input_lines_height(FRAME),
        3,
        "height counts wrapped visual rows"
    );
}

#[test]
fn input_lines_height_counts_rename_draft_lines_too() {
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);
    assert_eq!(app.input_lines_height(FRAME), 1);

    // Renaming inserts `\n`s into its own draft via modified Enters /
    // paste (routing covered by main.rs tests); here the helper must
    // count THAT active buffer, not the hidden message draft. The rename
    // renderer draws one logical line per row (it never wraps).
    app.mode = Mode::Renaming {
        buf: "ab\ncd\nef".to_owned(),
    };
    assert_eq!(app.input_lines_height(FRAME), 3);
}

// confirm popup state logic --------------------

#[test]
fn input_lines_height_counts_draft_while_searching() {
    let mut app = app_with_chats(1);
    app.input.insert_str("one\ntwo");
    assert_eq!(app.input_lines_height(FRAME), 2);
    app.begin_search();
    assert_eq!(
        app.input_lines_height(FRAME),
        2,
        "search overlays the editor; draft height unchanged"
    );
    app.search_push('n');
    assert_eq!(app.input_lines_height(FRAME), 2, "query lives in the popup");
}
