use super::super::*;
use super::support::*;

/// End-to-end smoke test: at 120x34 every code panel must be a closed
/// rectangle — right border present on EVERY body row, aligned with the
/// corners, never wrapped or clipped by the layout. The scan runs inside
/// the chat pane's INNER area: the pane itself is a rounded rectangle
/// now, so its own border glyphs (corners on rows 0/30, │ on cols
/// 26/119) must not confuse the code-panel search.
#[test]
fn full_ui_code_panels_are_closed_rectangles() {
    // Chat 1 has python panels, chat 2 the widest rust panel.
    let rects = layout_rects(Rect::new(0, 0, 120, 34), 1);
    let inner_x = (rects.chat.x + 1) as usize;
    let inner_w = rects.chat.width.saturating_sub(2) as usize;
    for active in [0usize, 1] {
        let mut app = App::new(mock::initial_chats());
        app.active = active;
        let rows = render_grid(&mut app);
        // Slice every row to the pane's inner columns and keep only the
        // pane's inner rows (each cell is one symbol char, so char
        // indices are cell indices).
        let inner_rows: Vec<String> = rows[1..(rects.chat.bottom() as usize - 1)]
            .iter()
            .map(|r| r.chars().skip(inner_x).take(inner_w).collect())
            .collect();
        let tops: Vec<usize> = inner_rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.contains('\u{256d}'))
            .map(|(y, _)| y)
            .collect();
        assert!(!tops.is_empty(), "chat {active}: no code panel rendered");
        for t in tops {
            let b = (t + 1..inner_rows.len())
                .find(|&y| inner_rows[y].contains('\u{2570}'))
                .unwrap_or_else(|| {
                    panic!("chat {active}: bottom border missing under top at y{t}")
                });
            let rc_top = col_of(&inner_rows[t], '\u{256e}')
                .unwrap_or_else(|| panic!("chat {active}: ╮ missing on top row y{t}"));
            let rc_bot = col_of(&inner_rows[b], '\u{256f}')
                .unwrap_or_else(|| panic!("chat {active}: ╯ missing on bottom row y{b}"));
            assert_eq!(
                rc_top, rc_bot,
                "chat {active} panel y{t}..{b}: right border column drifts"
            );
            for (dy, row) in inner_rows[t + 1..b].iter().enumerate() {
                let y = t + 1 + dy;
                let n = row.matches('\u{2502}').count();
                assert!(
                    n >= 2,
                    "chat {active} body row y{y}: only {n} borders — panel open: {row:?}"
                );
                let right = col_of(row, '\u{2502}').unwrap();
                assert_eq!(
                    right, rc_top,
                    "chat {active} body row y{y}: right border at {right}, expected {rc_top}"
                );
            }
        }
    }
}

// ---- panel outlines (rounded borders on both panes) --------------------

/// The sidebar and the chat pane render FULL rounded outlines: ╭╮╰╯
/// corners on all four corners of each pane, │ sides, ─ tops/bottoms —
/// with the titles still embedded in the top border rows and the outer
/// frame dimensions unchanged (the borders live inside the existing
/// rects; the sidebar's divider column became its right border and the
/// chat header's top-border row became the pane's top border).
#[test]
fn panel_outlines_render_rounded_borders_on_both_panels() {
    let mut app = App::new(vec![Chat::new("chat")]);
    let (rows, _) = render_grid_with_buffer(&mut app);

    // Sidebar: cols 0..=25, rows 0..=30 at 120×34.
    assert_eq!(rows[0].chars().next(), Some('╭'), "sidebar top-left");
    assert_eq!(rows[0].chars().nth(25), Some('╮'), "sidebar top-right");
    assert_eq!(rows[30].chars().next(), Some('╰'), "sidebar bottom-left");
    assert_eq!(rows[30].chars().nth(25), Some('╯'), "sidebar bottom-right");
    // Chat pane: cols 26..=119, rows 0..=30.
    assert_eq!(rows[0].chars().nth(26), Some('╭'), "chat top-left");
    assert_eq!(rows[0].chars().nth(119), Some('╮'), "chat top-right");
    assert_eq!(rows[30].chars().nth(26), Some('╰'), "chat bottom-left");
    assert_eq!(rows[30].chars().nth(119), Some('╯'), "chat bottom-right");
    // Vertical sides on every inner row of both panes.
    for (y, row) in rows.iter().enumerate().take(30).skip(1) {
        let col = |x: usize| row.chars().nth(x);
        assert_eq!(col(0), Some('│'), "sidebar left y{y}");
        assert_eq!(col(25), Some('│'), "sidebar right y{y}");
        assert_eq!(col(26), Some('│'), "chat left y{y}");
        assert_eq!(col(119), Some('│'), "chat right y{y}");
    }
    // Titles still embedded in the top border rows, unchanged.
    assert!(rows[0].contains(" Chats "), "sidebar title missing");
    assert!(rows[0].contains("#1/1"), "chat header title missing");
}

/// The hit-test seam agrees with the rendered outlines: the seam rects
/// are unchanged by the outlines, every rendered border cell sits inside
/// the panel the router routes its column to, and a wheel event over the
/// sidebar column still routes to the sidebar with the new rects
/// (layout_rects/panel_region never moved).
#[test]
fn panel_outlines_keep_the_hit_test_seam_consistent() {
    let mut app = App::new(vec![Chat::new("chat")]);
    let (_, buf) = render_grid_with_buffer(&mut app);
    let rects = layout_rects(Rect::new(0, 0, 120, 34), 1);

    // The seam rects are byte-identical to the pre-outline geometry.
    assert_eq!(rects.sidebar, Rect::new(0, 0, 26, 31));
    assert_eq!(rects.chat, Rect::new(26, 0, 94, 31));

    // Every rendered border cell belongs to the panel its column
    // hit-tests to: sidebar borders (cols 0/25) → Sidebar, chat borders
    // (cols 26/119) → Chat — rendering and routing can never disagree.
    for y in 1..30u16 {
        for x in [0u16, 25] {
            assert_eq!(buf[(x, y)].symbol(), "│", "sidebar border at ({x},{y})");
            assert_eq!(
                panel_region(&rects, x, y),
                PanelRegion::Sidebar,
                "column {x} must route to the sidebar"
            );
        }
        for x in [26u16, 119] {
            assert_eq!(buf[(x, y)].symbol(), "│", "chat border at ({x},{y})");
            assert_eq!(
                panel_region(&rects, x, y),
                PanelRegion::Chat,
                "column {x} must route to the chat pane"
            );
        }
    }
}
