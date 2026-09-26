use super::super::*;
use super::support::*;

// cwd + model strip -------------------------------

/// Default contract: the strip is VISIBLE — the chat header border
/// carries the `cwd:` readout on startup; ^O toggles it off.
#[test]
fn status_strip_is_visible_by_default() {
    let mut app = App::new(mock::initial_chats());
    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains("cwd:"),
        "strip must be visible by default: {:?}",
        rows[0]
    );
}

/// Visible by default: the readout rides the SAME top-border row as the chat
/// header title — right-aligned into the pane's last columns and
/// dim-styled (theme-driven). Mock chats carry no model labels and the
/// workspace root is unwired here, so both segments show `—`.
#[test]
fn status_strip_renders_right_aligned_and_dim_on_the_header_border() {
    let mut app = App::new(mock::initial_chats());
    let theme = Theme::tokyo_night();
    // Zero vertical cost: render with the strip visible (the default),
    // then toggle it off — ONLY the header border row (row 0) may
    // change; every other row is untouched, proving the strip never
    // steals a content row.
    let (before, _) = render_grid_with_buffer(&mut app);
    app.toggle_status_strip();
    let (rows, _) = render_grid_with_buffer(&mut app);
    for (y, (b, a)) in before.iter().zip(rows.iter()).enumerate().skip(1) {
        assert_eq!(b, a, "row {y} must be untouched by the strip");
    }

    app.toggle_status_strip();
    let (rows, buf) = render_grid_with_buffer(&mut app);
    let header = &rows[0];
    let text = "cwd: \u{2014} \u{00b7} \u{2014}";
    assert!(header.contains(text), "strip text missing: {header:?}");
    // Right-aligned: the readout's last column is the chat pane's last
    // INNER column (118 @120, right of it the rounded border), i.e. it
    // starts at 118 - 10 + 1 = 109.
    let start = col_of_sub(header, text).expect("strip column");
    assert_eq!(start, 109, "strip not right-aligned: {header:?}");
    // Dim styling: every cell of the readout uses theme.dim (not the
    // bold header blue, not the border selection color).
    for (i, _) in text.chars().enumerate() {
        assert_eq!(
            buf[((start + i) as u16, 0)].fg,
            theme.dim,
            "strip cell {i} not dim"
        );
    }
}

/// Model segment reuses the model-label metadata: the active
/// chat's LAST KNOWN label is shown; a chat without any label shows the
/// `—` placeholder (live switching re-labels per chat).
#[test]
fn status_strip_shows_active_chats_last_model() {
    let mut app = App::new(mock::initial_chats());
    app.chats[0]
        .messages
        .push(Message::assistant_with_model("labelled answer", "glm-5.2"));

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains("cwd: \u{2014} \u{00b7} glm-5.2"),
        "model label missing: {:?}",
        rows[0]
    );

    // Switch to a chat with no labels: placeholder returns.
    app.select_next();
    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains("\u{00b7} \u{2014}"),
        "unlabelled chat must show the placeholder: {:?}",
        rows[0]
    );
}

/// Sticky display state at render level: an unlabelled answer (command
/// result) appended AFTER a labelled one must not change the strip, and
/// the ctx segment rides on the sticky last-known usage.
#[test]
fn status_strip_keeps_last_known_model_and_ctx_after_command_answer() {
    let mut app = App::new(mock::initial_chats());
    app.chats[0]
        .messages
        .push(Message::assistant_with_model("labelled answer", "glm-5.2"));
    app.last_turn_usage = Some(Usage {
        input_tokens: 18432,
        output_tokens: 512,
        context_window: Some(131_072),
    });

    // A command answer arrives: model-less message, no new usage.
    app.chats[0].messages.push(Message::assistant("done"));

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains("cwd: \u{2014} \u{00b7} glm-5.2"),
        "model must stay last-known: {:?}",
        rows[0]
    );
    assert!(
        rows[0].contains(" \u{00b7} ctx 14% (18.4k/131.0k)"),
        "ctx must stay last-known: {:?}",
        rows[0]
    );
}

