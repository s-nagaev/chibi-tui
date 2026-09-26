use super::support::*;
use crate::app::*;

/// Search semantics at the state level:
/// commit jumps to the nearest match at or after the cursor, n/N walk
/// with wraparound, and a snapshot refresh reindexes while keeping the
/// current hit when it still exists.
#[test]
fn log_search_commit_jump_wraparound_and_reindex() {
    let lines = vec![
        "alpha one".to_owned(),
        "beta ALPHA two".to_owned(),
        "gamma".to_owned(),
        "alpha three".to_owned(),
    ];
    let mk = |cursor: usize, lines: Vec<String>, search: Option<LogSearch>| Mode::LogViewer {
        state: LogViewerState {
            cursor,
            wrap: false,
            row_offset: 0,
            lines: lines
                .into_iter()
                .map(crate::diag::LogEntry::parse)
                .collect(),
            snapshot_total: crate::diag::total_appended(),
            search_buf: None,
            search,
            copy_note: None,
            copy_note_at: None,
        },
    };

    // Commit from the middle: the cursor jumps DOWN to the nearest hit.
    let mut app = App::new(Vec::new());
    app.mode = mk(2, lines.clone(), None);
    app.log_open_search();
    for ch in "alpha".chars() {
        app.log_search_push(ch);
    }
    app.log_commit_search();
    let state = log_state(&app);
    assert_eq!(state.search.as_ref().unwrap().matches, vec![0, 1, 3]);
    assert_eq!(state.search.as_ref().unwrap().current, None);
    assert_eq!(state.cursor, 3, "jumped to the nearest match >= cursor");

    // n wraps from the last hit back to the first.
    app.log_search_next();
    let state = log_state(&app);
    assert_eq!(state.cursor, 0);
    assert_eq!(state.search.as_ref().unwrap().current, Some(0));

    // N goes back (wrapping to the tail hit).
    app.log_search_prev();
    let state = log_state(&app);
    assert_eq!(state.cursor, 3);
    assert_eq!(state.search.as_ref().unwrap().current, Some(2));

    // A snapshot refresh (tail re-arm) reindexes the same pattern; the
    // current hit survives while its ordinal stays valid.
    app.mode = mk(
        0,
        vec!["alpha again".to_owned(), "unrelated".to_owned()],
        Some(LogSearch {
            pattern: "alpha".to_owned(),
            matches: vec![0, 3, 7],
            current: Some(2),
        }),
    );
    if let Mode::LogViewer { state } = &mut app.mode {
        state.reindex_search();
    }
    let state = log_state(&app);
    assert_eq!(state.search.as_ref().unwrap().matches, vec![0]);
    assert_eq!(
        state.search.as_ref().unwrap().current,
        None,
        "ordinal beyond the new list drops the current hit"
    );

    // An empty pattern commit switches the search off.
    app.log_open_search();
    app.log_commit_search();
    assert!(log_state(&app).search.is_none(), "empty pattern = off");
}

/// y copies the full logical line and leaves the brief header feedback
/// behind (hermetic: hand-built state, no diag stream involvement).
#[test]
fn log_copy_selected_sets_feedback_note() {
    let mut app = App::new(Vec::new());
    app.mode = Mode::LogViewer {
        state: LogViewerState {
            cursor: 1,
            wrap: true,
            row_offset: 0,
            lines: vec![
                "first".to_owned(),
                "second line with the content to copy".to_owned(),
            ]
            .into_iter()
            .map(crate::diag::LogEntry::parse)
            .collect(),
            snapshot_total: crate::diag::total_appended(),
            search_buf: None,
            search: None,
            copy_note: None,
            copy_note_at: None,
        },
    };
    app.log_copy_selected();
    let state = log_state(&app);
    assert_eq!(state.copy_note.as_deref(), Some("copied"));
    assert!(state.copy_note_at.is_some(), "expiry anchor recorded");

    // The note expires: after its window the header is clean again.
    if let Mode::LogViewer { state } = &mut app.mode {
        state.copy_note_at = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(3))
                .expect("clock moved backwards"),
        );
        state.expire_copy_note();
    }
    assert!(log_state(&app).copy_note.is_none());
}
