use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

// ^G log viewer ------------------------------

#[test]
fn ctrl_g_opens_log_viewer_from_normal_mode() {
    let marker = format!(
        "routing-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    chibi_tui::diag::append(&marker);
    let mut app = app_with_chats(1);

    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);

    assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
    let state = match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state,
        other => panic!("expected LogViewer, got {other:?}"),
    };
    assert!(state.at_tail(), "opens live-tailing: cursor on the tail");
    assert!(
        state.lines.iter().any(|e| e.text == *marker),
        "the modal snapshot carries the buffered marker"
    );
}

/// Chord-freedom regression: ^G is consumed as the hotkey — it must not
/// insert anything into the prompt textarea.
#[test]
fn ctrl_g_in_normal_mode_never_touches_input_buffer() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    app.close_log_viewer();
    assert!(app.input.lines().iter().all(|l| l.is_empty()));
}

#[test]
fn ctrl_g_opens_log_viewer_from_sidebar_focus_too() {
    let mut app = app_with_chats(2);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);

    assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
    // Closing returns to the editor pane (modal close semantics).
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
    assert!(app.mode.is_normal());
}

#[test]
fn log_viewer_pgup_pgdn_arrows_and_letters_navigate() {
    let mut app = app_with_chats(1);
    for i in 0..60 {
        chibi_tui::diag::append(format!("filler-{i}"));
    }
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);

    // The diag stream is process-global (other tests append in
    // parallel), so indices are relative to the open-time snapshot.
    let cursor = |app: &chibi_tui::app::App| match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state.cursor,
        other => panic!("expected LogViewer, got {other:?}"),
    };
    let tail = cursor(&app);
    assert!(tail >= 59, "the 60 filler lines are in the snapshot");

    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(app.log_visible_rows, 20, "page = one modal page");
    assert_eq!(cursor(&app), tail - 20, "PgUp pages up, cursor follows");
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(cursor(&app), tail - 21, "↑ steps one line");
    press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
    assert_eq!(cursor(&app), tail - 22, "k steps one line too");
    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(cursor(&app), tail - 2);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(cursor(&app), tail - 1, "↓ steps one line, still pinned");
    press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
    let at_tail = match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state.at_tail(),
        other => panic!("expected LogViewer, got {other:?}"),
    };
    assert!(at_tail, "j lands back on the live tail (re-armed)");

    press(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
    assert_eq!(cursor(&app), 0, "g jumps to the top");
    press(&mut app, KeyCode::Char('G'), KeyModifiers::NONE);
    let at_tail = match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state.at_tail(),
        other => panic!("expected LogViewer, got {other:?}"),
    };
    assert!(at_tail, "G jumps back to the live tail");
}

/// `w` toggles wrap from the keyboard; the cursor stays over the same
/// logical line (full render-level behavior covered in ui.rs).
#[test]
fn log_viewer_w_toggles_wrap() {
    let mut app = app_with_chats(1);
    chibi_tui::diag::append("some line");
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    let wrap = |app: &chibi_tui::app::App| match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state.wrap,
        other => panic!("expected LogViewer, got {other:?}"),
    };
    assert!(!wrap(&app));
    press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
    assert!(wrap(&app), "`w` turns wrap on");
    press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
    assert!(!wrap(&app), "`w` turns wrap back off");
}

#[test]
fn log_viewer_esc_closes_and_keeps_state_intact() {
    let mut app = app_with_chats(2);
    app.scroll_up(7);
    let scroll_before = app.scroll;
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert!(app.mode.is_normal(), "Esc closes the viewer");
    assert_eq!(app.scroll, scroll_before, "chat view untouched");
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
}