/// Long workspace path: the readout is truncated to the space left of
/// the header title with a single-column ellipsis — no overflow, left
/// title intact, no collision.
#[test]
fn status_strip_truncates_long_paths_with_ellipsis() {
    let mut app = App::new(mock::initial_chats());
    app.workspace_root = Some(format!("/tmp/{}", "w".repeat(120)));

    let rows = render_grid(&mut app);
    let header = &rows[0];
    assert!(
        header.contains("#1/4"),
        "left header title must survive: {header:?}"
    );
    assert!(
        header.contains('\u{2026}'),
        "ellipsis missing from truncated strip: {header:?}"
    );
    assert!(
        !header.contains(&"w".repeat(120)),
        "untruncated path leaked into the row: {header:?}"
    );
    assert!(
        header.trim_end().width() <= 120,
        "header row overflowed: {} cols",
        header.trim_end().width()
    );
}

/// The cwd segment shows the last three path components with a leading
/// `/` (path-tail format), not the bare basename.
#[test]
fn status_strip_shows_the_cwd_path_tail() {
    let mut app = App::new(mock::initial_chats());
    app.workspace_root = Some("/Users/sergio/Develop/personal/chibi-tui".into());

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains("cwd: /Develop/personal/chibi-tui"),
        "cwd tail missing: {:?}",
        rows[0]
    );
}

/// Under width pressure the tail is cut from the LEFT with a leading
/// `…` while the working directory itself and the model readout
/// survive (the strip is right-aligned, so its tail end is the part
/// worth keeping).
#[test]
fn status_strip_left_truncates_the_cwd_tail_but_keeps_the_directory() {
    let mut app = App::new(mock::initial_chats());
    app.workspace_root = Some(format!("/Users/sergio/{}/personal/chibi", "d".repeat(100)));

    let rows = render_grid(&mut app);
    let header = &rows[0];
    assert!(header.contains('\u{2026}'), "ellipsis missing: {header:?}");
    assert!(
        header.contains("/personal/chibi"),
        "working directory lost: {header:?}"
    );
    assert!(
        !header.contains(&"d".repeat(100)),
        "untruncated tail leaked into the row: {header:?}"
    );
    assert!(
        !header.contains("/sergio/"),
        "tail must be cut from the left, not the right: {header:?}"
    );
    assert!(
        header.contains("\u{00b7} \u{2014}"),
        "model readout must survive the squeeze: {header:?}"
    );
    assert!(
        header.trim_end().width() <= 120,
        "header row overflowed: {} cols",
        header.trim_end().width()
    );
}

// ctx usage segment in the strip -----------------------------

/// Human token formatting: raw digits under 1000, `x.xk` under a
/// million, else `x.xM`; tenths truncate (never round up).
#[test]
fn human_tokens_format_boundaries() {
    assert_eq!(human_tokens(0), "0");
    assert_eq!(human_tokens(999), "999");
    assert_eq!(human_tokens(1000), "1.0k");
    assert_eq!(human_tokens(18432), "18.4k");
    assert_eq!(human_tokens(999_999), "999.9k");
    assert_eq!(human_tokens(1_050_000), "1.0M");
    assert_eq!(human_tokens(1_310_720), "1.3M");
}

/// Known window: pct is input tokens against the window (floored) and
/// both counts render in human format.
#[test]
fn status_strip_shows_context_usage_with_known_window() {
    let mut app = App::new(mock::initial_chats());
    app.last_turn_usage = Some(Usage {
        input_tokens: 18432,
        output_tokens: 512,
        context_window: Some(131_072),
    });

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains(" \u{00b7} ctx 14% (18.4k/131.0k)"),
        "ctx segment missing or malformed: {:?}",
        rows[0]
    );
}

/// Unknown window (and the zero-window degenerate): absolute count
/// only — no pct, no invented max.
#[test]
fn status_strip_shows_absolute_usage_when_window_unknown() {
    let mut app = App::new(mock::initial_chats());
    app.last_turn_usage = Some(Usage {
        input_tokens: 18432,
        output_tokens: 512,
        context_window: None,
    });

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains(" \u{00b7} ctx 18.4k"),
        "absolute usage missing: {:?}",
        rows[0]
    );
    assert!(
        !rows[0].contains('%'),
        "no pct without a window: {:?}",
        rows[0]
    );

    let zero_window = Usage {
        input_tokens: 999,
        output_tokens: 0,
        context_window: Some(0),
    };
    assert_eq!(
        context_usage_segment(&zero_window),
        " \u{00b7} ctx 999",
        "zero window must degrade to absolute"
    );
}

