use super::super::*;
use super::support::*;

fn unique_marker(tag: &str) -> String {
    format!(
        "ui-test-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// The `log*` unseen-lines marker appears on the status line (dim token
/// after the hints) when the viewer is closed and unseen lines exist —
/// and the whole row still fits the 120-col discipline with the LONGEST
/// status label (`● disconnected (press R)`).
#[test]
fn log_marker_shows_on_status_line_and_fits_120_cols() {
    let marker = unique_marker("unseen");
    crate::diag::append(&marker);

    let mut app = App::new(mock::initial_chats()); // popup-free state
    app.connection = crate::app::Connection::Disconnected;
    let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 34);
    let status_row = rows.last().expect("status row exists");

    assert!(
        status_row.contains("log*"),
        "unseen marker must show: {status_row:?}"
    );
    let width: usize = UnicodeWidthStr::width(status_row.as_str());
    assert!(
        width <= 120,
        "status row must fit 120 cols with the marker + longest label, got {width}: {status_row:?}"
    );
}

/// The marker hides while the log viewer modal is open (the user is
/// looking at the stream) — mode-gated, so no global-state racing.
#[test]
fn log_marker_hidden_while_viewer_modal_is_open() {
    let mut app = App::new(mock::initial_chats());
    app.begin_log_viewer();
    let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 34);
    let status_row = rows.last().expect("status row exists");
    assert!(
        !status_row.contains("log*"),
        "marker must hide while the viewer is open: {status_row:?}"
    );
}

/// The viewer modal renders buffered lines (mono view) with the closing
/// hint; live-tail shows the newest content.
#[test]
fn log_viewer_modal_renders_lines_and_hint() {
    let marker = unique_marker("modal");
    crate::diag::append(&marker);

    let mut app = App::new(mock::initial_chats());
    app.begin_log_viewer();
    let flat = render_grid(&mut app).join("\n");

    assert!(flat.contains("Diagnostics log"), "title: {flat}");
    assert!(
        flat.contains(&marker),
        "buffered line visible in the modal: {flat}"
    );
    assert!(
        flat.contains("Esc close") && flat.contains("PgUp/PgDn page"),
        "footer hint visible: {flat}"
    );
    // Header state: position + tail + wrap state
    // ride the footer row.
    assert!(
        flat.contains("live") && flat.contains("wrap: off"),
        "header shows tail and wrap state: {flat}"
    );
    // At the bottom: no "+K new lines" (nothing arrived since the open).
    assert!(
        !flat.contains("new lines"),
        "no +K hint at the live tail: {flat}"
    );
}

/// Pinned (cursor above the tail) with arrivals pending: the frozen view
/// keeps its position and the footer counts the new lines
/// (`+K new lines`).
#[test]
fn log_viewer_modal_shows_plus_k_hint_when_detached() {
    let mut app = App::new(mock::initial_chats());
    for i in 0..12 {
        crate::diag::append(format!("filler-{i}"));
    }
    app.begin_log_viewer();
    app.log_cursor_up(3);
    assert!(
        match &app.mode {
            Mode::LogViewer { state } => !state.at_tail(),
            _ => false,
        },
        "precondition: pinned"
    );

    let marker = unique_marker("arrived");
    crate::diag::append(&marker);

    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains("new lines"),
        "+K footer must show while pinned with arrivals: {flat}"
    );
    assert!(
        !flat.contains(&marker),
        "frozen snapshot must NOT show lines that arrived while pinned: {flat}"
    );

    // Re-arming the tail (G / cursor back to the newest line) refreshes
    // the snapshot and clears the hint.
    app.log_jump_bottom();
    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains(&marker),
        "live-tail shows the arrival: {flat}"
    );
    assert!(
        !flat.contains("new lines"),
        "hint clears at the bottom: {flat}"
    );
}

/// `[tui]` lifecycle events render (so the unified stream is visible);
/// long stderr lines are right-truncated instead of wrapping (row math):
/// the line HEAD is visible, the tail is cut, and the content never
/// spills onto a second row.
#[test]
fn log_viewer_renders_tui_events_and_truncates_long_lines() {
    crate::diag::append_tui("spawn `chibi` (pid 4242)");
    let long = format!("LONGHEAD-{}", "x".repeat(300));
    crate::diag::append(&long);

    let mut app = App::new(mock::initial_chats());
    app.begin_log_viewer();
    let flat = render_grid(&mut app).join("\n");

    assert!(
        flat.contains("[tui] spawn `chibi` (pid 4242)"),
        "[tui] event visible: {flat}"
    );
    assert!(
        flat.contains("LONGHEAD-"),
        "long line reaches the modal: {flat}"
    );
    // No wrap: the 300-x tail must NOT reappear on any row (truncated).
    assert!(
        !flat.contains(&"x".repeat(150)),
        "long line must be truncated, never wrapped: {flat}"
    );
}

