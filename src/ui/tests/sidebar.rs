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

    // Divider column: x=25 (sidebar width 26, right border col), any
    // main-area row inside both renders.
    let (_, chat_focused_buf) = render_grid_with_buffer(&mut app);
    assert_eq!(chat_focused_buf[(25, 5)].fg, theme.selection);

    app.focus = Focus::Sidebar;
    let (_, sidebar_focused_buf) = render_grid_with_buffer(&mut app);
    assert_eq!(sidebar_focused_buf[(25, 5)].fg, theme.blue);

    // Bottom-extended divider (below the block, e.g. the input band)
    // follows the same emphasis: row 33 of 34 @120×34 is status-ish;
    // check row just below the sidebar block too. Use row 30.
    assert_eq!(sidebar_focused_buf[(25, 30)].fg, theme.blue);
    assert_ne!(
        chat_focused_buf[(25, 5)].fg,
        sidebar_focused_buf[(25, 5)].fg,
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

    let marker_fg = |app: &mut App| render_grid_at_with_buffer(app, 120, 34).1[(26, 32)].fg;

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
