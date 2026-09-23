use super::support::*;
use crate::app::*;
use crate::app::{ChatGeometry, ChatRowMeta, SelectionPoint};

fn row_meta(logical: usize, start: usize, end: usize, text: &str) -> ChatRowMeta {
    ChatRowMeta {
        logical,
        start,
        end,
        text: text.to_owned(),
    }
}

fn geometry_with_rows(chat_id: &str, rows: Vec<ChatRowMeta>) -> ChatGeometry {
    ChatGeometry {
        chat_id: chat_id.to_owned(),
        inner: Rect::new(26, 1, 94, 10),
        skip: 0,
        rows,
    }
}

fn point(row: usize, col: usize) -> SelectionPoint {
    SelectionPoint { row, col }
}

/// THE plain-text extraction criterion: chars of the wrapped rows
/// between the ordered endpoints, concatenated ACROSS wrap points of
/// one logical line and newline-SEPARATED at real line breaks.
#[test]
fn selection_text_joins_wraps_and_breaks_at_logical_lines() {
    let mut app = app_with_chats(1);
    // Logical line 0 ("alpha beta gamma delta") wrapped at width 10
    // into three rows; logical line 1 is a separate line.
    app.chat_geometry = Some(geometry_with_rows(
        &app.chats[0].id,
        vec![
            row_meta(0, 0, 10, "alpha beta"),
            row_meta(0, 11, 16, "gamma"),
            row_meta(0, 17, 22, "delta"),
            row_meta(1, 0, 6, "second"),
        ],
    ));

    // Wrap-spanning drag within ONE logical line: rows concatenate
    // without newlines, reverse drag normalized.
    app.selection = Some(crate::app::ChatSelection {
        anchor: point(2, 5),
        head: point(0, 6),
        dragging: false,
    });
    assert_eq!(
        app.selection_text().as_deref(),
        Some("beta gamma delta"),
        "wrap points join without newlines"
    );

    // A drag crossing into the NEXT logical line: the real line break
    // becomes a newline (mid-word cut included); the wrap gap between
    // "gamma" and "delta" restores one space.
    app.selection = Some(crate::app::ChatSelection {
        anchor: point(1, 0),
        head: point(3, 4),
        dragging: false,
    });
    assert_eq!(app.selection_text().as_deref(), Some("gamma delta\nseco"));

    // Same logical line only: no newline even across rows.
    app.selection = Some(crate::app::ChatSelection {
        anchor: point(1, 0),
        head: point(2, 5),
        dragging: false,
    });
    assert_eq!(app.selection_text().as_deref(), Some("gamma delta"));
}

/// The extraction is guarded against a geometry from a foreign thread
/// (the one-iteration staleness window after a thread switch).
#[test]
fn selection_text_rejects_a_foreign_chats_geometry() {
    let mut app = app_with_chats(1);
    app.chat_geometry = Some(geometry_with_rows(
        "some-other-thread",
        vec![row_meta(0, 0, 5, "hello")],
    ));
    app.selection = Some(crate::app::ChatSelection {
        anchor: point(0, 0),
        head: point(0, 5),
        dragging: false,
    });
    assert_eq!(app.selection_text(), None, "stale geometry is rejected");
}

/// Lifecycle: press starts a live drag, drag moves the head only while
/// live, release with a real selection keeps it and extracts the text,
/// a plain click (release without drag) clears instead.
#[test]
fn selection_lifecycle_press_drag_release_and_plain_click() {
    let mut app = app_with_chats(1);
    app.chat_geometry = Some(geometry_with_rows(
        &app.chats[0].id,
        vec![row_meta(0, 0, 11, "hello world")],
    ));

    app.begin_selection(point(0, 0));
    let sel = app.selection.expect("press starts the selection");
    assert!(sel.dragging);

    app.drag_selection(point(0, 5));
    app.drag_selection(point(0, 11));
    assert_eq!(
        app.selection.expect("still live").head,
        point(0, 11),
        "drag moves the head"
    );

    let text = app.release_selection().expect("real selection copies");
    assert_eq!(text, "hello world");
    let sel = app.selection.expect("held after release");
    assert!(!sel.dragging, "released");

    // Drag AFTER the release must not resurrect/move the selection.
    app.drag_selection(point(0, 2));
    assert_eq!(app.selection.expect("held").head, point(0, 11));

    // Plain click: press + release at the same point clears, no copy.
    app.begin_selection(point(0, 3));
    assert_eq!(app.release_selection(), None, "plain click copies nothing");
    assert!(app.selection.is_none(), "plain click clears");
}

/// A selection is cleared by a thread switch (the single
/// [`App::select_chat`] seam behind every switching path) and by the
/// explicit clear (Esc), and it never lands in the persisted snapshot:
/// it lives on [`App`], not on [`Chat`].
#[test]
fn selection_clears_on_thread_switch_and_is_never_chat_state() {
    let mut app = app_with_chats(2);
    app.begin_selection(point(0, 0));
    app.drag_selection(point(0, 4));
    assert!(app.selection.is_some());

    app.clear_selection();
    assert!(app.selection.is_none(), "explicit clear (Esc)");

    app.begin_selection(point(0, 0));
    app.select_next();
    assert!(
        app.selection.is_none(),
        "thread switch clears the selection"
    );
    // The chat struct carries no selection: serialization of a message
    // snapshot (the persisted unit) is unaffected by the feature.
    let mut chat = Chat::new("clean");
    chat.messages.push(crate::model::Message::user("hello"));
    let json = serde_json::to_string(&chat.messages).unwrap();
    assert!(
        !json.contains("selection"),
        "nothing selection-shaped persisted"
    );
}

/// [`ChatGeometry::position_at`]: terminal rows map to document rows
/// through the scroll offset (skip), columns to char offsets through
/// display width, the right edge clamps at the row end, and points
/// outside the inner rect are rejected.
#[test]
fn chat_geometry_position_at_maps_rows_columns_and_clamps() {
    let rows: Vec<ChatRowMeta> = (0..5)
        .map(|i| row_meta(i, 0, 4, &format!("r{i}")))
        .chain([
            row_meta(5, 0, 11, "hello world"),
            row_meta(5, 12, 15, "abc"),
        ])
        .collect();
    let geom = ChatGeometry {
        chat_id: "t".into(),
        inner: Rect::new(26, 1, 10, 2),
        skip: 5,
        rows,
    };
    // First visible row = document row 5 (skip); col 0 at the pane edge.
    assert_eq!(geom.position_at(26, 1), Some(point(5, 0)));
    // 6 columns in → 6 chars ("hello ") fully left of the pointer.
    assert_eq!(geom.position_at(32, 1), Some(point(5, 6)));
    // Second visible row = document row 6; past its end clamps at 3.
    assert_eq!(geom.position_at(35, 2), Some(point(6, 3)));
    // Below the pane: rejected (the router clamps drags separately).
    assert_eq!(geom.position_at(30, 3), None);
    // Outside the pane horizontally (divider column) too.
    assert_eq!(geom.position_at(25, 1), None);
    // No rows at all: nothing to hit.
    let empty = ChatGeometry {
        chat_id: "t".into(),
        inner: Rect::new(26, 1, 10, 2),
        skip: 0,
        rows: Vec::new(),
    };
    assert_eq!(empty.position_at(26, 1), None);
}
