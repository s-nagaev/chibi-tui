use super::super::*;

// ---- layout rects / panel region (mouse hit-testing) ----

#[test]
fn layout_rects_matches_the_draw_layout() {
    let rects = layout_rects(Rect::new(0, 0, 120, 40), 1);
    // Sidebar is the 26-col left column of the main area; the chat
    // pane takes the rest of the width.
    assert_eq!(rects.sidebar.width, 26);
    assert_eq!(rects.chat.x, 26);
    assert_eq!(rects.chat.width, 120 - 26);
    // Root stack: main area of 37 rows, then spinner / input / hints
    // rows of one row each at the bottom.
    assert_eq!(rects.root[0].height, 37);
    assert_eq!(rects.root[1].height, 1);
    assert_eq!(rects.root[2].height, 1);
    assert_eq!(rects.root[3].height, 1);
    assert_eq!(rects.root[3].y, 39);
}

#[test]
fn layout_rects_clamps_the_input_block_on_tiny_frames() {
    // 6-row frame with a 10-row draft: the clamp keeps spinner + hints
    // + a sliver of chat, so the block collapses to 2 rows and the
    // chat pane absorbs what remains.
    let rects = layout_rects(Rect::new(0, 0, 120, 6), 10);
    assert_eq!(rects.root[2].height, 2);
    assert_eq!(rects.root[0].height, 2);
    // A 1-row frame never underflows.
    let rects = layout_rects(Rect::new(0, 0, 40, 1), 1);
    assert_eq!(rects.root[2].height, 1);
}

/// The growth cap is `min(frame_height - 4, 3/4 * frame_height)`, floored at
/// one row. Tall frames are limited by the 3/4 rule, frames at or below 16
/// rows by the tiny-frame guard.
#[test]
fn max_input_block_height_applies_the_three_quarters_cap() {
    // Tall frame: 3/4 wins (40*3/4 = 30 < 40-4 = 36).
    assert_eq!(max_input_block_height(40), 30);
    assert_eq!(max_input_block_height(34), 25);
    // The exact crossover: at 16 both rules agree on 12.
    assert_eq!(max_input_block_height(16), 12);
    // Below the crossover the tiny-frame guard (h-4) wins.
    assert_eq!(max_input_block_height(12), 8);
    assert_eq!(max_input_block_height(8), 4);
    // Saturating floor: tiny frames never underflow below one row.
    assert_eq!(max_input_block_height(4), 1);
    assert_eq!(max_input_block_height(1), 1);
}

/// `LayoutRects::input` re-applies the same cap, so a RAW height handed in
/// by the mouse router (which never sees a clamped value) can never outgrow
/// the frame the renderer would paint.
#[test]
fn layout_rects_reapplies_the_frame_cap_to_a_raw_height() {
    // A tall draft at a 20-row frame caps at min(16, 15) = 15.
    let rects = layout_rects(Rect::new(0, 0, 120, 20), 30);
    assert_eq!(rects.root[2].height, 15);
}

#[test]
fn chat_column_excludes_the_right_margin_and_matches_the_input_row() {
    let rects = layout_rects(Rect::new(0, 0, 120, 40), 1);
    let column = rects.chat_column();
    // Starts past the sidebar divider, skips the 1-col right margin,
    // and is vertically aligned with the input block (root[2]).
    assert_eq!(column.x, 26);
    assert_eq!(column.width, 120 - 26 - 1);
    assert_eq!(column.y, rects.root[2].y);
    assert_eq!(column.height, rects.root[2].height);
}

#[test]
fn panel_region_hit_tests_the_panels_and_the_chrome() {
    let rects = layout_rects(Rect::new(0, 0, 120, 40), 1);
    // Inside the sidebar column.
    assert_eq!(panel_region(&rects, 10, 5), PanelRegion::Sidebar);
    assert_eq!(panel_region(&rects, 0, 0), PanelRegion::Sidebar);
    // Inside the chat pane (right of the divider).
    assert_eq!(panel_region(&rects, 26, 0), PanelRegion::Chat);
    assert_eq!(panel_region(&rects, 60, 10), PanelRegion::Chat);
    // Chrome rows (spinner / input / hints) are never panels.
    assert_eq!(panel_region(&rects, 60, 37), PanelRegion::Other);
    assert_eq!(panel_region(&rects, 60, 38), PanelRegion::Other);
    assert_eq!(panel_region(&rects, 60, 39), PanelRegion::Other);
    // Out of bounds is Other too.
    assert_eq!(panel_region(&rects, 120, 10), PanelRegion::Other);
    assert_eq!(panel_region(&rects, 60, 40), PanelRegion::Other);
}
