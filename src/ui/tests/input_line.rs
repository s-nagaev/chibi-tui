use super::super::*;
use super::support::*;

//------------------------------------------------------------------------

/// While renaming, the prompt line must show the ✎ rename editor — the
/// regular `❯`-marked prompt (placeholder or typed draft) must be fully
/// hidden so the two editors never mix on one baseline.
#[test]
fn prompt_hidden_while_renaming() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.begin_rename();
    let rows = render_grid(&mut app);

    // Input line = 2nd row from the bottom.
    let input_row = &rows[rows.len() - 2];
    assert!(
        input_row.contains("Rename thread"),
        "rename editor must replace the prompt, got {input_row:?}"
    );
    assert!(
        !input_row.contains("Type a message"),
        "prompt placeholder must NOT leak into the rename line: {input_row:?}"
    );
}

/// The `❯` prompt marker must sit before the placeholder on an empty
/// input, and before the typed text once the user types.
#[test]
fn input_marker_renders_before_placeholder_and_text() {
    let mut app = App::new(vec![Chat::new("chat")]);

    // Empty → marker at the chat-column start, dim placeholder after it.
    let rows = render_grid(&mut app);
    let input_row = &rows[rows.len() - 2];
    let marker_col = col_of(input_row, '❯').expect("marker missing");
    let ph_col = col_of_sub(input_row, "Type a message").expect("placeholder missing");
    assert_eq!(
        marker_col + PROMPT_MARKER_WIDTH as usize,
        ph_col,
        "marker must sit directly before the placeholder, got {input_row:?}"
    );

    // Typed text → same marker position, text after it, no placeholder.
    app.input.insert_str("hello");
    let rows = render_grid(&mut app);
    let input_row = &rows[rows.len() - 2];
    let marker_col = col_of(input_row, '❯').expect("marker missing with text");
    assert!(
        input_row.contains("hello"),
        "typed text missing: {input_row:?}"
    );
    assert!(
        !input_row.contains("Type a message"),
        "placeholder must hide once typing starts: {input_row:?}"
    );
    let text_col = col_of_sub(input_row, "hello").unwrap();
    assert_eq!(
        marker_col + PROMPT_MARKER_WIDTH as usize,
        text_col,
        "text must start right after `❯ `, got {input_row:?}"
    );
}

/// Empty input: the `⏎ send` chip shares the placeholder's baseline,
/// hugging the RIGHT edge of the CHAT COLUMN (one cell before the
/// sidebar divider). Chip starts at display column 112 and ends at 118.
#[test]
fn send_chip_right_aligned_on_placeholder_baseline() {
    let mut app = App::new(vec![Chat::new("chat")]);
    let rows = render_grid(&mut app);
    let row = &rows[rows.len() - 2];

    assert!(row.contains("⏎ send"), "chip missing: {row:?}");
    assert!(
        row.contains("Type a message…"),
        "placeholder missing: {row:?}"
    );

    // Chip occupies exactly the 7 display columns ending at column 118
    // (divider at 119 untouched).
    let chip_col = col_of_sub(row, "⏎ send").expect("chip missing");
    assert_eq!(chip_col, 112, "unexpected chip position: {row:?}");

    // Same-baseline guarantee: pure padding between placeholder end and
    // the chip.
    let ph_end = col_of_sub(row, "Type a message…").unwrap() + "Type a message…".width();
    let polluted: Vec<char> = row
        .chars()
        .skip(ph_end)
        .take(chip_col - ph_end)
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        polluted.is_empty(),
        "baseline polluted between placeholder and chip: {polluted:?}"
    );
}

// render-level checks -------------------------

/// The rename editor shows label + current draft on one baseline with
/// the save/cancel hint right-aligned at the panel edge.
#[test]
fn rename_editor_renders_draft_with_right_aligned_hint() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.begin_rename();
    app.rename_push('X');
    app.rename_push('Y');
    let rows = render_grid(&mut app);
    let input_row = &rows[rows.len() - 2];

    assert!(input_row.contains("✎ Rename thread…"), "got {input_row:?}");
    assert!(input_row.contains("XY"), "draft missing: {input_row:?}");
    assert!(
        input_row.contains("\u{23ce} save \u{00b7} esc cancel"),
        "save/cancel hint missing: {input_row:?}"
    );

    // Right-aligned: the hint's last glyph sits at display column 118 —
    // the final cell of the chat column (0..=118); divider at 119 stays.
    let hint_end =
        col_of_sub(input_row, "esc cancel ").expect("hint missing") + "esc cancel ".width();
    assert_eq!(hint_end, 119, "hint not flush with the panel edge");
}

