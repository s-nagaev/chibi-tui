use super::super::*;
use super::support::*;

/// Status hints line lists the thread tools AND the thoughts state
/// token; `^C cancel` stays readable next to the longest connection
/// label. The `^S on/off` token joined (the reasoning toggle's
/// visible feedback + hint); to pay for it the self-evident `⇧↵`
/// newline hint retired and `^⇧F all` compacted to `^⇧F` (Shift+Enter
/// is the universal chat-app newline convention and the shifted find
/// next to `^F` reads as the global search — README documents both, and
/// the F1 modal is the full on-screen reference).
#[test]
fn status_line_lists_rename_and_newline_hints() {
    let mut app = App::new(mock::initial_chats());
    app.connection = Connection::Disconnected;
    let last = render_grid(&mut app).last().unwrap().clone();

    for needle in [
        "^R",
        "^C cancel",
        "^/\u{2325}\u{2191}\u{2193} chats",
        "F1 help",
        "^D",
        "^F",
        "^T",
        "^O",
        "^S on",
    ] {
        assert!(last.contains(needle), "{needle} missing from {last:?}");
    }
    assert!(
        !last.contains("^T next"),
        "stale cycling hint must be gone: {last:?}"
    );
    assert!(
        !last.contains("caret"),
        "retired caret hint must be gone: {last:?}"
    );
    assert!(
        !last.contains("\u{21e7}\u{21b5}"),
        "retired newline hint must be gone: {last:?}"
    );
    assert!(
        !last.contains("^S off"),
        "default state must read on: {last:?}"
    );
}

/// the hints line plus the LONGEST connection
/// label (`● disconnected (press R)`) must fit one row at 120 columns.
/// Paragraph clips overflowing content, so the presence of the label's
/// tail on the rendered row PROVES nothing was cut — an honest fit check.
#[test]
fn status_hints_fit_120_cols_with_longest_status_label() {
    let mut app = App::new(mock::initial_chats());
    app.connection = Connection::Disconnected;
    let last = render_grid(&mut app).last().unwrap().clone();

    assert!(
        last.contains("disconnected (press R)"),
        "status label clipped — hints overflowed 120 cols: {last:?}"
    );
    // Total painted width cannot exceed the frame width.
    assert!(
        last.trim_end().width() <= 120,
        "hints row too wide: {} cols — {:?}",
        last.trim_end().width(),
        last
    );
}

/// Hint compaction stays within budget with the longest status label.
#[test]
fn status_hints_still_fit_with_search_hint() {
    let mut app = App::new(mock::initial_chats());
    app.connection = Connection::Disconnected;
    let last = render_grid(&mut app).last().unwrap().clone();
    assert!(last.contains("^F"), "^F missing from hints: {last:?}");
    assert!(last.trim_end().width() <= 120, "hints row too wide");
}

/// A busy-refusal status toast renders in the status line (replacing the
/// hint block while visible) and always fits with the longest connection
/// label on one 120-col row.
#[test]
fn status_toast_renders_in_status_line() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.connection = Connection::Disconnected;
    app.show_status("can't delete — busy");
    let last = render_grid(&mut app).last().unwrap().clone();

    assert!(
        last.contains("can't delete — busy"),
        "toast missing: {last:?}"
    );
    assert!(
        last.contains("disconnected (press R)"),
        "connection label clipped by toast: {last:?}"
    );
    assert!(last.trim_end().width() <= 120, "toast row too wide");
    // Hints are hidden while the toast is up.
    assert!(!last.contains("^N"), "hints must yield to the toast");
}

/// the hints line gains `^⇧F all` and still fits
/// 120 cols with the longest status label — the self-evident `^L` and
/// `^V` hints retired to make room (both actions stay README-documented).
#[test]
fn status_hints_still_fit_with_global_search_hint() {
    let mut app = App::new(mock::initial_chats());
    app.connection = Connection::Disconnected;
    let last = render_grid(&mut app).last().unwrap().clone();

    assert!(
        last.contains("^\u{21e7}F"),
        "^⇧F missing from hints: {last:?}"
    );
    assert!(last.contains("^F"), "^F missing: {last:?}");
    assert!(
        last.contains("^C cancel"),
        "^C cancel must never be dropped"
    );
    assert!(
        last.contains("disconnected (press R)"),
        "status label clipped — hints overflowed 120 cols: {last:?}"
    );
    assert!(
        last.trim_end().width() <= 120,
        "hints row too wide: {} cols — {:?}",
        last.trim_end().width(),
        last
    );
}

/// the hints line carries `^T` (focus toggle —
/// wrap-cycling was removed) and still fits 120 cols with the longest
/// status label — `^R rename` compacted to `^R` to absorb the +1 col;
/// The status strip later compacted `^T panel` to bare `^T` to pay for
/// the `^O info` token. ^C cancel is never dropped.
#[test]
fn status_hints_still_fit_with_ctrl_t_panel_hint() {
    let mut app = App::new(mock::initial_chats());
    app.connection = Connection::Disconnected;
    let last = render_grid(&mut app).last().unwrap().clone();

    assert!(last.contains("^T"), "^T missing from hints: {last:?}");
    assert!(
        !last.contains("next"),
        "stale ^T next hint must be gone: {last:?}"
    );
    assert!(
        last.contains("^C cancel"),
        "^C cancel must never be dropped"
    );
    assert!(
        last.contains("disconnected (press R)"),
        "status label clipped — hints overflowed 120 cols: {last:?}"
    );
    assert!(
        last.trim_end().width() <= 120,
        "hints row too wide: {} cols — {:?}",
        last.trim_end().width(),
        last
    );
}

/// the hints line carries `^O` and, after
/// compacting `^D del`/`^T panel` to bare tokens, still fits 120 cols
/// with the longest status label. ^C cancel is never dropped.
#[test]
fn status_hints_still_fit_with_status_strip_hint() {
    let mut app = App::new(mock::initial_chats());
    app.connection = Connection::Disconnected;
    let last = render_grid(&mut app).last().unwrap().clone();

    assert!(last.contains("^O"), "^O missing from hints: {last:?}");
    assert!(
        last.contains("^C cancel"),
        "^C cancel must never be dropped"
    );
    assert!(
        last.contains("disconnected (press R)"),
        "status label clipped — hints overflowed 120 cols: {last:?}"
    );
    assert!(
        last.trim_end().width() <= 120,
        "hints row too wide: {} cols — {:?}",
        last.trim_end().width(),
        last
    );
}

/// the `^P` hint is advertised only when the backend
/// listed the clone command at handshake, and the row still fits 120
/// cols with the longest status label once it shows.
#[test]
fn clone_hint_advertised_only_when_backend_supports_it() {
    let mut app = App::new(mock::initial_chats());
    app.connection = Connection::Disconnected;
    let last = render_grid(&mut app).last().unwrap().clone();
    assert!(
        !last.contains("^P"),
        "clone hint must stay hidden without the capability: {last:?}"
    );

    app.set_backend_commands(vec![
        "/reset".to_owned(),
        "/new_thread_with_current_context".to_owned(),
    ]);
    let last = render_grid(&mut app).last().unwrap().clone();
    assert!(last.contains("^P"), "clone hint missing: {last:?}");
    assert!(
        last.trim_end().width() <= 120,
        "hints row too wide: {} cols — {:?}",
        last.trim_end().width(),
        last
    );
}
