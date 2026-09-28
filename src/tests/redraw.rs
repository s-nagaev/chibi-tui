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