// header state, wrap, level colors -----------

use crate::app::LogViewerState;

/// Hand-built viewer state for the hermetic render tests below (the
/// global diag stream is shared by parallel tests and would race).
/// Lines go through the same ingestion parse as real arrivals, so the
/// tests exercise the production level attribution. The
/// The search fields default to off.
fn viewer_state(cursor: usize, wrap: bool, lines: Vec<String>) -> LogViewerState {
    LogViewerState {
        cursor,
        wrap,
        row_offset: 0,
        lines: lines
            .into_iter()
            .map(crate::diag::LogEntry::parse)
            .collect(),
        snapshot_total: crate::diag::total_appended(),
        search_buf: None,
        search: None,
        copy_note: None,
        copy_note_at: None,
    }
}

/// The header (border title) carries the cursor position, the tail
/// state and the wrap toggle; stepping the cursor up flips live →
/// pinned, `w` flips wrap: off → on.
#[test]
fn log_viewer_header_shows_position_tail_and_wrap() {
    let mut app = App::new(mock::initial_chats());
    for i in 0..5 {
        crate::diag::append(format!("filler-{i}"));
    }
    app.begin_log_viewer();

    let rows = render_grid(&mut app);
    let title = rows
        .iter()
        .find(|r| r.contains("Diagnostics log"))
        .expect("viewer title row exists");
    assert!(title.contains("line "), "position shown: {title}");
    assert!(title.contains("live"), "tail state shown: {title}");
    assert!(title.contains("wrap: off"), "wrap state shown: {title}");

    // Two cursor steps up: the header must flip to pinned.
    app.log_cursor_up(2);
    let rows = render_grid(&mut app);
    let title = rows
        .iter()
        .find(|r| r.contains("Diagnostics log"))
        .expect("viewer title row exists");
    assert!(title.contains("pinned"), "pinned state shown: {title}");
    assert!(!title.contains(" live "), "no live while pinned: {title}");

    // `w` toggles wrap and the header follows.
    app.log_toggle_wrap();
    let rows = render_grid(&mut app);
    let title = rows
        .iter()
        .find(|r| r.contains("Diagnostics log"))
        .expect("viewer title row exists");
    assert!(title.contains("wrap: on"), "wrap on shown: {title}");
}

/// Wrap off keeps the compact truncated timeline (the old contract);
/// wrap on reflows the SAME logical line over several rows so its tail
/// becomes readable. Hermetic: hand-built pinned state, so the global
/// diag stream (shared by parallel tests) cannot race the render.
#[test]
fn log_viewer_wrap_toggle_reflows_long_lines() {
    let long = format!("WRAPHEAD-{}", "y".repeat(200));
    let mk_state = |wrap: bool| {
        viewer_state(
            1,
            wrap,
            vec![
                "head filler".to_owned(),
                long.clone(),
                "tail filler".to_owned(),
            ],
        )
    };

    let mut app = App::new(mock::initial_chats());
    app.mode = Mode::LogViewer {
        state: mk_state(false),
    };
    let flat = render_grid(&mut app).join("\n");
    assert!(
        !flat.contains(&"y".repeat(150)),
        "wrap off: long line stays truncated, never reflowed: {flat}"
    );

    app.mode = Mode::LogViewer {
        state: mk_state(true),
    };
    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains(&"y".repeat(100)),
        "wrap on: the line tail is reflowed into view: {flat}"
    );
}

