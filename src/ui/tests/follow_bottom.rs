use super::super::*;
use super::support::*;

//------------------------------------------------------------------------
const TAIL_SENTINEL: &str = "ENDOFREPLY7X";

const HEAD_SENTINEL: &str = "GREETING_A1";

/// Single-line markdown paragraph — one LOGICAL line, many DISPLAY rows
/// once wrapped at ~94 chat columns.
fn long_paragraph(seed: &str, sentences: usize) -> String {
    let sentence = format!("{seed}-ish plain wrapping filler for scroll math. ");
    sentence.repeat(sentences) + seed
}

fn grid_contains(rows: &[String], needle: &str) -> bool {
    rows.iter().any(|r| r.contains(needle))
}

/// Regression (code_block_empty_render_bug): a fenced block containing a
/// line WIDER than the panel's clamped interior (~70 cols) must CLIP the
/// line to the interior, not drop it whole. The old token loop `break`-ed
/// on the first chunk that did not fit, so an over-long line rendered as
/// a fully BLANK interior row inside the frame (language badge and
/// borders present, zero code text) — exactly the user-reported empty
/// tsx box.
#[test]
fn code_block_long_line_clips_into_panel_not_blank_row() {
    let mut app = App::new(vec![Chat::new("code")]);
    let long_line = "<Item label=\"Alpha sector lightweight visible panel trim kit\" value={42} />";
    assert!(long_line.chars().count() > 70);
    let md = format!("```tsx\nconst a = 1;\nconst b = 2;\n{long_line}\n```\n");
    app.chats[0].messages.push(Message::assistant(md));

    let (rows, _) = render_grid_with_buffer(&mut app);

    // Short body lines must be fully visible inside the panel.
    assert!(
        grid_contains(&rows, "const a = 1;"),
        "short code line missing: {:?}",
        rows.iter()
            .filter(|r| r.contains('\u{2502}'))
            .collect::<Vec<_>>()
    );
    assert!(
        grid_contains(&rows, "const b = 2;"),
        "short code line missing: {:?}",
        rows.iter()
            .filter(|r| r.contains('\u{2502}'))
            .collect::<Vec<_>>()
    );
    // The over-long line must show its CLIPPED PREFIX, never a blank row.
    let prefix = &long_line[..24];
    assert!(
        grid_contains(&rows, prefix),
        "over-long code line rendered as a blank interior row (prefix {prefix:?} absent)"
    );
}

/// THE regression: with several exchanges plus a long wrapped final
/// reply, follow-bottom must show the END of the last reply above the
/// input band. The old logical-line total clipped it away.
#[test]
fn follow_bottom_shows_end_of_last_wrapped_reply() {
    let mut app = App::new(vec![Chat::new("scroll")]);
    {
        let chat = &mut app.chats[0];
        chat.messages.push(Message::user("first question"));
        // First reply anchors the TRUE top of the content.
        chat.messages.push(Message::assistant(format!(
            "{HEAD_SENTINEL} short ack zero"
        )));
        for n in 1..6 {
            chat.messages
                .push(Message::user(format!("filler question {n}")));
            chat.messages
                .push(Message::assistant(format!("short ack {n}")));
        }
        chat.messages.push(Message::user("final question"));
        chat.messages.push(Message::assistant(
            long_paragraph("wrapped", 60) + TAIL_SENTINEL,
        ));
    }

    // Follow-bottom (scroll untouched): tail sentinel must be on screen.
    let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 20);
    assert!(
        grid_contains(&rows, TAIL_SENTINEL),
        "follow-bottom hid the end of the newest wrapped reply"
    );

    // One page up, then far enough that the clamp saturates at
    // max_scroll: the view must reach the absolute top of the WRAPPED
    // content, and nothing from below may leak into that view.
    let page = app.chat_visible_rows.max(1);
    app.scroll_up(page);
    let _ = render_grid_at_with_buffer(&mut app, 120, 20); // refresh page size
    app.scroll_up(u16::MAX / 2);
    let (rows_top, _) = render_grid_at_with_buffer(&mut app, 120, 20);
    assert!(
        grid_contains(&rows_top, HEAD_SENTINEL),
        "max-scroll-up missed the true top of wrapped content"
    );
    assert!(
        !grid_contains(&rows_top, TAIL_SENTINEL),
        "top view shows rows that belong far below"
    );
}

/// Wide-glyph variant: totals must derive from unicode-width columns, not
/// byte lengths, or a CJK/emoji-heavy reply clips its tail.
#[test]
fn follow_bottom_shows_end_of_wide_glyph_reply() {
    let mut app = App::new(vec![Chat::new("wide")]);
    app.chats[0].messages.push(Message::assistant(format!(
        "{}{}",
        "\u{4e16}\u{754c}\u{30ec}\u{30d9}\u{30eb} filler ".repeat(120),
        "\u{7d42}\u{7aef}9X" // 終端 + ASCII digits sentinel
    )));
    let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 20);
    // Ratatui paints a 2-column glyph into its leading cell and pads the
    // trailing skip-cell with a blank — collapse blanks before matching.
    let squeezed: Vec<String> = rows.iter().map(|r| r.replace(' ', "")).collect();
    assert!(
        grid_contains(&squeezed, "\u{7d42}\u{7aef}9X"),
        "wide-glyph reply tail hidden in follow-bottom mode"
    );
}

/// Overflow hint fires iff WRAPPED rows exceed the visible pane — even
/// when the logical line count fits comfortably.
#[test]
fn overflow_hint_appears_iff_wrapped_rows_exceed_visible() {
    // 120×14 ⇒ main area 11 rows, spinner row y=11, chat inner height 10.
    let mut app = App::new(vec![Chat::new("hint")]);
    app.chats[0].messages.push(Message::user("q"));
    // One huge unbreakable token: 3 LOGICAL lines (~45 display rows).
    app.chats[0]
        .messages
        .push(Message::assistant("X".repeat(4000)));
    let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 14);
    assert!(
        rows[11].contains("more"),
        "wrapped overflow (>10 rows) must raise the hint"
    );

    let mut calm = App::new(vec![Chat::new("calm")]);
    calm.chats[0].messages.push(Message::assistant("tiny"));
    let (rows_calm, _) = render_grid_at_with_buffer(&mut calm, 120, 14);
    assert!(
        !rows_calm[11].contains("more"),
        "hint raised although content fits"
    );
}
