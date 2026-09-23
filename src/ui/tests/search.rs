use super::super::*;
use super::support::*;

// render-level checks -------------------------

/// The search popup renders the query row, the live match list with role
/// labels + snippets, and the total count in the title.
#[test]
fn search_popup_renders_query_matches_and_count() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0]
        .messages
        .push(Message::user("the needle question"));
    app.chats[0]
        .messages
        .push(Message::assistant("answer with needle inside"));
    app.begin_search();
    for ch in "needle".chars() {
        app.search_push(ch);
    }

    let rows = render_grid(&mut app);
    let flat: String = rows.join("\n");

    assert!(
        flat.contains("Find in thread") && flat.contains("\u{00b7} 2"),
        "title must carry the total count: {flat:?}"
    );
    assert!(flat.contains("\u{276f} needle"), "query row missing");
    assert!(flat.contains("You"), "user role label missing");
    assert!(flat.contains("Chibi"), "assistant role label missing");
    assert!(
        flat.contains("needle question") && flat.contains("needle inside"),
        "snippet context windows missing"
    );
    assert!(flat.contains("Enter jump"), "hint missing");
    // Selected row marker rendered.
    assert!(flat.contains("\u{25b8}"), "selection marker missing");
}

/// Empty query state renders gracefully: placeholder query row, no
/// match rows, no crash, hint still present.
#[test]
fn search_popup_empty_query_is_graceful() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0].messages.push(Message::user("needle"));
    app.begin_search();
    let rows = render_grid(&mut app);
    let flat: String = rows.join("\n");

    assert!(
        flat.contains("type to search\u{2026}"),
        "placeholder missing"
    );
    assert!(flat.contains("\u{00b7} 0"), "zero count in title");
    assert!(!flat.contains("\u{25b8}"), "no selection with no matches");
    assert!(flat.contains("Esc close"), "hint intact");
}

/// Tiny terminals must not panic while the search popup is open — the
/// popup box clamps to whatever fits (same guarantee as the delete
/// popup), and repeated draws stay stable.
#[test]
fn search_popup_never_panics_on_tiny_terminal() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0].messages.push(Message::user("needle"));
    app.begin_search();
    for ch in "needle".chars() {
        app.search_push(ch);
    }
    let (rows, _) = render_grid_at_with_buffer(&mut app, 40, 8);
    let flat: String = rows.join("\n");
    assert!(flat.contains("Find in thread"), "popup title present");
    // Second draw after viewport state exists — still no panic.
    let _ = render_grid_at_with_buffer(&mut app, 40, 8);
}

/// No popup in Normal mode — no overlay artifacts.
#[test]
fn no_search_popup_when_closed() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0].messages.push(Message::user("needle"));
    let flat = render_grid(&mut app).join("\n");
    assert!(!flat.contains("Find in thread"));
}

/// Snippet elides long context with `…` on both sides and keeps the hit
/// visible.
#[test]
fn search_snippet_elides_ends() {
    // Long enough that the 16-char window can't cover both ends.
    let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau";
    let m = crate::app::SearchMatch {
        message_index: 0,
        line_index: 0,
        line_text: text.to_owned(),
        col: 30, // inside "delta"
    };
    let s = search_snippet(&m, "delta");
    assert!(s.contains("delta"), "hit must stay visible: {s:?}");
    assert!(s.starts_with('\u{2026}'), "left elision: {s:?}");
    assert!(s.ends_with('\u{2026}'), "right elision: {s:?}");
    assert!(s.len() <= text.len(), "snippet must be shorter than source");
}

// THE wrapped-jump regression ----------------

