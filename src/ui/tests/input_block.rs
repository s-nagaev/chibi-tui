use super::super::*;
use super::support::*;

/// Vertical band layout, bottom-up at frame height `H` with editor block
/// height `h`: status hints `H-1`, band `[H-1-h ..= H-2]`, spinner row
/// directly above the band.
fn input_band(h: u16, frame_h: u16) -> std::ops::RangeInclusive<u16> {
    (frame_h - 1 - h)..=(frame_h - 2)
}

/// Shift+Enter growth: a 3-line draft occupies exactly 3 rows of the
/// bottom band (one draft line per row), chat pane shrinks upward, and
/// clearing the input collapses the block back to a single placeholder
/// row. Helper and grid must agree at every step.
#[test]
fn input_block_grows_to_three_rows_and_collapses_back() {
    let mut app = App::new(vec![Chat::new("chat")]);
    assert_eq!(app.input_lines_height(), 1);

    app.input.insert_str("one\ntwo\nthree");
    assert_eq!(app.input_lines_height(), 3);
    let rows = render_grid(&mut app);
    let h = 3u16;
    let band = input_band(h, rows.len() as u16).collect::<Vec<_>>();
    assert!(
        rows[band[0] as usize].contains("❯") && rows[band[0] as usize].contains("one"),
        "first band row must carry marker + head line: {:?}",
        rows[band[0] as usize]
    );
    assert!(
        rows[band[1] as usize].contains("two"),
        "second band row must show draft line 2: {:?}",
        rows[band[1] as usize]
    );
    assert!(
        rows[band[2] as usize].contains("three"),
        "third band row must show draft line 3: {:?}",
        rows[band[2] as usize]
    );
    // Chat pane header still rendered above the grown block.
    assert!(rows[0].contains("#1/1"));
    // Status hints pinned below the band (compacted ^N token).
    assert!(rows.last().unwrap().contains("^N"));

    // Collapse back to exactly one placeholder row.
    app.clear_input();
    assert_eq!(app.input_lines_height(), 1);
    let rows = render_grid(&mut app);
    let last = rows.len();
    assert!(
        rows[last - 2].contains("Type a message") && rows[last - 2].contains("⏎ send"),
        "placeholder row must be back at len-2: {:?}",
        rows[last - 2]
    );
    assert!(
        !rows[last - 4..last - 2].iter().any(|r| r.contains("three")),
        "no stale draft line may linger in former band rows"
    );
}

/// Sidebar divider (`│`, col 25) runs unbroken through spinner + grown
/// editor block + status line for EVERY height 2..=20; the margin cell
/// past the chat column stays empty on each band row.
#[test]
fn sidebar_divider_unbroken_at_every_growth_height() {
    for h in [2u16, 3, 5, 10, 19, 20] {
        let mut app = App::new(vec![Chat::new("chat")]);
        for _ in 0..(h - 1) {
            app.input.insert_str("\n");
        }
        app.input.insert_str("x");
        assert_eq!(app.input_lines_height(), h);

        let (_, buf) = render_grid_with_buffer(&mut app);
        for y in input_band(h, buf.area.height) {
            let cell = &buf[(25, y)];
            assert_eq!(
                cell.symbol(),
                "\u{2502}",
                "height {h}: divider missing at y{y}"
            );
            assert_ne!(
                cell.bg,
                Theme::tokyo_night().input_panel_bg,
                "height {h}: divider cell tinted at y{y}"
            );
            assert_eq!(
                buf[(119, y)].symbol(),
                " ",
                "height {h}: margin cell polluted at y{y}"
            );
        }
    }
}

/// The grown block repeats the `⏎ send` chip on its FIRST row only,
/// flush right with the chat column (ends col 118 @120).
#[test]
fn multiline_send_chip_sits_on_first_row_flush_right_only() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.input.insert_str("aa\nbb"); // height 2
    assert_eq!(app.input_lines_height(), 2);

    let rows = render_grid(&mut app);
    let band = input_band(2, rows.len() as u16).collect::<Vec<_>>();
    let first_row = &rows[band[0] as usize];
    let second_row = &rows[band[1] as usize];

    let chip_col = col_of_sub(first_row, "⏎ send").expect("chip missing on first row");
    assert_eq!(
        chip_col + "⏎ send ".width(),
        119,
        "chip must end flush with chat column edge"
    );
    assert!(
        !second_row.contains("⏎ send"),
        "chip must not repeat on later rows: {second_row:?}"
    );

    // Exactly ONE chip glyph sequence anywhere in the frame (the hints
    // bar spells no send chip at all).
    let total: usize = rows.iter().map(|r| r.matches("⏎ send").count()).sum();
    assert_eq!(total, 1, "chip must appear exactly once per frame");
}

/// Panel tint spans EVERY row of the grown block inside the chat column
/// only — divider cell, sidebar strip and right margin stay untinted.
#[test]
fn grown_block_tints_each_row_inside_chat_column_only() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("chat")]);
    app.input.insert_str("aa\nbb\ncc"); // height 3
    assert_eq!(app.input_lines_height(), 3);

    let (_, buf) = render_grid_with_buffer(&mut app);
    for y in input_band(3, buf.area.height) {
        for x in [26u16, 60, 100, 118] {
            assert_eq!(
                buf[(x, y)].bg,
                theme.input_panel_bg,
                "tint missing at ({x},{y})"
            );
        }
        assert_ne!(buf[(25, y)].bg, theme.input_panel_bg, "divider tinted");
        assert_ne!(buf[(0, y)].bg, theme.input_panel_bg, "sidebar tinted");
        assert_ne!(buf[(119, y)].bg, theme.input_panel_bg, "margin tinted");
    }
}