/// Modal isolation: while the log viewer is open, typing and every
/// global binding is swallowed — nothing reaches the textarea, nothing
/// fires, and the popup stays open.
#[test]
fn log_viewer_swallows_typing_and_global_chords() {
    let mut app = app_with_chats(3);
    for i in 0..20 {
        chibi_tui::diag::append(format!("filler-{i}"));
    }
    type_in(&mut app, "draft");
    app.scroll_up(30);
    let scroll_before = app.scroll;
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
    let cursor = |app: &chibi_tui::app::App| match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state.cursor,
        other => panic!("expected LogViewer, got {other:?}"),
    };
    let cursor_before = cursor(&app);

    // Typing must not reach the textarea and must not move the cursor
    // (letters avoid the viewer's own keys, x/y/z are not bound here).
    type_in(&mut app, "xyz");
    assert_eq!(cursor(&app), cursor_before);
    // Global chords suspended: nav, new chat, rename entry, wipe,
    // delete, searches, focus toggle.
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    // Left/Right are not nav here: swallowed too.
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);

    assert!(
        matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }),
        "popup must stay open"
    );
    assert_eq!(app.chats.len(), 3, "Ctrl+N must not fire");
    assert_eq!(app.scroll, scroll_before, "PgUp-style chat scroll blocked");
    assert!(
        !matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
        "Ctrl+L must not fire"
    );
    assert!(!app.should_quit);
    assert_eq!(
        app.input.lines().join(""),
        "draft",
        "typing must not leak into (nor disturb) the prompt textarea"
    );
    assert_eq!(
        cursor(&app),
        cursor_before,
        "swallowed keys must not move the read position"
    );
}

// `/` search, n/N, `y` copy ----------

/// Hand-built pinned viewer state: hermetic against the global diag
/// stream that parallel tests append to.
pub(crate) fn log_viewer_state(
    cursor: usize,
    lines: Vec<&str>,
    search: Option<chibi_tui::app::LogSearch>,
) -> chibi_tui::app::LogViewerState {
    chibi_tui::app::LogViewerState {
        cursor,
        wrap: false,
        row_offset: 0,
        lines: lines
            .into_iter()
            .map(|s| chibi_tui::diag::LogEntry::parse(s.to_owned()))
            .collect(),
        snapshot_total: chibi_tui::diag::total_appended(),
        search_buf: None,
        search,
        copy_note: None,
        copy_note_at: None,
    }
}

fn viewer_search(app: &chibi_tui::app::App) -> &chibi_tui::app::LogSearch {
    match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => {
            state.search.as_ref().expect("search committed")
        }
        other => panic!("expected LogViewer, got {other:?}"),
    }
}

/// `/` opens the prompt, typing fills it, Enter commits; n/N walk the
/// hits with wraparound on both ends (line-level, both wrap modes use
/// the same logical lines, so this holds under wrap too).
#[test]
fn log_viewer_search_open_commit_and_navigate_wraparound() {
    let mut app = app_with_chats(1);
    app.mode = chibi_tui::app::Mode::LogViewer {
        state: log_viewer_state(
            3,
            vec!["alpha one", "beta ALPHA two", "gamma", "alpha three"],
            None,
        ),
    };

    // Open the prompt and type the pattern.
    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    for ch in "alpha".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    let buf = match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state.search_buf.clone().expect("prompt open"),
        other => panic!("expected LogViewer, got {other:?}"),
    };
    assert_eq!(buf, "alpha", "prompt holds the typed pattern");

    // Enter commits: case-insensitive, line-level, cursor jumps to the
    // nearest hit at or after its line (3).
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let search = viewer_search(&app);
    assert_eq!(search.pattern, "alpha");
    assert_eq!(search.matches, vec![0, 1, 3]);
    assert_eq!(search.current, None, "no hit selected before the first n");
    let cursor = |app: &chibi_tui::app::App| match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state.cursor,
        other => panic!("expected LogViewer, got {other:?}"),
    };
    assert_eq!(cursor(&app), 3);

    // n walks forward and wraps at the tail hit.
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert_eq!(cursor(&app), 0);
    assert_eq!(viewer_search(&app).current, Some(0));
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert_eq!(cursor(&app), 1);
    assert_eq!(viewer_search(&app).current, Some(1));
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert_eq!(cursor(&app), 3);
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert_eq!(cursor(&app), 0, "n wraps around at the end");

    // N steps back (wrapping to the tail hit from the top).
    press(&mut app, KeyCode::Char('N'), KeyModifiers::NONE);
    assert_eq!(cursor(&app), 3);
    assert_eq!(viewer_search(&app).current, Some(2));
}