/// Levels parsed at ingestion colorize per the log_line_style table:
/// TRACE very dim, DEBUG dim gray, INFO/SUCCESS green, WARNING yellow,
/// ERROR red, CRITICAL bold red. Checked against the actual rendered
/// cells. The viewer state is built by hand (pinned, so the render
/// never refreshes it): the global diag stream is shared by parallel
/// tests and would race.
#[test]
fn log_viewer_colors_levels_per_mapping() {
    let theme = Theme::tokyo_night();
    let cases: [(&str, &str, ratatui::style::Color, bool); 7] = [
        ("LVC-TRACE", "TRACE", theme.log_trace, false),
        ("LVC-DEBUG", "DEBUG", theme.log_debug, false),
        ("LVC-INFO", "INFO", theme.green, false),
        ("LVC-WARN", "WARNING", theme.yellow, false),
        ("LVC-ERROR", "ERROR", theme.red, false),
        ("LVC-CRIT", "CRITICAL", theme.red, true),
        ("LVC-OK", "SUCCESS", theme.green, false),
    ];
    let lines: Vec<String> = cases
        .iter()
        .map(|(marker, level, _, _)| {
            format!("2026-09-01 10:00:00.000 | {level} | chibi.m:1 - body {marker}")
        })
        .collect();

    let mut app = App::new(mock::initial_chats());
    app.mode = Mode::LogViewer {
        // Pinned to a middle line: the snapshot stays frozen.
        state: viewer_state(3, false, lines),
    };
    let (rows, buf) = render_grid_with_buffer(&mut app);

    for (marker, _, expected, bold) in &cases {
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains(marker))
            .unwrap_or_else(|| panic!("level line {marker} visible in the viewer"));
        let col = col_of_sub(row, marker).expect("marker column");
        let cell = &buf[(col as u16, y as u16)];
        assert_eq!(cell.fg, *expected, "level color for {marker}");
        assert_eq!(
            cell.modifier.contains(Modifier::BOLD),
            *bold,
            "bold flag for {marker}"
        );
    }
}

/// The backend's custom levels colorize per their registration colors
/// routed through the theme slots: TOOL light-blue → blue, THINK
/// light-magenta / CALL magenta → purple, CHECK/MODERATOR light-red →
/// red, SUBAGENT cyan → cyan, DELEGATE blue → blue. Rendered-cell
/// check, same technique as the standard levels above.
#[test]
fn log_viewer_colors_custom_levels_per_mapping() {
    let theme = Theme::tokyo_night();
    let cases: [(&str, &str, ratatui::style::Color); 7] = [
        ("LVC-TOOL", "TOOL", theme.blue),
        ("LVC-THINK", "THINK", theme.purple),
        ("LVC-CALL", "CALL", theme.purple),
        ("LVC-CHECK", "CHECK", theme.red),
        ("LVC-MODERATOR", "MODERATOR", theme.red),
        ("LVC-SUBAGENT", "SUBAGENT", theme.cyan),
        ("LVC-DELEGATE", "DELEGATE", theme.blue),
    ];
    let lines: Vec<String> = cases
        .iter()
        .map(|(marker, level, _)| {
            format!("2026-09-01 10:00:00 | {level} | chibi.m:1 - body {marker}")
        })
        .collect();

    let mut app = App::new(mock::initial_chats());
    app.mode = Mode::LogViewer {
        // Pinned to a middle line: the snapshot stays frozen.
        state: viewer_state(3, false, lines),
    };
    let (rows, buf) = render_grid_with_buffer(&mut app);

    for (marker, _, expected) in &cases {
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains(marker))
            .unwrap_or_else(|| panic!("custom level line {marker} visible in the viewer"));
        let col = col_of_sub(row, marker).expect("marker column");
        let cell = &buf[(col as u16, y as u16)];
        assert_eq!(cell.fg, *expected, "custom level color for {marker}");
        assert!(
            !cell.modifier.contains(Modifier::BOLD),
            "custom levels carry no extra modifiers: {marker}"
        );
    }
}

/// DoD: every custom level resolves to a REAL theme slot — not the
/// level-less fallback — in EVERY bundled theme, so a future theme that
/// misses a slot fails here instead of shipping an uncolored tier.
#[test]
fn log_line_style_resolves_custom_levels_to_slots_in_every_bundled_theme() {
    use crate::diag::LogLevel;
    for theme in Theme::bundled() {
        let cases = [
            (LogLevel::Tool, theme.blue),
            (LogLevel::Think, theme.purple),
            (LogLevel::Call, theme.purple),
            (LogLevel::Check, theme.red),
            (LogLevel::Moderator, theme.red),
            (LogLevel::Subagent, theme.cyan),
            (LogLevel::Delegate, theme.blue),
        ];
        for (level, slot) in cases {
            let entry = crate::diag::LogEntry {
                text: "t".to_owned(),
                level: Some(level),
            };
            let style = log_line_style(&entry, &theme);
            assert_eq!(
                style.fg,
                Some(slot),
                "{level:?} must take its theme slot in every bundled theme"
            );
            assert_ne!(
                style.fg,
                Some(theme.fg),
                "{level:?} must not fall through to the level-less default"
            );
        }
    }
}