/// THE core acceptance criterion: a search hit inside a visually-WRAPPED
/// paragraph (one logical line spanning many display rows) must jump to
/// the correct WRAPPED row — the match sits near the top of the viewport
/// afterwards, not buried at the bottom (which a logical-line-only
/// approximation would produce) and not at the message's first row.
#[test]
fn search_jump_lands_wrapped_match_near_top() {
    // Narrow chat pane forces heavy wrapping of the long paragraph.
    const FRAME_W: u16 = 60;
    const FRAME_H: u16 = 24;
    // Chat pane inner width = frame - sidebar(26) - the pane's two
    // rounded border columns; chat inner height =
    // frame - top border(1) - bottom border(1) - spinner(1) - input(1)
    // - status(1).
    let pane_width = FRAME_W - 26 - 2;

    let mut app = App::new(vec![Chat::new("wrap")]);
    {
        let chat = &mut app.chats[0];
        chat.messages.push(Message::user("first question"));
        // A LONG single-paragraph reply: one logical line that wraps to
        // many display rows at this width. The needle sits in the MIDDLE
        // of the paragraph so the jump must land on a continuation row,
        // not the first row of the message.
        let mut para = String::from("alpha filler ");
        for _ in 0..40 {
            para.push_str("filler word ");
        }
        para.push_str("needle42 mid-paragraph ");
        for _ in 0..40 {
            para.push_str("trailing filler ");
        }
        chat.messages.push(Message::assistant(para));
        // Plenty of content BELOW so follow-bottom would hide the match
        // entirely — the jump must detach from the bottom.
        for i in 0..30 {
            chat.messages.push(Message::user(format!("filler q {i}")));
            chat.messages.push(Message::assistant("short ack"));
        }
    }

    // Find the wrapped row of the needle via the renderer's own totals.
    // Rebuild logical lines exactly as render_chat does, then use the
    // SAME indexed wrap helper the renderer consumes.
    let mut lines: Vec<markdown::MdLine> = Vec::new();
    let mut msg_ranges: Vec<(usize, usize)> = Vec::new();
    for msg in &app.chats[0].messages {
        let start = lines.len();
        match msg.role {
            Role::User => {
                lines.push(Line::from(Span::styled("\u{25cf} You", Style::new())));
            }
            Role::Assistant => {
                lines.push(Line::from(Span::styled("\u{25cf} Chibi", Style::new())));
            }
        }
        if msg.pending {
            lines.push(Line::from(""));
        } else {
            lines.extend(markdown::render(&msg.markdown, &Theme::tokyo_night()));
        }
        lines.push(Line::from(""));
        msg_ranges.push((start, lines.len()));
    }
    let (_, first_row_of) = wrap_message_rows_indexed(&lines, pane_width as usize);
    // Message 1 (the long paragraph) occupies logical lines
    // [msg_ranges[1].0, msg_ranges[1].1); its rendered line 0 is
    // start + 1 (after the role header).
    let logical = msg_ranges[1].0 + 1;
    let needle_wrapped_row = first_row_of[logical];
    assert!(
        needle_wrapped_row > 1,
        "needle must live on a WRAPPED continuation row, got {needle_wrapped_row}"
    );

    // Full render path: set the pending jump and draw.
    app.begin_search();
    for ch in "needle42".chars() {
        app.search_push(ch);
    }
    let (mi, li, col) = {
        let m = &app.search_matches()[0];
        (m.message_index, m.line_index, m.col)
    };
    assert!(app.jump_to_selected(), "match must be found");
    assert_eq!(
        app.pending_search_jump,
        Some((mi, li, col)),
        "hit in message {mi}, rendered line {li}, char col {col}"
    );

    let (rows, _) = render_grid_at_with_buffer(&mut app, FRAME_W, FRAME_H);
    let y = rows
        .iter()
        .position(|r| r.contains("needle42"))
        .expect("needle must be visible after the jump");
    // Chat pane inner starts at y=1 (top border). "Near top" = within the
    // first few rows of the pane; 1-row padding puts the match at inner
    // row 1 → grid y=2. Allow a small tolerance for border/glyph offsets.
    assert!(
        (1..=5).contains(&y),
        "match must be near the top of the pane, got y={y}: {rows:?}"
    );
    assert!(
        !app.at_bottom(),
        "jump must detach from follow-bottom (match is far above the end)"
    );
    assert!(
        y < rows.len() - 4,
        "match must NOT be at the bottom of the pane"
    );
}
