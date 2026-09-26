//! Log viewer scrolling over the diag ring: wrap toggle and viewport
//! page navigation (both append to the shared ring).
use super::support::*;

/// `w` flips the wrap flag; the cursor keeps
/// pointing at the SAME logical line, and one cursor step still walks
/// one logical line even when wrap splits it over several rows.
#[test]
fn wrap_toggle_keeps_cursor_over_logical_lines() {
    let mut app = app_with_chats(0);
    crate::diag::append("short head");
    crate::diag::append(format!("LONG-{}", "x".repeat(300)));
    crate::diag::append("short tail");
    app.begin_log_viewer();

    // The diag stream is process-global and other tests append to it in
    // parallel, so all indices are relative to the snapshot taken at
    // open: the three lines above are the LAST three snapshot entries.
    let len = log_state(&app).lines.len();
    let tail = len - 1;

    assert!(!log_state(&app).wrap, "wrap starts off");
    assert!(log_state(&app).at_tail(), "cursor parked on the tail");
    app.log_toggle_wrap();
    assert!(log_state(&app).wrap, "`w` turns wrap on");
    assert_eq!(
        log_state(&app).cursor,
        tail,
        "cursor still on the same logical line"
    );

    // One step up = one logical line: from `short tail` straight onto
    // the 300-char line, regardless of its wrapped row count.
    app.log_cursor_up(1);
    assert_eq!(log_state(&app).cursor, tail - 1);
    app.log_cursor_up(1);
    assert_eq!(log_state(&app).cursor, tail - 2);
    app.log_cursor_up(1);
    assert_eq!(
        log_state(&app).cursor,
        tail.saturating_sub(3),
        "clamped only at the very top"
    );

    app.log_toggle_wrap();
    assert!(!log_state(&app).wrap, "second `w` turns wrap back off");
}

/// PgUp/PgDn move the cursor by one viewport of
/// rows (lines while wrap is off); landing back on the newest line
/// re-arms the live tail.
#[test]
fn page_navigation_moves_cursor_by_viewport_rows() {
    let mut app = app_with_chats(0);
    for i in 0..60 {
        crate::diag::append(format!("filler-{i}"));
    }
    app.begin_log_viewer();
    // Default page seam before the first render (same as chat pane).
    assert_eq!(app.log_visible_rows, 20);
    // Indices relative to the open-time snapshot: the diag stream is
    // process-global and other tests append to it in parallel.
    let tail = log_state(&app).lines.len() - 1;
    assert_eq!(log_state(&app).cursor, tail, "opens on the newest line");

    app.log_page_up();
    assert_eq!(log_state(&app).cursor, tail - 20, "PgUp = one page up");
    app.log_page_up();
    assert_eq!(log_state(&app).cursor, tail - 40, "PgUp accumulates");
    app.log_page_down();
    assert_eq!(log_state(&app).cursor, tail - 20, "PgDn = one page down");
    let baseline = log_state(&app).snapshot_total;
    app.log_page_down();
    assert!(
        log_state(&app).at_tail(),
        "PgDn lands back on the tail and re-arms"
    );
    assert!(
        log_state(&app).snapshot_total >= baseline,
        "tail re-armed: snapshot refreshed to current ring content"
    );
}