/// No usage (fresh app, cleared mid-request, old backend): the segment
/// vanishes entirely — the strip carries only `cwd` and `model`.
#[test]
fn status_strip_omits_the_usage_segment_without_usage() {
    let mut app = App::new(mock::initial_chats());

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains("cwd:"),
        "strip itself must render: {:?}",
        rows[0]
    );
    assert!(
        !rows[0].contains("ctx"),
        "usage leaked without data: {:?}",
        rows[0]
    );
}

/// Turning usage on/off changes the strip ONLY by the segment: same
/// `cwd` + `model` text, segment appended at its tail (the border-dash
/// fill shrinks to make room — the strip is right-aligned). The rows
/// compared are title rows on the pane's top border, so the rounded
/// right-corner glyph rides at the tail of either readout; border
/// drawing chars are stripped first — the assertion is about the
/// payload, not where the corner lands.
#[test]
fn strip_with_usage_differs_from_without_only_by_the_segment() {
    let payload = |row: &str| {
        row[row.find("cwd:").unwrap()..]
            .chars()
            .filter(|c| {
                !matches!(
                    c,
                    '\u{2500}' | '\u{2502}' | '\u{256d}' | '\u{256e}' | '\u{2570}' | '\u{256f}'
                )
            })
            .collect::<String>()
    };

    let mut app = App::new(mock::initial_chats());
    let (rows, _) = render_grid_with_buffer(&mut app);
    let without = payload(&rows[0]);

    app.last_turn_usage = Some(Usage {
        input_tokens: 18432,
        output_tokens: 512,
        context_window: Some(131_072),
    });
    let (rows, _) = render_grid_with_buffer(&mut app);
    let with = payload(&rows[0]);
    assert_eq!(
        with,
        format!("{without} \u{00b7} ctx 14% (18.4k/131.0k)"),
        "strip must differ only by the ctx segment"
    );
}

/// Ctrl+O parity: the segment rides INSIDE the existing strip — shown
/// by default together with the strip, hidden once ^O turns it off.
#[test]
fn usage_segment_inherits_ctrl_o_visibility() {
    let mut app = App::new(mock::initial_chats());
    app.last_turn_usage = Some(Usage {
        input_tokens: 18432,
        output_tokens: 512,
        context_window: Some(131_072),
    });

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains("ctx"),
        "usage rides the visible strip by default: {:?}",
        rows[0]
    );

    app.toggle_status_strip();
    let rows = render_grid(&mut app);
    assert!(
        !rows[0].contains("ctx"),
        "usage must hide together with the strip: {:?}",
        rows[0]
    );
}

/// A tight budget reads `…onal/chibi`, the working
/// directory itself never falls under the cut; a roomy budget leaves
/// the tail untouched.
#[test]
fn left_truncate_ellipsis_cuts_from_the_left() {
    assert_eq!(
        left_truncate_ellipsis("/Develop/personal/chibi", 11),
        "\u{2026}onal/chibi"
    );
    assert_eq!(
        left_truncate_ellipsis("/Develop/personal/chibi", 40),
        "/Develop/personal/chibi"
    );
}

/// Too-narrow chat pane: the strip yields entirely instead of colliding
/// with the header title (no panic, no readout, no overflow).
#[test]
fn status_strip_skips_when_the_chat_pane_cannot_host_it() {
    let mut app = App::new(mock::initial_chats());
    let (rows, _) = render_grid_at_with_buffer(&mut app, 40, 20);
    assert!(
        !rows[0].contains("cwd:"),
        "strip must not render in a too-narrow pane: {:?}",
        rows[0]
    );
    assert!(
        rows[0].trim_end().width() <= 40,
        "narrow frame overflowed: {} cols",
        rows[0].trim_end().width()
    );
}