/// Esc on the open prompt cancels it and keeps a previously committed
/// search intact; Enter on an empty prompt switches the search off.
#[test]
fn log_viewer_search_esc_cancels_and_empty_commit_switches_off() {
    let previous = Some(chibi_tui::app::LogSearch {
        pattern: "gamma".to_owned(),
        matches: vec![2],
        current: Some(0),
    });
    let mut app = app_with_chats(1);
    app.mode = chibi_tui::app::Mode::LogViewer {
        state: log_viewer_state(0, vec!["alpha one", "gamma line"], previous.clone()),
    };

    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => {
            assert!(state.search_buf.is_none(), "prompt closed by Esc");
            assert_eq!(&state.search, &previous, "old search untouched");
        }
        other => panic!("expected LogViewer, got {other:?}"),
    }

    // Empty pattern commit = search off.
    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => {
            assert!(state.search.is_none(), "empty pattern switches off");
        }
        other => panic!("expected LogViewer, got {other:?}"),
    }
}

/// While the search prompt is open it owns the keyboard: nav letters
/// and global chords land in the buffer, nothing else moves.
#[test]
fn log_viewer_search_prompt_swallows_everything_else() {
    let mut app = app_with_chats(1);
    app.mode = chibi_tui::app::Mode::LogViewer {
        state: log_viewer_state(1, vec!["alpha one", "beta two"], None),
    };
    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    // Letters that are viewer keys outside the prompt, plus a global
    // chord and arrows: all swallowed, only plain chars type.
    press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('G'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // commit "wnG"
    match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => {
            let search = state.search.as_ref().expect("committed");
            assert_eq!(search.pattern, "wnG", "only plain chars reached the buffer");
            assert!(
                search.matches.is_empty(),
                "no line matches, search stays active with zero hits"
            );
            assert_eq!(state.cursor, 1, "cursor never moved while typing");
            assert!(!state.wrap, "w did not toggle wrap inside the prompt");
        }
        other => panic!("expected LogViewer, got {other:?}"),
    }
}

/// `y` copies the cursor line and the header gets the brief `copied`
/// feedback (the OSC 52 bytes themselves are asserted in clipboard.rs;
/// here the write goes to the test process stdout, which is harmless).
#[test]
fn log_viewer_y_sets_copied_feedback() {
    let mut app = app_with_chats(1);
    app.mode = chibi_tui::app::Mode::LogViewer {
        state: log_viewer_state(1, vec!["first", "second line"], None),
    };
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => {
            assert_eq!(state.copy_note.as_deref(), Some("copied"));
            assert!(state.copy_note_at.is_some());
        }
        other => panic!("expected LogViewer, got {other:?}"),
    }
}

/// Ctrl+C keeps its popup-class meaning: it opens the quit
/// confirmation from the log viewer; the second Ctrl+C confirms.
#[test]
fn log_viewer_ctrl_c_opens_quit_confirmation() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm);
    assert!(!app.should_quit);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.should_quit, "second Ctrl+C confirms the quit");
    assert_eq!(app.chats.len(), 1, "quit must not mutate chats");
}

/// Other modals own ^G: the confirm popup swallows it; the search
/// popups swallow the Ctrl flavor (plain chars feed the query); the
/// rename editor consumes any Char into its draft (documented branch
/// behavior, same as Ctrl+N pushing 'n' there).
#[test]
fn ctrl_g_swallowed_by_other_modals() {
    // Delete-confirm popup: swallowed, popup stays.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);

    // In-thread search popup: swallowed, query untouched.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
    assert_eq!(app.search_query(), Some(""));

    // Rename session: 'g' lands in the draft (any-Char branch).
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Renaming { .. }));
    assert_eq!(app.rename_buf(), Some("chat-0g"));
}
