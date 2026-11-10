//! Word-boundary wrap seam (`app::wrap_line_ranges` / `wrap_line`): the
//! shared rule behind the editor's soft wrap, the log viewer's reflow and
//! the sidebar's chat-name wrapping.

use crate::app::wrap_line_ranges;

/// Ranges of `wrap_line_ranges` mapped back to their text, so the assertions
/// read as the wrapped lines a caller would render.
fn rows(line: &str, width: usize, reserve: usize) -> Vec<String> {
    wrap_line_ranges(line, width, reserve)
        .into_iter()
        .map(|(start, end)| line.chars().skip(start).take(end - start).collect())
        .collect()
}

#[test]
fn wrap_ranges_fit_a_line_within_the_width() {
    assert_eq!(wrap_line_ranges("abc", 10, 0), vec![(0, 3)]);
    assert_eq!(wrap_line_ranges("abc", 3, 0), vec![(0, 3)]);
    assert_eq!(rows("hello world", 11, 0), vec!["hello world"]);
}

#[test]
fn wrap_ranges_break_at_the_last_space_that_fits() {
    // Width 9: `hello wor` fits but `hello word` does not, so the row ends
    // at the LAST space that fits (index 5) and the break space is dropped.
    assert_eq!(wrap_line_ranges("hello world", 9, 0), vec![(0, 5), (6, 11)]);
    assert_eq!(rows("hello world", 9, 0), vec!["hello", "world"]);
}

#[test]
fn wrap_ranges_hard_break_a_token_wider_than_the_row() {
    // No space anywhere: the token is cut at the width, always making
    // progress (never an empty row, never a hang).
    assert_eq!(
        wrap_line_ranges("abcdefgh", 3, 0),
        vec![(0, 3), (3, 6), (6, 8)]
    );
    // A token longer than the row after a break also hard-breaks, and the
    // dropped space leaves a gap the second row starts after.
    assert_eq!(
        wrap_line_ranges("abcd efghij", 4, 0),
        vec![(0, 4), (5, 9), (9, 11)]
    );
}

#[test]
fn wrap_ranges_drop_only_the_break_space_of_a_space_run() {
    // Multiple spaces: the row breaks at the LAST space of the run that
    // fits, so the earlier spaces of the run stay inside the first row and
    // only the break space itself is dropped.
    assert_eq!(
        wrap_line_ranges("ab   cdef", 5, 0),
        vec![(0, 4), (5, 9)],
        "row 1 keeps `ab  `, the run's tail space is the dropped break"
    );
    // A single space run narrower than the width is kept verbatim.
    assert_eq!(wrap_line_ranges("ab cd", 5, 0), vec![(0, 5)]);
}

#[test]
fn wrap_ranges_reserve_columns_on_the_first_row_only() {
    // Capacity 4 on row 1 (8 withheld from 12), 12 on every row after.
    assert_eq!(
        wrap_line_ranges("aaa bbbbbbbbbbbbbbb", 12, 8),
        vec![(0, 3), (4, 16), (16, 19)]
    );
}

#[test]
fn wrap_ranges_empty_line_is_one_empty_row() {
    assert_eq!(wrap_line_ranges("", 5, 0), vec![(0, 0)]);
    assert_eq!(wrap_line_ranges("", 5, 8), vec![(0, 0)]);
}

#[test]
fn wrap_ranges_cjk_counts_display_width() {
    // Four 2-column glyphs at width 4: two per row.
    assert_eq!(
        wrap_line_ranges("\u{4f60}\u{597d}\u{4e16}\u{754c}", 4, 0),
        vec![(0, 2), (2, 4)]
    );
    // Degenerate width 0 clamps to 1 and still advances (one glyph per row
    // is impossible for a 2-column glyph, so each row holds one glyph).
    assert_eq!(
        wrap_line_ranges("\u{4f60}\u{597d}", 0, 0),
        vec![(0, 1), (1, 2)]
    );
}

#[test]
fn wrap_ranges_are_stable_for_identical_inputs() {
    let line = "the quick brown fox jumps over the lazy dog";
    let first = wrap_line_ranges(line, 12, 8);
    assert_eq!(first, wrap_line_ranges(line, 12, 8), "pure function");
    // Every row is non-empty and forward-moving (no row can loop forever).
    assert!(first.iter().all(|&(start, end)| start < end));
    assert!(first.windows(2).all(|w| w[0].1 <= w[1].0));
}
