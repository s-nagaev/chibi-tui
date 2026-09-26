//! Log-viewer cursor pinning and live-tail re-arm.
//!
//! Keep this test isolated from the other diag-appending test modules:
//! it pins an exact cursor position derived from the global diag ring
//! size at open, so a concurrent appender (or a big pre-existing
//! backlog) breaks it. Module name = sort-order gap to `viewer_*`.
use super::support::*;

#[test]
fn log_navigation_pins_and_return_to_bottom_re_arms_live_tail() {
    let mut app = app_with_chats(0);
    for i in 0..40 {
        crate::diag::append(format!("filler-{i}"));
    }
    app.begin_log_viewer();
    let baseline = log_state(&app).snapshot_total;

    // Detach: stepping the cursor up freezes the snapshot (baseline
    // stays put). Steps walk LOGICAL lines one by one.
    app.log_cursor_up(30);
    app.log_cursor_up(30);
    assert_eq!(log_state(&app).cursor, 0, "cursor clamps at the top");
    app.log_cursor_down(10);
    assert_eq!(log_state(&app).cursor, 10);
    assert!(
        !log_state(&app).at_tail(),
        "still pinned away from the tail"
    );

    // Lines arriving while pinned count against the baseline…
    let detached_marker = unique_line("detached");
    crate::diag::append(&detached_marker);
    assert_eq!(
        log_state(&app).snapshot_total,
        baseline,
        "snapshot frozen while pinned"
    );
    assert!(
        !log_state(&app)
            .lines
            .iter()
            .any(|e| e.text == *detached_marker),
        "frozen snapshot does not show the arrival"
    );
    let total_now = crate::diag::total_appended();
    assert!(
        total_now >= baseline,
        "monotonic total includes the detached arrival"
    );

    // …and G (jump to bottom) re-arms the tail: the snapshot refreshes
    // to CURRENT content.
    app.log_jump_bottom();
    let state = log_state(&app);
    assert!(state.at_tail(), "back at the bottom");
    assert!(
        state.lines.iter().any(|e| e.text == *detached_marker),
        "live-tail refresh picked up the detached arrival"
    );
    assert!(
        state.snapshot_total >= baseline,
        "baseline advanced to the current ring content"
    );
}
