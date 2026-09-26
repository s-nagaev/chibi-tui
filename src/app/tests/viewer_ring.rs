//! Log viewer against the process-global diag ring: snapshot on open,
//! seen-watermark, modal gating, close semantics. These tests append to
//! the shared ring, so they stay grouped (see `log_navigation`).
use super::support::*;
use crate::app::*;

#[test]
fn opening_log_viewer_snapshots_ring_and_lives_tail() {
    let marker = unique_line("snap");
    crate::diag::append(&marker);
    let total_before_open = crate::diag::total_appended();

    // Works with ZERO chats: the viewer reads the global diag stream.
    let mut app = App::new(Vec::new());
    app.begin_log_viewer();

    let state = log_state(&app);
    assert!(
        state.at_tail(),
        "opens live-tailing: cursor on the newest line"
    );
    assert!(
        state.lines.iter().any(|e| e.text == *marker),
        "snapshot copy contains the buffered marker"
    );
    // Reset-on-open (consumer-side watermark): everything up to the
    // open-time total is seen. Monotonicity makes `>=` exact — the
    // watermark must have captured a total that already includes the
    // pre-open marker.
    assert!(
        app.log_seen_total >= total_before_open,
        "seen watermark advanced past the pre-open marker: {} < {}",
        app.log_seen_total,
        total_before_open
    );
    assert_eq!(
        app.log_seen_total, state.snapshot_total,
        "watermark and +K baseline coincide at open"
    );
}

/// The unseen marker logic end to end at the state level: unseen arrivals
/// after open/close keep the app "dirty" (marker would show); re-opening
/// (and closing at the bottom) marks everything seen again.
#[test]
fn seen_watermark_resets_on_open_and_close_at_bottom() {
    let mut app = App::new(Vec::new());
    crate::diag::append("pre-existing");
    assert!(
        crate::diag::total_appended() > app.log_seen_total,
        "precondition: fresh app has unseen lines"
    );

    app.begin_log_viewer();
    let seen_at_open = app.log_seen_total;

    // Lines arriving while the viewer is OPEN at the tail are watched
    // live: closing at the bottom marks them seen too.
    crate::diag::append("watched-live");
    assert!(app.close_log_viewer());
    assert!(
        app.log_seen_total > seen_at_open,
        "close-at-bottom advances the watermark past arrivals"
    );

    // Closing while PINNED (cursor above the tail) does not mark: the
    // missed lines stay unseen.
    let mut app = App::new(Vec::new());
    for i in 0..10 {
        crate::diag::append(format!("filler-{i}"));
    }
    app.begin_log_viewer();
    let seen_at_open = app.log_seen_total;
    app.log_cursor_up(5);
    assert!(
        !log_state(&app).at_tail(),
        "precondition: cursor up pinned the view away from the tail"
    );
    crate::diag::append("missed-while-pinned");
    assert!(app.close_log_viewer());
    assert_eq!(
        app.log_seen_total, seen_at_open,
        "close while pinned must not mark the missed lines seen"
    );
    assert!(
        crate::diag::total_appended() > app.log_seen_total,
        "the missed line still counts as unseen (marker shows)"
    );
}

#[test]
fn opening_log_viewer_noop_while_another_modal_owns_the_keyboard() {
    // Rename session.
    let mut app = app_with_chats(1);
    app.begin_rename();
    app.begin_log_viewer();
    assert!(matches!(app.mode, Mode::Renaming { .. }));

    // Delete-confirm popup.
    let mut app = app_with_chats(1);
    app.begin_delete_confirm();
    app.begin_log_viewer();
    assert_eq!(app.mode, Mode::ConfirmDelete);

    // In-thread search popup.
    let mut app = app_with_chats(1);
    app.begin_search();
    app.begin_log_viewer();
    assert!(matches!(app.mode, Mode::Searching { .. }));

    // Global search popup.
    let mut app = app_with_chats(1);
    app.begin_search_all();
    app.begin_log_viewer();
    assert!(matches!(app.mode, Mode::SearchingAll { .. }));

    // Already-open viewer: re-open must not reset the cursor.
    let mut app = app_with_chats(0);
    for i in 0..12 {
        crate::diag::append(format!("filler-{i}"));
    }
    app.begin_log_viewer();
    app.log_cursor_up(7);
    let pinned_at = log_state(&app).cursor;
    app.begin_log_viewer();
    assert_eq!(
        log_state(&app).cursor,
        pinned_at,
        "re-open does not clobber"
    );
}

#[test]
fn closing_log_viewer_resets_mode_and_focus() {
    // At the bottom (live-tail): close marks everything seen.
    let mut app = app_with_chats(2);
    app.focus = Focus::Sidebar;
    app.begin_log_viewer();
    assert!(app.close_log_viewer());
    assert!(app.mode.is_normal());
    assert_eq!(app.focus, Focus::Chat, "modal close returns to editor");
    assert!(!app.close_log_viewer(), "closing again is a no-op");

    // Scrolled up: close still resets the UI…
    let mut app = App::new(Vec::new());
    for i in 0..12 {
        crate::diag::append(format!("filler-{i}"));
    }
    app.begin_log_viewer();
    let seen_at_open = app.log_seen_total;
    app.log_cursor_up(9);
    assert!(!log_state(&app).at_tail(), "precondition: pinned");
    crate::diag::append("missed-while-pinned");
    assert!(app.close_log_viewer());
    assert!(app.mode.is_normal());
    assert_eq!(app.focus, Focus::Chat);
    // …and the watermark did NOT advance: the missed line stays unseen.
    assert_eq!(
        app.log_seen_total, seen_at_open,
        "close while detached must not mark missed lines seen"
    );
}
