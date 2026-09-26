/// the shared wrap chunker must agree with the
/// renderer row-for-row (character-level, display width, exact fit stays
/// one row, degenerate width never loops).
#[test]
fn wrap_line_chunks_by_display_width() {
    assert_eq!(crate::app::wrap_line("abc", 10), vec!["abc"]);
    assert_eq!(crate::app::wrap_line("abc", 3), vec!["abc"]);
    assert_eq!(crate::app::wrap_line("abcd", 3), vec!["abc", "d"]);
    assert_eq!(crate::app::wrap_line("abcdef", 2), vec!["ab", "cd", "ef"]);
    // Wide (CJK) chars count their display width, not their char count.
    assert_eq!(
        crate::app::wrap_line("\u{4f60}\u{597d}\u{4e16}", 4),
        vec!["\u{4f60}\u{597d}", "\u{4e16}"]
    );
    // Degenerate width: clamped to 1, no division by zero, no hang.
    assert_eq!(crate::app::wrap_line("ab", 0), vec!["a", "b"]);
    assert_eq!(crate::app::wrapped_row_count("", 5), 1);
}

//------------------------------------------------------------------------
