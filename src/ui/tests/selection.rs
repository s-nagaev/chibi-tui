use super::super::*;
use super::support::*;

// ---- mouse text selection: wrap meta + highlight -----------------------

/// The full wrap exposes per-row hit-test metadata: the owning logical
/// line, the char range within it (break spaces dropped by the wrap
/// leave gaps) and the plain text — and the row/first-row outputs stay
/// identical to the indexed wrapper's.
#[test]
fn wrap_message_rows_full_records_logical_and_char_ranges() {
    let lines = vec![plain_line("alpha beta gamma delta"), plain_line("second")];
    let (rows, first_row_of, meta) = wrap_message_rows_full(&lines, 10);
    assert_eq!(rows.len(), meta.len(), "one meta entry per display row");
    assert_eq!(first_row_of, vec![0, 3], "first-row map unchanged");

    // "alpha beta" | "gamma" | "delta": the wrap drops the break
    // spaces, so the char ranges gap (0..10, 11..16, 17..22).
    assert_eq!(meta[0].logical, 0);
    assert_eq!((meta[0].start, meta[0].end), (0, 10));
    assert_eq!(meta[0].text, "alpha beta");
    assert_eq!(meta[1].logical, 0);
    assert_eq!((meta[1].start, meta[1].end), (11, 16));
    assert_eq!(meta[1].text, "gamma");
    assert_eq!(meta[2].text, "delta");
    // The next LOGICAL line starts a fresh range.
    assert_eq!(meta[3].logical, 1);
    assert_eq!((meta[3].start, meta[3].end), (0, 6));
    assert_eq!(meta[3].text, "second");
}

/// THE render acceptance criterion: exactly the selected chars take a
/// REVERSED overlay on top of their own style — unselected text keeps
/// its style untouched, and rows outside the selection are
/// byte-identical. Rendered through the full `TestBackend` path with
/// the selection anchored in display-row space.
#[test]
fn selection_highlights_selected_chars_only_in_render() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("sel")]);
    app.chats[0]
        .messages
        .push(Message::assistant("hello world"));
    // First render populates the geometry seam (the same one the
    // mouse router hit-tests through).
    let _ = render_grid_with_buffer(&mut app);
    let geom = app.chat_geometry.clone().expect("geometry cached");
    let r = geom
        .rows
        .iter()
        .position(|m| m.text.contains("hello world"))
        .expect("message row in the geometry");
    // The row's y on the grid: inner top + (row - skip).
    let y = geom.inner.y + (r - geom.skip) as u16;

    app.selection = Some(crate::app::ChatSelection {
        anchor: crate::app::SelectionPoint { row: r, col: 0 },
        head: crate::app::SelectionPoint { row: r, col: 5 },
        dragging: false,
    });
    let (rows, buf) = render_grid_with_buffer(&mut app);
    let row_text = &rows[y as usize];
    let x0 = col_of_sub(row_text, "hello").expect("message rendered");

    // "hello" (5 chars) reversed; the space and "world" not.
    for dx in 0..5usize {
        let cell = &buf[(x0 as u16 + dx as u16, y)];
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "selected char {dx} must be reversed"
        );
    }
    for dx in 5..11usize {
        let cell = &buf[(x0 as u16 + dx as u16, y)];
        assert!(
            !cell.modifier.contains(Modifier::REVERSED),
            "unselected char {dx} must not be reversed"
        );
    }
    // Styling preserved: reversed is an overlay, the fg slot of a
    // selected char equals its unselected neighbor's slot family
    // (plain markdown text renders in the theme fg either way).
    assert_eq!(
        buf[(x0 as u16, y)].fg,
        buf[(x0 as u16 + 6, y)].fg,
        "selected and unselected text share the markdown fg slot"
    );
    assert_eq!(buf[(x0 as u16, y)].fg, theme.fg);

    // Clearing the selection restores the plain render.
    app.clear_selection();
    let (rows_plain, buf_plain) = render_grid_with_buffer(&mut app);
    assert_eq!(rows_plain[y as usize], *row_text, "text identical");
    assert!(
        !buf_plain[(x0 as u16, y)]
            .modifier
            .contains(Modifier::REVERSED),
        "no highlight after the clear"
    );
}

/// A selection spanning several wrapped rows highlights its slice on
/// every affected row: full rows entirely, edge rows partially.
#[test]
fn selection_highlight_spans_wrapped_rows() {
    let mut app = App::new(vec![Chat::new("wrap")]);
    // Narrow pane: force the paragraph onto multiple display rows.
    app.chats[0]
        .messages
        .push(Message::assistant("alpha beta gamma delta epsilon"));
    let _ = render_grid_at_with_buffer(&mut app, 50, 24);
    let geom = app.chat_geometry.clone().expect("geometry cached");
    let content: Vec<usize> = geom
        .rows
        .iter()
        .enumerate()
        .filter(|(_, m)| m.text.contains("alpha") || m.text.contains("epsilon"))
        .map(|(i, _)| i)
        .collect();
    assert!(
        content.len() >= 2,
        "precondition: the message wraps over several rows"
    );
    let first = content[0];
    let second = content[1];

    app.selection = Some(crate::app::ChatSelection {
        anchor: crate::app::SelectionPoint { row: first, col: 6 },
        head: crate::app::SelectionPoint {
            row: second,
            col: 5,
        },
        dragging: false,
    });
    let (_, buf) = render_grid_at_with_buffer(&mut app, 50, 24);
    let y_first = geom.inner.y + (first - geom.skip) as u16;
    let y_second = geom.inner.y + (second - geom.skip) as u16;

    // First row: from char 6 to the row end is reversed.
    let reversed_first: Vec<bool> = (0..geom.rows[first].text.chars().count())
        .map(|dx| {
            buf[((geom.inner.x as usize + dx) as u16, y_first)]
                .modifier
                .contains(Modifier::REVERSED)
        })
        .collect();
    assert!(!reversed_first[5], "chars before the anchor stay plain");
    assert!(
        reversed_first[6..].iter().all(|&rev| rev),
        "the anchor-to-end slice is reversed: {reversed_first:?}"
    );
    // Second row: chars 0..5 reversed (its slice of the selection).
    for dx in 0..5usize {
        assert!(
            buf[((geom.inner.x as usize + dx) as u16, y_second)]
                .modifier
                .contains(Modifier::REVERSED),
            "second-row slice char {dx} reversed"
        );
    }
}