/// Unknown level names stay graceful at the style-table level too: the
/// level parses to `None` and renders the default foreground — no
/// panic, no raw leak, no invented color.
#[test]
fn log_line_style_falls_back_to_default_fg_for_unknown_levels() {
    let theme = Theme::tokyo_night();
    for token in ["NOTALEVEL", "TOOOL", "tool2", ""] {
        let entry = crate::diag::LogEntry::parse(format!("2026-09-01 10:00:00 | {token} | body"));
        assert_eq!(entry.level, None, "unknown token {token:?} parses to None");
        assert_eq!(
            log_line_style(&entry, &theme).fg,
            Some(theme.fg),
            "unknown level {token:?} keeps the default foreground"
        );
    }
}

/// Graceful degradation: lines without a recognizable ` | LEVEL | `
/// field (old backends, malformed output, plain stderr noise) render in
/// the default foreground exactly as before, and `[tui]` lifecycle
/// events stay dim.
#[test]
fn log_viewer_unknown_lines_render_default_and_tui_events_stay_dim() {
    let theme = Theme::tokyo_night();
    let lines = vec![
        "plain stderr noise".to_owned(),
        "2026-09-01 10:00:00.000 | NOTALEVEL | chibi.m:1 - body".to_owned(),
        "a | INFO".to_owned(),
        "[tui] handshake ok (protocol v1)".to_owned(),
    ];

    let mut app = App::new(mock::initial_chats());
    app.mode = Mode::LogViewer {
        state: viewer_state(1, false, lines),
    };
    let (rows, buf) = render_grid_with_buffer(&mut app);

    for marker in ["plain stderr noise", "NOTALEVEL", "a | INFO"] {
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains(marker))
            .unwrap_or_else(|| panic!("line {marker:?} visible in the viewer"));
        let col = col_of_sub(row, marker).expect("marker column");
        assert_eq!(
            buf[(col as u16, y as u16)].fg,
            theme.fg,
            "default foreground for {marker:?}"
        );
    }

    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.contains("handshake ok"))
        .expect("tui event visible in the viewer");
    let col = col_of_sub(row, "handshake ok").expect("marker column");
    assert_eq!(
        buf[(col as u16, y as u16)].fg,
        theme.dim,
        "[tui] events stay dim"
    );
}

// search + copy ------------------------

/// Every occurrence of the pattern lights up in the log_match slot, and
/// the line the cursor's current hit sits on is emphasized (reversed on
/// top of the slot). Non-match text keeps its level color. Hand-built
/// pinned state, so the shared diag stream cannot race the render.
#[test]
fn log_viewer_search_highlights_matches_and_current() {
    use crate::app::LogSearch;
    let theme = Theme::tokyo_night();
    let mut app = App::new(mock::initial_chats());
    app.mode = Mode::LogViewer {
        state: viewer_state(
            0,
            false,
            vec![
                "one ALPHA mid".to_owned(),
                "plain middle".to_owned(),
                "three alpha end".to_owned(),
            ],
        ),
    };
    if let Mode::LogViewer { state } = &mut app.mode {
        state.search = Some(LogSearch {
            pattern: "alpha".to_owned(),
            matches: vec![0, 2],
            current: Some(0),
        });
    }
    let (rows, buf) = render_grid_with_buffer(&mut app);

    // Current hit line: the match cells take the slot AND are reversed.
    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.contains("ALPHA"))
        .expect("current match line visible");
    let col = col_of_sub(row, "ALPHA").expect("match column");
    for dx in 0..5usize {
        let cell = &buf[((col + dx) as u16, y as u16)];
        assert_eq!(cell.fg, theme.log_match, "match slot fg at dx={dx}");
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "current match emphasized at dx={dx}"
        );
    }
    let before = &buf[((col - 1) as u16, y as u16)];
    assert_ne!(before.fg, theme.log_match, "text before the hit untouched");

    // The other hit line: same slot, no reversed emphasis.
    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.contains("alpha"))
        .expect("second match line visible");
    let col = col_of_sub(row, "alpha").expect("match column");
    let cell = &buf[(col as u16, y as u16)];
    assert_eq!(cell.fg, theme.log_match);
    assert!(
        !cell.modifier.contains(Modifier::REVERSED),
        "non-current hits stay plain slot color"
    );
}

