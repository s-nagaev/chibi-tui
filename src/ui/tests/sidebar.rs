use super::super::*;
use super::support::*;

// visuals ------------------------------------------

/// Focus emphasis differs between focuses: with Chat focused, the
/// sidebar divider is the resting dark selection tone; with Sidebar
/// focused it lifts to theme.blue. Snapshot-style per-cell fg compare
/// on the same app state, only the focus flipped.
#[test]
fn sidebar_border_emphasis_differs_between_focuses() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("one"), Chat::new("two")]);

    // Divider column: x=29 (sidebar width 30, right border col), any
    // main-area row inside both renders.
    let (_, chat_focused_buf) = render_grid_with_buffer(&mut app);
    assert_eq!(chat_focused_buf[(29, 5)].fg, theme.selection);

    app.focus = Focus::Sidebar;
    let (_, sidebar_focused_buf) = render_grid_with_buffer(&mut app);
    assert_eq!(sidebar_focused_buf[(29, 5)].fg, theme.blue);

    // Bottom-extended divider (below the block, e.g. the input band)
    // follows the same emphasis: row 33 of 34 @120×34 is status-ish;
    // check row just below the sidebar block too. Use row 30.
    assert_eq!(sidebar_focused_buf[(29, 30)].fg, theme.blue);
    assert_ne!(
        chat_focused_buf[(29, 5)].fg,
        sidebar_focused_buf[(29, 5)].fg,
        "border color must visibly differ between focuses"
    );
}

/// The ` Chats ` title brightens blue → cyan while the sidebar holds
/// focus (same snapshot technique as the border test).
#[test]
fn sidebar_title_emphasis_differs_between_focuses() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("one"), Chat::new("two")]);

    // Title cell: find "C" of "Chats" on row 0 (x≈1..7).
    let title_x = |buf: &ratatui::buffer::Buffer| {
        (0..20)
            .find(|&x| buf[(x, 0)].symbol() == "C")
            .expect("title not found")
    };

    let (_, chat_buf) = render_grid_with_buffer(&mut app);
    let x_chat = title_x(&chat_buf);
    assert_eq!(chat_buf[(x_chat, 0)].fg, theme.blue);

    app.focus = Focus::Sidebar;
    let (_, side_buf) = render_grid_with_buffer(&mut app);
    let x_side = title_x(&side_buf);
    assert_eq!(side_buf[(x_side, 0)].fg, theme.cyan);
}

/// Idle unselected dots brighten dim → fg while the sidebar owns the
/// keyboard; selected idle stays green either way.
#[test]
fn sidebar_idle_dots_brighten_when_focused() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("first"), Chat::new("second")]);

    let (_, chat_buf) = render_grid_with_buffer(&mut app);
    // Row 2 = second chat's unselected idle dot (column 1, right of the
    // outline's left border).
    assert_eq!(chat_buf[(1, 2)].fg, theme.dim);

    app.focus = Focus::Sidebar;
    let (_, side_buf) = render_grid_with_buffer(&mut app);
    assert_eq!(side_buf[(1, 2)].fg, theme.fg, "unselected dot brightens");
    // Selected (row 1) keeps green in both focuses.
    assert_eq!(chat_buf[(1, 1)].fg, theme.green);
    assert_eq!(side_buf[(1, 1)].fg, theme.green);
}

/// The prompt's `❯` marker dims cyan → theme.dim while the sidebar
/// holds focus (subtle "typing goes nowhere" cue); back to cyan on
/// return.
#[test]
fn prompt_marker_dims_while_sidebar_focused() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("one")]);
    type_text_into_input(&mut app, "draft");

    let marker_fg = |app: &mut App| render_grid_at_with_buffer(app, 120, 34).1[(30, 32)].fg;

    assert_eq!(marker_fg(&mut app), theme.cyan, "Chat focus → cyan");
    app.focus = Focus::Sidebar;
    assert_eq!(marker_fg(&mut app), theme.dim, "Sidebar focus → dimmed");
}

