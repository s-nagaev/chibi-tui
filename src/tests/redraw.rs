//! Unit tests for the event-driven redraw gate (see `run_loop` in the
//! binary crate root): the pure tick-gate helper and the dirty-flag
//! controller that decides when a frame is painted.

use super::support::{app_with_chats, submit_text};

#[test]
fn idle_app_needs_no_tick_redraw() {
    let app = app_with_chats(2);
    assert!(!app.any_busy());
    assert!(app.status_message.is_none());
    // Fully idle: the 100 ms tick must be a no-op, never a repaint.
    assert!(!crate::should_redraw(&app));
}

#[test]
fn busy_active_chat_keeps_the_tick_redraw_alive() {
    let mut app = app_with_chats(1);
    let _ = submit_text(&mut app, "hello");
    assert!(app.any_busy());
    assert!(crate::should_redraw(&app));
}

#[test]
fn busy_background_chat_keeps_the_tick_redraw_alive() {
    let mut app = app_with_chats(2);
    let _ = submit_text(&mut app, "hello");
    // The request lives in chat 0; the user switched to chat 1. A background
    // request's sidebar marker must keep animating, so the gate is `any_busy`,
    // not `is_busy` (active chat only).
    app.active = 1;
    assert!(!app.is_busy());
    assert!(app.any_busy());
    assert!(crate::should_redraw(&app));
}

#[test]
fn visible_status_toast_keeps_the_tick_redraw_alive() {
    let mut app = app_with_chats(1);
    app.show_status("backend busy");
    assert!(app.status_message.is_some());
    // The toast countdown expires on the tick cadence even when nothing is
    // busy: the gate must keep ticking while it is visible.
    assert!(crate::should_redraw(&app));
}

#[test]
fn tick_expires_the_toast_and_returns_to_idle_gate() {
    let mut app = app_with_chats(1);
    app.show_status("backend busy");
    while app.status_message.is_some() {
        app.tick_status_message();
    }
    assert!(!crate::should_redraw(&app));
}

#[test]
fn requested_frame_is_consumed_once() {
    let mut redraw = crate::RedrawController::fresh();
    // Initial dirty state: the very first loop iteration paints.
    assert!(redraw.take());
    // ... and the flag is consumed, so an eventless loop stays parked.
    assert!(!redraw.take());
    // A state-mutating event arm marks the frame dirty exactly once...
    redraw.request();
    // ...the next iteration repaints and clears the flag again.
    assert!(redraw.take());
    assert!(!redraw.take());
}

// ---- input growth cap (both frame regimes) --------------------

use chibi_tui::ui::max_input_block_height;
use ratatui::layout::Rect;

/// The editor growth cap in the TALL regime (frame height > 16): 3/4 of the
/// frame, always below the tiny-frame guard, and the raw draft height is
/// clamped to it.
#[test]
fn input_growth_cap_uses_three_quarters_on_tall_frames() {
    let frame = Rect::new(0, 0, 120, 40);
    assert_eq!(max_input_block_height(40), 30);

    let mut app = app_with_chats(1);
    let lines: Vec<String> = (0..50).map(|i| format!("l{i:02}")).collect();
    app.input.insert_str(&lines.join("\n"));
    assert_eq!(
        app.input_lines_height(frame),
        30,
        "50-line draft clamps to 3/4 of a 40-row frame"
    );
}

/// The editor growth cap in the TINY regime (frame height <= 16): the
/// tiny-frame guard (h - 4) wins, keeping the spinner row, the hints row and
/// a sliver of chat alive; the floor is one row.
#[test]
fn input_growth_cap_uses_the_guard_on_tiny_frames() {
    // h = 16: the guard and 3/4 agree on 12.
    let frame_16 = Rect::new(0, 0, 120, 16);
    assert_eq!(max_input_block_height(16), 12);
    // h = 12: the guard (8) is below 3/4 (9), so it wins.
    let frame_12 = Rect::new(0, 0, 120, 12);
    assert_eq!(max_input_block_height(12), 8);

    let mut app = app_with_chats(1);
    for _ in 0..30 {
        app.input.insert_str("x\n");
    }
    assert_eq!(app.input_lines_height(frame_16), 12);
    assert_eq!(app.input_lines_height(frame_12), 8);
    // Degenerate frames never drop below a single row.
    assert_eq!(app.input_lines_height(Rect::new(0, 0, 120, 1)), 1);
}