/// the input row carries the panel tint background —
/// but ONLY inside the chat column (cols 26..=118 @120). The divider cell
/// (col 25) and the sidebar strip (cols 0..=24) must NOT get the tint.
#[test]
fn input_row_has_panel_tint_background() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("chat")]);

    let (_, buf) = render_grid_with_buffer(&mut app);
    let y = buf.area.height - 2;
    assert_ne!(
        buf[(0, y)].bg,
        theme.input_panel_bg,
        "sidebar strip must NOT get the input tint"
    );
    assert_eq!(
        buf[(26, y)].bg,
        theme.input_panel_bg,
        "empty input row must carry the panel tint from the chat-column start"
    );
    assert_eq!(
        buf[(118, y)].bg,
        theme.input_panel_bg,
        "tint must run to the chat column's right edge"
    );
    assert_ne!(
        buf[(25, y)].bg,
        theme.input_panel_bg,
        "divider cell must NOT get the input tint"
    );

    app.input.insert_str("tinted");
    let (_, buf) = render_grid_with_buffer(&mut app);
    let y = buf.area.height - 2;
    assert_eq!(
        buf[(60, y)].bg,
        theme.input_panel_bg,
        "typed-over input row must keep the panel tint"
    );
}

// restored canonical render-level checks ------

/// Exactly ONE `│` glyph on the input row, at the sidebar-border column
/// (col 25), keeping its own bg — NOT the input tint. Input tint confined
/// to sampled chat-column cells; nothing left of the divider is tinted.
#[test]
fn sidebar_divider_unbroken_through_input_row() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("chat")]);
    let (_, buf) = render_grid_with_buffer(&mut app);
    let y = buf.area.height - 2;

    let divider_cols: Vec<u16> = (0..buf.area.width)
        .filter(|&x| buf[(x, y)].symbol() == "\u{2502}")
        .collect();
    assert_eq!(divider_cols, vec![25], "exactly one divider cell");

    let divider = &buf[(25, y)];
    assert_eq!(divider.fg, theme.selection, "divider fg");
    assert_ne!(
        divider.bg, theme.input_panel_bg,
        "divider cell must NOT carry the input tint"
    );

    for x in [26u16, 60, 100, 112] {
        assert_eq!(
            buf[(x, y)].bg,
            theme.input_panel_bg,
            "tint confined to the chat column at col {x}"
        );
    }
    assert_ne!(
        buf[(0, y)].bg,
        theme.input_panel_bg,
        "nothing left of the divider may be tinted"
    );
}

/// Rename line shares the same tinted treatment: `❯` marker at the
/// chat-column start AND `input_panel_bg` tint across the chat column;
/// the divider cell stays untinted.
#[test]
fn rename_line_shares_marker_and_panel_tint() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("chat")]);
    app.begin_rename();

    let (_, buf) = render_grid_with_buffer(&mut app);
    let y = buf.area.height - 2;

    assert_eq!(
        buf[(26, y)].symbol(),
        "\u{276f}",
        "marker must sit at the chat-column start in rename mode too"
    );
    assert_eq!(
        buf[(60, y)].bg,
        theme.input_panel_bg,
        "rename line must carry the input-panel tint mid-row"
    );
    assert_ne!(
        buf[(25, y)].bg,
        theme.input_panel_bg,
        "rename tint must not leak onto the divider cell"
    );
}

/// Empty input: `⏎ send` chip hugs the RIGHT edge of the chat column.
/// Math invariant: chip_start + chip_width == CHAT_X0 + chat_column.width
/// → starts at col 112, ends flush at 118, zero non-whitespace between
/// placeholder end and chip start, margin cell past the column untouched.
#[test]
fn send_chip_right_edge_and_baseline_math_with_marker() {
    const CHAT_X0: u16 = 26;
    let theme = Theme::tokyo_night();
    let chat_column_width = |buf_area_width: u16| -> u16 { buf_area_width - 1 - CHAT_X0 };

    let mut app = App::new(vec![Chat::new("chat")]);
    let (_, buf) = render_grid_with_buffer(&mut app);
    let y = buf.area.height - 2;
    let row = &render_grid(&mut app)[y as usize];

    assert!(row.contains("⏎ send"), "chip missing: {row:?}");

    let chip_col = col_of_sub(row, "⏎ send").expect("chip missing") as u16;
    let ccw = chat_column_width(buf.area.width);
    assert_eq!(
        chip_col + "\u{23ce} send ".width() as u16,
        CHAT_X0 + ccw,
        "chip must END flush with the chat column's right edge"
    );
    // Margin cell past the chat column stays empty and untinted.
    assert_eq!(buf[(119, y)].symbol(), " ", "margin cell past the column");
    assert_ne!(buf[(119, y)].bg, theme.input_panel_bg);

    // Same-baseline guarantee: pure padding between placeholder end and
    // the chip start.
    let ph_end = col_of_sub(row, "Type a message…").unwrap() + "Type a message…".width();
    let polluted: Vec<char> = row
        .chars()
        .skip(ph_end)
        .take((chip_col as usize) - ph_end)
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        polluted.is_empty(),
        "baseline polluted between placeholder and chip: {polluted:?}"
    );
}