// selection auto-scroll ------------------------------

/// Type into the prompt editor through the in-house input directly (test
/// helper shared by marker tests).
fn type_text_into_input(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.input.input(crate::input::Input {
            key: crate::input::Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        });
    }
}

/// Selection-aware viewport: with MORE chats than sidebar rows, moving
/// the selection down must scroll the list so the ACTIVE row becomes
/// visible (ratatui's List keeps a fresh-state offset minimal but
/// selection-inclusive every frame), and moving back up restores the
/// top rows.
#[test]
fn sidebar_scrolls_selection_into_view_beyond_viewport() {
    let chats: Vec<Chat> = (0..30).map(|i| Chat::new(format!("chat-{i}"))).collect();
    let mut app = App::new(chats);

    // Tiny frame: 12 rows total → main area ≈ 9 rows ⇒ ~9 visible chats.
    let rows_of = |app: &mut App| {
        render_grid_at_with_buffer(app, 120, 12)
            .0
            .into_iter()
            .collect::<Vec<_>>()
    };
    let visible_named = |rows: &[String], name: &str| rows.iter().any(|r| r.contains(name));

    let rows = rows_of(&mut app);
    assert!(visible_named(&rows, "chat-0"), "top chat visible initially");
    assert!(!visible_named(&rows, "chat-25"), "precondition sanity");

    // Jump to a selection deep BELOW the viewport…
    for _ in 0..25 {
        app.select_next();
    }
    let rows = rows_of(&mut app);
    assert!(
        visible_named(&rows, "chat-25"),
        "active chat scrolled INTO view when below viewport"
    );

    // …and far ABOVE it again.
    app.active = 0;
    app.scroll = 0;
    let rows = rows_of(&mut app);
    assert!(
        visible_named(&rows, "chat-0"),
        "selection returned to the visible top"
    );
}

// rendering ----------------------------

/// Background reply rendering: the inactive thread's dot turns yellow
/// (unread-activity slot) and its name goes bold, while the row keeps
/// the resting panel background (NO selection highlight). The selected
/// chat keeps its active green dot + highlight, a read neighbour keeps
/// the default dim dot and an unbold name. The marker survives fresh
/// re-renders (state, not a one-frame effect).
#[test]
fn unread_background_thread_renders_yellow_dot_bold_name_no_highlight() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![
        Chat::new("fresh"),
        Chat::new("marked"),
        Chat::new("quiet"),
    ]);
    app.active = 1; // "marked" selected, both neighbours inactive
    app.chats[0].unread = true;

    let (rows, buf) = render_grid_with_buffer(&mut app);
    // Sidebar rows: " Chats " title on row 0, chat rows start at 1; the
    // lifecycle dot sits at column 1, the name at column 3 (right of the
    // outline's left border).
    assert!(rows[1].contains("fresh") && rows[2].contains("marked"));

    // Unread inactive dot: hollow glyph in the unread-activity yellow.
    assert_eq!(buf[(1, 1)].symbol(), "\u{25cb}");
    assert_eq!(buf[(1, 1)].fg, theme.unread_activity);
    // Bold dim name, resting background: no highlight.
    assert_eq!(buf[(3, 1)].fg, theme.dim);
    assert!(buf[(3, 1)].modifier.contains(Modifier::BOLD));
    assert_ne!(buf[(3, 1)].bg, theme.selection, "no selection highlight");
    assert_eq!(buf[(3, 1)].bg, theme.panel);

    // Selected chat: active-marker dot, highlight on its row.
    assert_eq!(buf[(1, 2)].fg, theme.active_marker);
    assert_eq!(buf[(3, 2)].bg, theme.selection, "selected row highlighted");

    // Read inactive neighbour: default dot, plain dim name.
    assert_eq!(buf[(1, 3)].fg, theme.dot_default);
    assert!(!buf[(3, 3)].modifier.contains(Modifier::BOLD));

    // Fresh frame + scrolling: the marker is sticky until selection.
    let (_, buf2) = render_grid_with_buffer(&mut app);
    assert_eq!(buf2[(1, 1)].fg, theme.unread_activity, "survives re-render");
    app.scroll_up(5);
    let (_, buf3) = render_grid_with_buffer(&mut app);
    assert_eq!(buf3[(1, 1)].fg, theme.unread_activity, "survives scrolling");
}

