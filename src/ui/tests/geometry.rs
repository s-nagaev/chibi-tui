use super::super::*;

// ---- layout rects / panel region (mouse hit-testing) ----

#[test]
fn layout_rects_matches_the_draw_layout() {
    let rects = layout_rects(Rect::new(0, 0, 120, 40), 1, DEFAULT_SIDEBAR_WIDTH);
    // Sidebar is the 30-col left column of the main area; the chat
    // pane takes the rest of the width.
    assert_eq!(rects.sidebar.width, 30);
    assert_eq!(rects.chat.x, 30);
    assert_eq!(rects.chat.width, 120 - 30);
    // Root stack: main area of 37 rows, then spinner / input / hints
    // rows of one row each at the bottom.
    assert_eq!(rects.root[0].height, 37);
    assert_eq!(rects.root[1].height, 1);
    assert_eq!(rects.root[2].height, 1);
    assert_eq!(rects.root[3].height, 1);
    assert_eq!(rects.root[3].y, 39);
}

#[test]
fn layout_rects_honors_a_runtime_sidebar_width() {
    // A non-default width flows straight into the horizontal split (the
    // mouse drag route: App::sidebar_width → layout_rects → renderer and
    // router agree).
    let rects = layout_rects(Rect::new(0, 0, 120, 40), 1, 44);
    assert_eq!(rects.sidebar.width, 44);
    assert_eq!(rects.chat.x, 44);
    assert_eq!(rects.chat.width, 120 - 44);
    // A stale persisted width past the half-frame cap gets clamped to the
    // same geometry the drag router would produce.
    let rects = layout_rects(Rect::new(0, 0, 120, 40), 1, 200);
    assert_eq!(rects.sidebar.width, 60);
}

#[test]
fn layout_rects_clamps_the_input_block_on_tiny_frames() {
    // 6-row frame with a 10-row draft: the clamp keeps spinner + hints
    // + a sliver of chat, so the block collapses to 2 rows and the
    // chat pane absorbs what remains.
    let rects = layout_rects(Rect::new(0, 0, 120, 6), 10, DEFAULT_SIDEBAR_WIDTH);
    assert_eq!(rects.root[2].height, 2);
    assert_eq!(rects.root[0].height, 2);
    // A 1-row frame never underflows.
    let rects = layout_rects(Rect::new(0, 0, 40, 1), 1, DEFAULT_SIDEBAR_WIDTH);
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

/// The width clamp is `clamp(w, 20 ..= min(frame/2, frame-24))`: a floor of
/// 20 columns, a half-frame cap, and a chat-pane survival floor of 24
/// columns (`frame - 24`), so the chat area's `Min(20)` constraint always
/// holds with slack. Saturating math keeps tiny frames at the 20 floor.
#[test]
fn clamp_sidebar_width_enforces_min_cap_and_chat_floor() {
    // In-range widths pass through untouched.
    assert_eq!(clamp_sidebar_width(30, 120), 30);
    assert_eq!(clamp_sidebar_width(26, 120), 26);
    // Below the floor → floor.
    assert_eq!(clamp_sidebar_width(10, 120), 20);
    // Past the half-frame cap → cap (60 @120).
    assert_eq!(clamp_sidebar_width(200, 120), 60);
    // The chat-pane floor wins on narrow frames: max = min(22, 20) = 20
    // @44, and on a 40-col frame both rules agree on 20 (saturating).
    assert_eq!(clamp_sidebar_width(44, 44), 20);
    assert_eq!(clamp_sidebar_width(200, 40), 20);
    assert_eq!(clamp_sidebar_width(30, 24), 20);
}

/// `LayoutRects::input` re-applies the same cap, so a RAW height handed in
/// by the mouse router (which never sees a clamped value) can never outgrow
/// the frame the renderer would paint.
#[test]
fn layout_rects_reapplies_the_frame_cap_to_a_raw_height() {
    // A tall draft at a 20-row frame caps at min(16, 15) = 15.
    let rects = layout_rects(Rect::new(0, 0, 120, 20), 30, DEFAULT_SIDEBAR_WIDTH);
    assert_eq!(rects.root[2].height, 15);
}

#[test]
fn chat_column_excludes_the_right_margin_and_matches_the_input_row() {
    let rects = layout_rects(Rect::new(0, 0, 120, 40), 1, DEFAULT_SIDEBAR_WIDTH);
    let column = rects.chat_column();
    // Starts past the sidebar divider, skips the 1-col right margin,
    // and is vertically aligned with the input block (root[2]).
    assert_eq!(column.x, 30);
    assert_eq!(column.width, 120 - 30 - 1);
    assert_eq!(column.y, rects.root[2].y);
    assert_eq!(column.height, rects.root[2].height);
}

/// The editor wrap width follows the RUNTIME sidebar width (single source
/// of truth: `App::input_lines_height` reads it through the same helper).
#[test]
fn input_text_width_tracks_the_runtime_sidebar_width() {
    let area = Rect::new(0, 0, 120, 40);
    assert_eq!(
        input_text_width(area, DEFAULT_SIDEBAR_WIDTH),
        120 - 30 - 1 - 2
    );
    assert_eq!(input_text_width(area, 44), 120 - 44 - 1 - 2);
    // Clamped exactly like the layout: a raw overcap width must not
    // inflate the chat column past what the renderer paints.
    assert_eq!(input_text_width(area, 200), 120 - 60 - 1 - 2);
}

#[test]
fn panel_region_hit_tests_the_panels_and_the_chrome() {
    let rects = layout_rects(Rect::new(0, 0, 120, 40), 1, DEFAULT_SIDEBAR_WIDTH);
    // Inside the sidebar column.
    assert_eq!(panel_region(&rects, 10, 5), PanelRegion::Sidebar);
    assert_eq!(panel_region(&rects, 0, 0), PanelRegion::Sidebar);
    // Inside the chat pane (right of the divider).
    assert_eq!(panel_region(&rects, 30, 0), PanelRegion::Chat);
    assert_eq!(panel_region(&rects, 60, 10), PanelRegion::Chat);
    // Chrome rows (spinner / input / hints) are never panels.
    assert_eq!(panel_region(&rects, 60, 37), PanelRegion::Other);
    assert_eq!(panel_region(&rects, 60, 38), PanelRegion::Other);
    assert_eq!(panel_region(&rects, 60, 39), PanelRegion::Other);
    // Out of bounds is Other too.
    assert_eq!(panel_region(&rects, 120, 10), PanelRegion::Other);
    assert_eq!(panel_region(&rects, 60, 40), PanelRegion::Other);
}