/// A hit past the first wrap chunk still highlights: the search runs on
/// the logical line and the ranges are cut into the reflowed rows at
/// their char offsets.
#[test]
fn log_viewer_search_highlight_survives_wrap_boundary() {
    use crate::app::LogSearch;
    let theme = Theme::tokyo_night();
    // 110 x's push `alpha` past the first chunk at the demo width.
    let long = format!("{}alpha tail", "x".repeat(110));
    let mut app = App::new(mock::initial_chats());
    // Pinned (cursor 0 of 2 lines): the snapshot never refreshes.
    app.mode = Mode::LogViewer {
        state: viewer_state(0, true, vec![long, "tail filler".to_owned()]),
    };
    if let Mode::LogViewer { state } = &mut app.mode {
        state.search = Some(LogSearch {
            pattern: "alpha".to_owned(),
            matches: vec![0],
            current: Some(0),
        });
    }
    let (rows, buf) = render_grid_with_buffer(&mut app);

    // The reflow split the line: chunk rows exist, and the row that
    // carries the hit highlights exactly the pattern cells.
    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.contains("alpha"))
        .expect("the second chunk shows the tail");
    let col = col_of_sub(row, "alpha").expect("match column in the wrapped row");
    for dx in 0..5usize {
        let cell = &buf[((col + dx) as u16, y as u16)];
        assert_eq!(
            cell.fg, theme.log_match,
            "highlight crossed the wrap at dx={dx}"
        );
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "current hit emphasis survives the reflow at dx={dx}"
        );
    }
    let before = &buf[((col - 1) as u16, y as u16)];
    assert_ne!(
        before.fg, theme.log_match,
        "padding x before the hit untouched"
    );
}

/// Header carries the whole search/copy state in one line: plain count
/// before the first n, `k/N` while navigating, the open prompt echo,
/// and the copy feedback (both flavors).
#[test]
fn log_viewer_header_shows_search_and_copy_states() {
    use crate::app::LogSearch;
    let mk = |cursor: usize, lines: Vec<String>| {
        let mut app = App::new(mock::initial_chats());
        app.mode = Mode::LogViewer {
            state: viewer_state(cursor, false, lines),
        };
        app
    };
    let title_of = |app: &mut App| {
        render_grid(app)
            .into_iter()
            .find(|r| r.contains("Diagnostics log"))
            .expect("viewer title row exists")
    };

    // Committed, nothing selected yet: bare count. A 4th line keeps the
    // cursor off the tail after n, so the hand-built snapshot never
    // gets replaced by the live refresh.
    let mut app = mk(
        0,
        vec![
            "alpha one".to_owned(),
            "filler".to_owned(),
            "two alpha".to_owned(),
            "end filler".to_owned(),
        ],
    );
    if let Mode::LogViewer { state } = &mut app.mode {
        state.search = Some(LogSearch {
            pattern: "alpha".to_owned(),
            matches: vec![0, 2],
            current: None,
        });
    }
    let title = title_of(&mut app);
    assert!(title.contains("matches: 2"), "bare count: {title}");
    assert!(
        !title.contains("matches: 2/"),
        "no position before n: {title}"
    );

    // After n: the cursor sat on hit #1 already, so n moves to hit #2.
    app.log_search_next();
    let title = title_of(&mut app);
    assert!(title.contains("matches: 2/2"), "navigating count: {title}");

    // Open prompt: pattern echo in the header, prompt line at the
    // bottom instead of the hint row.
    app.log_open_search();
    app.log_search_push('a');
    app.log_search_push('l');
    let rows = render_grid(&mut app);
    let title = rows
        .iter()
        .find(|r| r.contains("Diagnostics log"))
        .expect("title row");
    assert!(title.contains("search: /al"), "prompt echo: {title}");
    let footer = rows
        .iter()
        .find(|r| r.contains("Enter commit"))
        .expect("prompt line at the bottom");
    assert!(footer.contains("/al"), "prompt shows the buffer: {footer}");

    // Copy feedback rides the header too, both flavors.
    let mut app = mk(0, vec!["a".to_owned(), "b".to_owned()]);
    if let Mode::LogViewer { state } = &mut app.mode {
        state.copy_note = Some("copied".to_owned());
        state.copy_note_at = Some(std::time::Instant::now());
    }
    let title = title_of(&mut app);
    assert!(title.contains("\u{00b7} copied"), "success note: {title}");

    let mut app = mk(0, vec!["a".to_owned(), "b".to_owned()]);
    if let Mode::LogViewer { state } = &mut app.mode {
        state.copy_note = Some("copy: unavailable".to_owned());
        state.copy_note_at = Some(std::time::Instant::now());
    }
    let title = title_of(&mut app);
    assert!(title.contains("copy: unavailable"), "failure note: {title}");

    // The hint row advertises the new keys when nothing is open.
    let mut app = mk(0, vec!["a".to_owned(), "b".to_owned()]);
    let rows = render_grid(&mut app);
    assert!(
        rows.iter().any(|r| r.contains("/ search")),
        "hint mentions search"
    );
    assert!(
        rows.iter().any(|r| r.contains("y copy")),
        "hint mentions copy"
    );
}