/// Chat transcript surface carries `theme.bg` across the FULL chat pane —
/// top-border row, empty transcript space and the pane's right margin
/// column included — while sidebar/divider keep `theme.panel` and the
/// input zone keeps `theme.input_panel_bg` (the app-painted
/// panel < chat < input ladder, independent of the terminal profile).
#[test]
fn chat_surface_tints_full_column_with_theme_bg() {
    let theme = Theme::tokyo_night();
    // A fresh empty chat: every inner cell is blank transcript space —
    // exactly the surface the terminal background used to show through.
    let mut app = App::new(vec![Chat::new("chat")]);

    let (_, buf) = render_grid_with_buffer(&mut app);
    // Pane spans x 26..=119; the 1-row editor block puts the pane at
    // rows 0..=30 (row 31 spinner, 32 input, 33 hints).
    for (x, y) in [
        (26u16, 0u16),
        (60, 0),
        (119, 0),
        (26, 15),
        (60, 15),
        (119, 15),
        (118, 30),
    ] {
        assert_eq!(
            buf[(x, y)].bg,
            theme.bg,
            "chat surface not tinted at ({x},{y})"
        );
    }
    // Neighbors keep their own surfaces: sidebar + divider panel, input
    // zone panel tint, input-band margin cell untouched by either fill.
    assert_eq!(buf[(10, 15)].bg, theme.panel, "sidebar must stay panel");
    assert_eq!(buf[(25, 15)].bg, theme.panel, "divider must stay panel");
    assert_eq!(
        buf[(60, 32)].bg,
        theme.input_panel_bg,
        "input zone tint lost"
    );
    assert_ne!(
        buf[(119, 32)].bg,
        theme.bg,
        "input margin must not take chat tint"
    );
}

/// Cap + auto-scroll: a 23-line draft caps the block at MAX_INPUT_LINES
/// (20) rows; the viewport keeps the caret's LAST lines visible while
/// the earliest lines scroll out above.
#[test]
fn textarea_view_follows_caret_beyond_twenty_line_cap() {
    let mut app = App::new(vec![Chat::new("chat")]);
    let lines: Vec<String> = (0..23).map(|i| format!("zzq{i:02}")).collect();
    app.input.insert_str(&lines.join("\n"));
    assert_eq!(app.input_lines_height(), crate::app::MAX_INPUT_LINES as u16);

    let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 50);
    let flat = rows.join("\n");

    assert!(
        flat.contains("zzq22"),
        "caret line (last draft line) must be visible"
    );
    assert!(
        flat.contains("zzq21") && flat.contains("zzq03"),
        "trailing viewport window must hold recent lines"
    );
    for lost in ["zzq00", "zzq01", "zzq02"] {
        assert!(!flat.contains(lost), "{lost} should have scrolled out");
    }
}

/// Terminals too small for 20 rows + chrome must not panic: the block is
/// clamped to what fits (spinner + hints + sliver of chat survive) and
/// repeated draws stay stable.
#[test]
fn grown_input_never_panics_on_tiny_terminal() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("chat")]);
    for _ in 0..30 {
        app.input.insert_str("long\n");
    }
    let (rows, buf) = render_grid_at_with_buffer(&mut app, 80, 12);
    assert!(
        rows.last().unwrap().contains("^N"),
        "status hints must survive the clamp"
    );
    // Block clamped to 12-4 = 8 rows: band top lands at H-1-8 = 3.
    for y in 4..=10 {
        assert_eq!(
            buf[(26, y)].bg,
            theme.input_panel_bg,
            "clamped band row {y} must still be tinted"
        );
    }
    // Second draw after viewport state exists — still no panic.
    let _ = render_grid_at_with_buffer(&mut app, 80, 12);
}

/// Rename mode grows equally: a multiline title draft renders its first
/// line with label + hint and continuation lines below, hint only once.
#[test]
fn rename_editor_grows_and_renders_multiline_draft() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.begin_rename();
    for c in "ab\ncd".chars() {
        app.rename_push(c);
    }
    assert_eq!(app.input_lines_height(), 2);

    let rows = render_grid(&mut app);
    let band = input_band(2, rows.len() as u16).collect::<Vec<_>>();
    assert!(
        rows[band[0] as usize].contains("✎ Rename thread…")
            && rows[band[0] as usize].contains("ab")
            && rows[band[0] as usize].contains("esc cancel"),
        "first rename row must keep label/draft/hint: {:?}",
        rows[band[0] as usize]
    );
    assert!(
        rows[band[1] as usize].contains("cd"),
        "continuation line missing: {:?}",
        rows[band[1] as usize]
    );
    assert!(
        !rows[band[1] as usize].contains("esc cancel"),
        "hint must not repeat below the first row"
    );
}