// name wrap -----------------------------------------

/// Visible text of the sidebar's name cell on buffer row `y`: the columns
/// between the outline's left border + dot cell (x=0,1 and the space at 2)
/// and the outline's right border (x=29). A double-width glyph occupies two
/// cells, so the walk advances by the glyph's display width and skips the
/// continuation column; trailing blanks from unused columns are trimmed.
fn name_cell(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    let mut out = String::new();
    let mut x = 3u16;
    while x < 29 {
        let ch = buf[(x, y)].symbol().chars().next().unwrap_or(' ');
        out.push(ch);
        x += unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1) as u16;
    }
    out.trim_end().to_owned()
}

/// Sidebar name budget @120: the 30-wide sidebar block leaves 28
/// inner columns, the `dot + space` cell takes 2 of them, so 26 columns are
/// the wrap width. At exactly that budget the title stays on one row; one
/// column more wraps at the last space that fits.
#[test]
fn sidebar_name_at_the_row_budget_is_kept_whole_and_wider_wraps() {
    let exact = "a".repeat(26);
    let over = "b".repeat(27);
    let mut app = App::new(vec![Chat::new(exact.clone()), Chat::new(over.clone())]);
    let (_, buf) = render_grid_with_buffer(&mut app);

    assert_eq!(
        name_cell(&buf, 1),
        exact,
        "a name at the budget stays whole"
    );
    // No space to break at → a hard break at the wrap width.
    assert_eq!(name_cell(&buf, 2), "b".repeat(26));
    assert_eq!(name_cell(&buf, 3), "b", "the tail wraps onto the next row");
}

/// A name with spaces wraps at the LAST space that fits, and the break space
/// is dropped: `"aaaa bbbb"` at 4 columns reads `aaaa` / `bbbb`, never
/// `aaaa` / ` bbb`.
#[test]
fn sidebar_name_wraps_at_word_boundaries_and_drops_the_break_space() {
    let mut app = App::new(vec![Chat::new("aaaa bbbb cccc dddd eeee ffff")]);
    let (_, buf) = render_grid_with_buffer(&mut app);

    // 26 columns ⇒ `aaaa bbbb cccc dddd eeee` then `ffff`.
    assert_eq!(name_cell(&buf, 1), "aaaa bbbb cccc dddd eeee");
    assert_eq!(name_cell(&buf, 2), "ffff", "second row carries the tail");
    assert_eq!(name_cell(&buf, 3), "", "no third row needed");
}

/// A name longer than three rows caps at three: the first two rows are full
/// wraps and the third is cut to the budget minus the ellipsis with a
/// trailing `…`, so overflow is visible instead of silently vanishing.
#[test]
fn sidebar_name_past_three_rows_caps_with_a_trailing_ellipsis() {
    // 100 chars → five hard-wrapped rows of 26; capped to 2 full + ellipsis.
    let mut app = App::new(vec![Chat::new("z".repeat(100))]);
    let (_, buf) = render_grid_with_buffer(&mut app);

    assert_eq!(name_cell(&buf, 1), "z".repeat(26));
    assert_eq!(name_cell(&buf, 2), "z".repeat(26));
    assert_eq!(
        name_cell(&buf, 3),
        format!("{}\u{2026}", "z".repeat(25)),
        "row 3 is cut to 25 columns plus the ellipsis"
    );
    assert_eq!(name_cell(&buf, 4), "", "nothing spills onto a fourth row");
}

