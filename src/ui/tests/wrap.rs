use super::super::*;
use super::support::*;

/// greedy wrap keeps words intact and never
/// lets a row exceed the width budget.
#[test]
fn wrap_line_rows_wraps_words_without_exceeding_width() {
    let rows = wrap_line_rows(&plain_line("alpha beta gamma delta"), 10);
    let texts: Vec<String> = rows
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<String>()
        })
        .collect();
    assert_eq!(texts, vec!["alpha beta", "gamma", "delta"]);
}

/// Indentation and intra-row whitespace runs survive untouched — they are
/// part of the content (code blocks relied on this).
#[test]
fn wrap_line_rows_preserves_indentation_and_inner_spaces() {
    let src = "    return   x";
    let rows = wrap_line_rows(&plain_line(src), 30);
    assert_eq!(rows.len(), 1);
    let got: String = rows[0]
        .spans
        .iter()
        .map(|s| s.content.clone())
        .collect::<String>();
    assert_eq!(got, src);
}

/// Wide-glyph regression: CJK glyphs are 2 columns wide regardless of
/// their 3-byte UTF-8 length; they split cleanly BETWEEN glyphs.
#[test]
fn wrap_line_rows_splits_cjk_on_column_boundaries_not_bytes() {
    // "你好世界" = 12 bytes, 8 display columns.
    let rows = wrap_line_rows(&plain_line("\u{4f60}\u{597d}\u{4e16}\u{754c}"), 4);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].spans[0].content, "\u{4f60}\u{597d}");
    assert_eq!(rows[1].spans[0].content, "\u{4e16}\u{754c}");
}

/// Emoji (2 columns) never straddles the right edge of a row.
#[test]
fn wrap_line_rows_never_straddles_emoji_across_boundary() {
    // Three hourglass U+23F3 glyphs = 6 columns; budget 5.
    let rows = wrap_line_rows(&plain_line("\u{23f3}\u{23f3}\u{23f3}"), 5);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].spans[0].content, "\u{23f3}\u{23f3}");
    assert_eq!(rows[1].spans[0].content, "\u{23f3}");
}

/// An unbreakable token longer than the budget hard-splits across rows.
#[test]
fn wrap_line_rows_hard_splits_overlong_token() {
    let rows = wrap_line_rows(&plain_line(&"A".repeat(25)), 10);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].spans[0].content, "AAAAAAAAAA");
    assert_eq!(rows[1].spans[0].content, "AAAAAAAAAA");
    assert_eq!(rows[2].spans[0].content, "AAAAA");
}

/// Per-span styling survives a break: each output row keeps the style of
/// the characters it carries.
#[test]
fn wrap_line_rows_keeps_span_style_after_break() {
    let line = Line::from(vec![
        Span::styled("RED", Style::new().fg(ratatui::style::Color::Red)),
        Span::raw(" "),
        Span::styled("BLUE", Style::new().fg(ratatui::style::Color::Blue)),
    ]);
    let rows = wrap_line_rows(&line, 5);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].spans[0].content, "RED");
    assert_eq!(rows[0].spans[0].style.fg, Some(ratatui::style::Color::Red));
    assert_eq!(rows[1].spans[0].content, "BLUE");
    assert_eq!(rows[1].spans[0].style.fg, Some(ratatui::style::Color::Blue));
}

/// Empty / whitespace-only logical lines still occupy exactly one row.
#[test]
fn wrap_line_rows_empty_line_yields_one_blank_row() {
    assert_eq!(wrap_line_rows(&plain_line(""), 80).len(), 1);
    assert_eq!(wrap_line_rows(&Line::from(Vec::<Span>::new()), 80).len(), 1);
    let rows = wrap_line_rows(&plain_line("   "), 2);
    assert_eq!(rows.len(), 1);
}

#[test]
fn wrap_text_respects_width_and_keeps_words() {
    let lines = wrap_text("aaa bbb ccc ddd", 7);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].spans[0].content, "aaa bbb");
    assert_eq!(lines[1].spans[0].content, "ccc ddd");
}

#[test]
fn wrap_text_hard_splits_overlong_words() {
    let lines = wrap_text(&"x".repeat(30), 10);
    assert_eq!(lines.len(), 3);
    for l in &lines {
        assert!(l.spans[0].content.width() <= 10);
    }
}

#[test]
fn wrap_text_handles_empty_and_multiline() {
    assert_eq!(wrap_text("", 10).len(), 1);
    let two = wrap_text("one\ntwo", 10);
    assert_eq!(two.len(), 2);
    assert_eq!(two[1].spans[0].content, "two");
}

/// The wrap helper exposes per-logical-line first-row offsets and keeps
/// the plain (non-indexed) wrapper's behavior identical.
#[test]
fn wrap_message_rows_indexed_matches_plain_wrapper() {
    let lines = vec![
        plain_line("short"),
        plain_line("this line definitely wraps at a narrow width"),
        plain_line(""),
        plain_line("x"),
    ];
    let (rows, first_row_of) = wrap_message_rows_indexed(&lines, 12);
    // "short" → 1 row; the long line wraps to 4 rows; "" → 1; "x" → 1.
    assert_eq!(rows.len(), 7);
    assert_eq!(first_row_of.len(), lines.len());
    assert_eq!(first_row_of[0], 0, "short starts at row 0");
    assert_eq!(first_row_of[1], 1, "long line starts at row 1");
    assert_eq!(first_row_of[2], 5, "blank line starts after the long line");
    assert_eq!(first_row_of[3], 6, "x starts at the last row");
    assert_eq!(
        wrap_message_rows(&lines, 12),
        rows,
        "indexed variant must not change the plain wrapper's output"
    );
}
