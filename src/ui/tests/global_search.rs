use super::super::*;
use super::support::*;

// render-level checks --------------------

/// The global search popup renders the query row, the live match list
/// with THREAD TITLE + role labels + snippets, and the title carries the
/// total match count AND the searched-thread count.
#[test]
fn search_all_popup_renders_thread_titles_matches_and_counts() {
    let mut app = App::new(vec![Chat::new("Alpha"), Chat::new("Beta")]);
    app.chats[0]
        .messages
        .push(Message::user("the needle question"));
    app.chats[1]
        .messages
        .push(Message::assistant("answer with needle inside"));
    app.begin_search_all();
    for ch in "needle".chars() {
        app.search_all_push(ch);
    }

    let rows = render_grid(&mut app);
    let flat: String = rows.join("\n");

    assert!(
        flat.contains("Search all threads")
            && flat.contains("\u{00b7} 2")
            && flat.contains("(2 threads)"),
        "title must carry total count + thread count: {flat:?}"
    );
    assert!(flat.contains("\u{276f} needle"), "query row missing");
    assert!(flat.contains("Alpha"), "thread title label missing");
    assert!(flat.contains("Beta"), "thread title label missing");
    assert!(flat.contains("You"), "user role label missing");
    assert!(flat.contains("Chibi"), "assistant role label missing");
    assert!(
        flat.contains("needle question") && flat.contains("needle inside"),
        "snippet context windows missing"
    );
    assert!(flat.contains("Enter jump"), "hint missing");
    // Selected row marker rendered.
    assert!(flat.contains("\u{25b8}"), "selection marker missing");
    // Visually distinguishable from the in-thread popup.
    assert!(!flat.contains("Find in thread"), "wrong popup title");
}

/// Empty query state renders gracefully: placeholder query row, no
/// match rows, zero count + thread count in the title, hint intact.
#[test]
fn search_all_popup_empty_query_is_graceful() {
    let mut app = App::new(vec![Chat::new("solo")]);
    app.chats[0].messages.push(Message::user("needle"));
    app.begin_search_all();
    let rows = render_grid(&mut app);
    let flat: String = rows.join("\n");

    assert!(
        flat.contains("type to search all threads\u{2026}"),
        "placeholder missing: {flat:?}"
    );
    assert!(
        flat.contains("\u{00b7} 0") && flat.contains("(1 thread)"),
        "zero count + thread count in title"
    );
    assert!(!flat.contains("\u{25b8}"), "no selection with no matches");
    assert!(flat.contains("Esc close"), "hint intact");
}

/// Tiny terminals must not panic while the global search popup is open —
/// the box clamps to whatever fits, and repeated draws stay stable.
#[test]
fn search_all_popup_never_panics_on_tiny_terminal() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0].messages.push(Message::user("needle"));
    app.begin_search_all();
    for ch in "needle".chars() {
        app.search_all_push(ch);
    }
    let (rows, _) = render_grid_at_with_buffer(&mut app, 40, 8);
    let flat: String = rows.join("\n");
    assert!(flat.contains("Search all threads"), "popup title present");
    // Second draw after viewport state exists — still no panic.
    let _ = render_grid_at_with_buffer(&mut app, 40, 8);
}

/// No popup in Normal mode — no overlay artifacts.
#[test]
fn no_search_all_popup_when_closed() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0].messages.push(Message::user("needle"));
    let flat = render_grid(&mut app).join("\n");
    assert!(!flat.contains("Search all threads"));
}

/// The all-threads snippet path elides like the in-thread one (same
/// shared core — the wrapper must forward the right fields).
#[test]
fn search_all_snippet_elides_ends() {
    let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau";
    let m = crate::app::GlobalSearchMatch {
        chat_index: 3,
        chat_title: "deep thread".to_owned(),
        message_index: 0,
        line_index: 0,
        line_text: text.to_owned(),
        col: 30, // inside "delta"
    };
    let s = search_snippet_all(&m, "delta");
    assert!(s.contains("delta"), "hit must stay visible: {s:?}");
    assert!(s.starts_with('\u{2026}'), "left elision: {s:?}");
    assert!(s.ends_with('\u{2026}'), "right elision: {s:?}");
    assert!(s.len() <= text.len(), "snippet must be shorter than source");
}

// THE cross-thread wrapped-jump regression

/// THE core acceptance criterion for the global search: a match inside a
/// NON-active thread's visually-WRAPPED paragraph must (a) activate that
/// thread (same mechanics as Ctrl+↑/↓ switching) and (b) land on the
/// exact WRAPPED row near the top of the pane — never the message's
/// first row, never the bottom.
#[test]
fn search_all_jump_activates_non_active_thread_and_lands_wrapped_row() {
    // Narrow chat pane forces heavy wrapping of the long paragraph.
    const FRAME_W: u16 = 60;
    const FRAME_H: u16 = 24;
    // Chat pane inner width = frame - sidebar(26) - the pane's two
    // rounded border columns.
    let pane_width = FRAME_W - 26 - 2;

    let mut app = App::new(vec![Chat::new("target"), Chat::new("decoy")]);
    // The ACTIVE chat at search time: chat 0. Lots of filler so
    // follow-bottom of THIS chat would never show chat 1's needle.
    {
        let chat = &mut app.chats[0];
        chat.messages
            .push(Message::user("filler in the active thread"));
        for i in 0..20 {
            chat.messages.push(Message::user(format!("filler q {i}")));
            chat.messages.push(Message::assistant("short ack"));
        }
    }
    // The NON-active chat holding the wrapped needle.
    {
        let chat = &mut app.chats[1];
        chat.messages.push(Message::user("first question"));
        // A LONG single-paragraph reply: one logical line that wraps to
        // many display rows at this width. The needle sits in the MIDDLE
        // of the paragraph so the jump must land on a continuation row.
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
    app.active = 0; // searching from chat 0; the hit lives in chat 1

    // Rebuild logical lines of the TARGET chat exactly as render_chat
    // does, then use the SAME indexed wrap helper the renderer consumes
    // to prove the needle lives on a WRAPPED continuation row.
    let mut lines: Vec<markdown::MdLine> = Vec::new();
    let mut msg_ranges: Vec<(usize, usize)> = Vec::new();
    for msg in &app.chats[1].messages {
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
    let logical = msg_ranges[1].0 + 1; // long paragraph, after role header
    let needle_wrapped_row = first_row_of[logical];
    assert!(
        needle_wrapped_row > 1,
        "needle must live on a WRAPPED continuation row, got {needle_wrapped_row}"
    );

    // Full path: open the GLOBAL search from chat 0, type, jump.
    app.begin_search_all();
    for ch in "needle42".chars() {
        app.search_all_push(ch);
    }
    assert_eq!(app.search_all_matches().len(), 1);
    let (ci, mi, li, col) = {
        let m = &app.search_all_matches()[0];
        (m.chat_index, m.message_index, m.line_index, m.col)
    };
    assert_eq!(ci, 1, "hit lives in the NON-active thread");
    assert!(app.jump_to_selected_all(), "match must be found");
    assert_eq!(app.active, 1, "target thread activated by the jump");
    assert_eq!(app.pending_global_search_jump, Some((ci, mi, li, col)));

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