/// Multi-line titles (Shift+Enter allows `\n` in a name) collapse to spaces
/// BEFORE wrapping, so a name with a newline is one wrapped stream instead
/// of a hard row break.
#[test]
fn sidebar_multiline_name_collapses_to_spaces_before_wrapping() {
    let short = "first\nsecond";
    let long = format!("{}\n{}", "x".repeat(30), "y".repeat(30));
    let mut app = App::new(vec![Chat::new(short), Chat::new(long)]);
    let (_, buf) = render_grid_with_buffer(&mut app);

    assert_eq!(name_cell(&buf, 1), "first second");
    // The long name is one 61-char stream: the first row is a full 26-col
    // hard wrap of the `x` word, the second carries its 4-char tail (the
    // collapsed space + the 30-col `y` word never fit after it), and the
    // cap cuts the `y` overflow row to 25 columns + `…`.
    assert_eq!(name_cell(&buf, 2), "x".repeat(26));
    assert_eq!(
        name_cell(&buf, 3),
        "x".repeat(4),
        "the `x` word tail before the break"
    );
    assert_eq!(
        name_cell(&buf, 4),
        format!("{}\u{2026}", "y".repeat(25)),
        "the overflow `y` half is capped and ellipsized"
    );
}

/// A wide (CJK) name wraps by DISPLAY width, not char count: 2-column glyphs
/// fill the 26-column budget thirteen at a time, and the ellipsis consumes the
/// 25 remaining columns of the capped third row.
#[test]
fn sidebar_wide_cjk_name_wraps_by_display_width_not_char_count() {
    // 40 glyphs = 80 display columns → three 13-glyph rows + 10 left over,
    // capped to 3.
    let name: String = "\u{4e00}".repeat(40);
    let mut app = App::new(vec![Chat::new(name)]);
    let (_, buf) = render_grid_with_buffer(&mut app);

    let glyph = "\u{4e00}";
    assert_eq!(name_cell(&buf, 1), glyph.repeat(13));
    assert_eq!(name_cell(&buf, 2), glyph.repeat(13));
    // Row 3 is the capped row: cut to 25 columns, which holds twelve whole
    // glyphs (24 columns) plus the single-column `…`; the 26th column can
    // hold no whole wide glyph, so it stays blank rather than half-drawn.
    assert_eq!(name_cell(&buf, 3), format!("{}\u{2026}", glyph.repeat(12)));
    assert_eq!(
        buf[(28, 3)].symbol(),
        " ",
        "the leftover second column stays blank, no half glyph"
    );
}

/// The sidebar's right border and the chat pane's left border survive a wrap:
/// the name never spills past the inner edge.
#[test]
fn sidebar_wrapped_name_keeps_the_outlines_intact() {
    let mut app = App::new(vec![Chat::new("abcdefghijklmnopqrstuvwxyz0123456789XYZ")]);
    let (_, buf) = render_grid_with_buffer(&mut app);

    for y in 1..=3 {
        assert_eq!(buf[(29, y)].symbol(), "\u{2502}", "sidebar right border");
        assert_eq!(buf[(30, y)].symbol(), "\u{2502}", "chat pane left border");
    }
}

/// Selection highlight covers EVERY row of a multi-row wrapped item, not
/// just the first one (ratatui's `List` applies `highlight_style` to the
/// whole item area, which is why the item is a multi-line `Text`).
#[test]
fn sidebar_selected_wrapped_name_highlights_all_rows() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("z".repeat(60)), Chat::new("other")]);
    app.active = 0;
    let (_, buf) = render_grid_with_buffer(&mut app);

    for y in 1..=3 {
        assert_eq!(
            buf[(3, y)].bg,
            theme.selection,
            "row {y} of the selected wrapped item is highlighted"
        );
    }
    // The next chat starts below the three wrapped rows, unhighlighted.
    assert_ne!(buf[(3, 4)].bg, theme.selection);
}
