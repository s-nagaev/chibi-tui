use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

// table ↔ dispatch pinning -----------------
//
// The modal's content is the const table `chibi_tui::ui::HOTKEY_ROWS`.
// These tests pin that table against the REAL key handlers from both
// sides: (1) a dispatch enumeration below must equal the table row by
// row, and (2) every Normal-mode chord the table advertises must be
// demonstrably CLAIMED by `handle_key` — pressed on a draft-bearing app,
// none of them may land in the textarea as plain typing (the `(_, _)`
// fall-through would insert it). A chord added to the dispatch without
// a table row fails (1); a table row whose chord the dispatch dropped
// fails (2).

/// The dispatch's chord→action map as the help table must render it.
/// Maintained next to `handle_key`: a new binding updates BOTH this and
/// `ui::HOTKEY_ROWS` in the same commit, or the suite goes red.
const DISPATCH_CHORDS: &[(&str, &str, &str)] = &[
    ("Global", "F1", "open / close this keybindings help"),
    (
        "Global",
        "Ctrl+C",
        "cancel the active request · quit when idle (confirmation)",
    ),
    ("Global", "Ctrl+N", "new chat"),
    (
        "Global",
        "Ctrl+P",
        "clone the active thread (needs backend support)",
    ),
    ("Global", "Ctrl+R", "rename the active thread"),
    (
        "Global",
        "Ctrl+D",
        "delete the active thread (confirmation)",
    ),
    ("Global", "Ctrl+F", "find in the active thread"),
    ("Global", "Ctrl+Shift+F", "find in all threads"),
    ("Global", "Ctrl+T", "toggle pane focus (chat / sidebar)"),
    ("Global", "Ctrl+G", "open the diagnostics log viewer"),
    ("Global", "Ctrl+O", "toggle the status strip"),
    ("Global", "Ctrl+S", "toggle the thoughts block"),
    ("Global", "Ctrl+M", "open the model picker"),
    (
        "Global",
        "Ctrl+L",
        "stop the running request (confirmation)",
    ),
    (
        "Global",
        "Shift+Ctrl+L",
        "reset this thread · clear the dialog (confirmation)",
    ),
    ("Global", "Ctrl+↑/↓ · Alt+↑/↓", "switch the active thread"),
    ("Global", "PgUp / PgDn", "scroll the chat view"),
    (
        "Global",
        "Wheel ↑ / ↓",
        "scroll the chat · select in the focused sidebar",
    ),
    (
        "Global",
        "Drag-select",
        "highlight chat text · release copies it",
    ),
    ("Global", "y", "copy the active chat selection"),
    (
        "Global",
        "Ctrl-chords",
        "work under RU / UA keyboard layouts",
    ),
    ("Global", "Esc", "clear the input · dismiss popups"),
    ("Input", "Enter", "send the message (queues while busy)"),
    ("Input", "⇧↵ / ⌥↵", "insert a newline"),
    ("Input", "↑ / ↓", "move the caret in the draft"),
    ("Input", "Ctrl+A / Ctrl+E", "caret to line start / end"),
    ("Input", "Ctrl+U", "delete to the start of the line"),
    ("Input", "Ctrl+V / Cmd+V", "paste the clipboard"),
    ("Input", "Backspace", "delete backwards"),
    ("Input", "text", "type into the draft"),
    (
        "Sidebar (Ctrl+T)",
        "↑ / ↓",
        "select a thread (switches live)",
    ),
    ("Sidebar (Ctrl+T)", "Enter · Esc", "return to the editor"),
    ("Rename (Ctrl+R)", "Enter", "save the title"),
    (
        "Rename (Ctrl+R)",
        "⇧↵ / ⌥↵",
        "insert a newline into the title",
    ),
    ("Rename (Ctrl+R)", "Backspace", "delete backwards"),
    ("Rename (Ctrl+R)", "Esc", "cancel the rename"),
    ("Delete (Ctrl+D)", "Enter · y", "confirm the delete"),
    ("Delete (Ctrl+D)", "Esc · n", "cancel"),
    ("Model picker (Ctrl+M)", "↑ / ↓", "move the selection"),
    ("Model picker (Ctrl+M)", "PgUp / PgDn", "page the selection"),
    (
        "Model picker (Ctrl+M)",
        "Enter",
        "apply the highlighted model",
    ),
    ("Model picker (Ctrl+M)", "Esc", "close without switching"),
    (
        "Find in thread (Ctrl+F)",
        "text",
        "edit the query (live filter)",
    ),
    ("Find in thread (Ctrl+F)", "Backspace", "delete backwards"),
    ("Find in thread (Ctrl+F)", "↑ / ↓", "walk the matches"),
    (
        "Find in thread (Ctrl+F)",
        "Enter",
        "jump to the match · close",
    ),
    ("Find in thread (Ctrl+F)", "Esc", "close without jumping"),
    (
        "Find everywhere (Ctrl+⇧F)",
        "text",
        "edit the query (live filter)",
    ),
    ("Find everywhere (Ctrl+⇧F)", "Backspace", "delete backwards"),
    ("Find everywhere (Ctrl+⇧F)", "↑ / ↓", "walk the matches"),
    (
        "Find everywhere (Ctrl+⇧F)",
        "Enter",
        "activate the match's thread · jump",
    ),
    ("Find everywhere (Ctrl+⇧F)", "Esc", "close without jumping"),
    (
        "Log viewer (Ctrl+G)",
        "↑ / ↓ · k / j",
        "move the cursor one line",
    ),
    ("Log viewer (Ctrl+G)", "PgUp / PgDn", "page the view"),
    ("Log viewer (Ctrl+G)", "g / G", "jump to the top / the tail"),
    ("Log viewer (Ctrl+G)", "w", "toggle wrap"),
    ("Log viewer (Ctrl+G)", "/", "search the log"),
    ("Log viewer (Ctrl+G)", "n / N", "next / previous match"),
    ("Log viewer (Ctrl+G)", "y", "copy the cursor line"),
    ("Log viewer (Ctrl+G)", "Esc", "close"),
    ("Log search (/)", "text", "edit the pattern"),
    ("Log search (/)", "Backspace", "delete backwards"),
    ("Log search (/)", "Enter", "commit the search"),
    ("Log search (/)", "Esc", "cancel the search"),
    ("Error popup", "R", "reconnect"),
    ("Error popup", "q", "quit (confirmation)"),
    ("Error popup", "Esc · any key", "dismiss"),
    ("Quit confirm", "y · Enter", "quit"),
    ("Quit confirm", "Esc · n · q", "stay"),
    ("Help (F1)", "↑ / ↓ · PgUp / PgDn", "scroll the list"),
    ("Help (F1)", "F1 · Esc", "close"),
];

#[test]
fn help_modal_table_matches_the_enumerated_dispatch() {
    let table = chibi_tui::ui::HOTKEY_ROWS;
    assert_eq!(
        table.len(),
        DISPATCH_CHORDS.len(),
        "table row count drifted from the dispatch enumeration"
    );
    for (i, (row, (group, chord, action))) in table.iter().zip(DISPATCH_CHORDS.iter()).enumerate() {
        assert_eq!(row.group, *group, "row {i} group");
        assert_eq!(row.chord, *chord, "row {i} chord");
        assert_eq!(row.action, *action, "row {i} action");
    }
}

/// What the probe asserts about the draft after the chord was pressed.
#[derive(Clone, Copy, PartialEq)]
enum DraftEffect {
    /// The documented action does not touch the draft.
    Unchanged,
    /// The documented action empties the draft (Esc, ^U).
    Cleared,
    /// The documented action appends a newline (⇧↵ / ⌥↵).
    Newline,
    /// The documented action deletes backwards (Backspace).
    Shrink,
}

/// Concrete key events behind one Normal-mode table row, plus where the
/// press must happen (sidebar focus or editor focus) and the expected
/// draft effect. `text` rows are excluded: typing into the draft IS
/// their documented action, and the existing `type_in` coverage pins it.
/// The paste row (`Ctrl+V / Cmd+V`) is excluded too, but for the
/// OPPOSITE reason: pressing it hits the REAL system clipboard through
/// arboard, and parallel native clipboard access across the test
/// threads SIGSEGVs the test process on macOS (measured 2/10 full-suite
/// runs with the press, 0/10 at the baseline and without it). The row
/// stays pinned by the enumeration test above plus the dedicated
/// `ctrl_v_pastes_clipboard_into_input`, so coverage parity with the
/// pre-probe suite is preserved — one arboard caller, not three.
fn normal_mode_probes() -> Vec<(String, Vec<crossterm::event::KeyEvent>, DraftEffect, bool)> {
    use crossterm::event::KeyModifiers as M;
    let e = |code, m| key_event(code, m);
    vec![
        (
            "F1".into(),
            vec![e(KeyCode::F(1), M::NONE)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+C".into(),
            vec![e(KeyCode::Char('c'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+N".into(),
            vec![e(KeyCode::Char('n'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+P".into(),
            vec![e(KeyCode::Char('p'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+R".into(),
            vec![e(KeyCode::Char('r'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+D".into(),
            vec![e(KeyCode::Char('d'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+F".into(),
            vec![e(KeyCode::Char('f'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+Shift+F".into(),
            vec![e(KeyCode::Char('f'), M::CONTROL | M::SHIFT)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+T".into(),
            vec![e(KeyCode::Char('t'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+G".into(),
            vec![e(KeyCode::Char('g'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+O".into(),
            vec![e(KeyCode::Char('o'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+S".into(),
            vec![e(KeyCode::Char('s'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+M".into(),
            vec![e(KeyCode::Char('m'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+L".into(),
            vec![e(KeyCode::Char('l'), M::CONTROL)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+↑/↓ · Alt+↑/↓".into(),
            vec![e(KeyCode::Up, M::CONTROL), e(KeyCode::Up, M::ALT)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "PgUp / PgDn".into(),
            vec![e(KeyCode::PageUp, M::NONE), e(KeyCode::PageDown, M::NONE)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Esc".into(),
            vec![e(KeyCode::Esc, M::NONE)],
            DraftEffect::Cleared,
            false,
        ),
        (
            "Enter".into(),
            vec![e(KeyCode::Enter, M::NONE)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "⇧↵ / ⌥↵".into(),
            vec![e(KeyCode::Enter, M::SHIFT), e(KeyCode::Enter, M::ALT)],
            DraftEffect::Newline,
            false,
        ),
        (
            "↑ / ↓".into(),
            vec![e(KeyCode::Up, M::NONE), e(KeyCode::Down, M::NONE)],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+A / Ctrl+E".into(),
            vec![
                e(KeyCode::Char('a'), M::CONTROL),
                e(KeyCode::Char('e'), M::CONTROL),
            ],
            DraftEffect::Unchanged,
            false,
        ),
        (
            "Ctrl+U".into(),
            vec![e(KeyCode::Char('u'), M::CONTROL)],
            DraftEffect::Cleared,
            false,
        ),
        (
            "Backspace".into(),
            vec![e(KeyCode::Backspace, M::NONE)],
            DraftEffect::Shrink,
            false,
        ),
        (
            "↑ / ↓".into(),
            vec![e(KeyCode::Up, M::NONE), e(KeyCode::Down, M::NONE)],
            DraftEffect::Unchanged,
            true,
        ),
        (
            "Enter · Esc".into(),
            vec![e(KeyCode::Enter, M::NONE), e(KeyCode::Esc, M::NONE)],
            DraftEffect::Unchanged,
            true,
        ),
    ]
}

#[test]
fn help_modal_normal_mode_rows_are_claimed_by_the_live_dispatch() {
    use chibi_tui::app::Focus;
    for (chord, events, effect, sidebar) in normal_mode_probes() {
        for event in events {
            let mut app = app_with_chats(3);
            type_in(&mut app, "draft");
            if sidebar {
                press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
                assert_eq!(app.focus, Focus::Sidebar, "{chord}: setup");
            }
            press(&mut app, event.code, event.modifiers);
            // Newline-preserving join: a ⇧↵ probe must SEE the
            // multi-line draft (["draft", ""] → "draft\n").
            let after = app.input.lines().join("\n");
            match effect {
                DraftEffect::Unchanged => {
                    assert_eq!(after, "draft", "{chord}: typed into the draft");
                }
                DraftEffect::Cleared => {
                    assert_eq!(after, "", "{chord}: draft not cleared");
                }
                DraftEffect::Newline => {
                    assert_eq!(after, "draft\n", "{chord}: newline not inserted");
                }
                DraftEffect::Shrink => {
                    assert_eq!(after, "draf", "{chord}: backspace not applied");
                }
            }
        }
    }
}

/// Every in-modal row of the table must be captured by ITS popup: open
/// the popup, press the row's chord, the popup must still own the
/// keyboard afterwards (or close when the row says so), and nothing may
/// leak into the draft. Drift here means the modal lost a key the help
/// promises. Edge cases lean on the dispatch's honest no-ops: the
/// picker's Enter on a `Loading` listing and the search popups' Enter
/// with zero matches are guarded no-ops that keep the popup open.
#[test]
fn help_modal_in_modal_rows_are_captured_by_their_popups() {
    type PopupCase<'a> = (
        &'a dyn Fn(&mut chibi_tui::app::App),
        &'a [(KeyCode, KeyModifiers)],
        &'a [(KeyCode, KeyModifiers)],
    );
    let cases: Vec<PopupCase<'_>> = vec![
        (
            &|app: &mut chibi_tui::app::App| {
                press(app, KeyCode::Char('r'), KeyModifiers::CONTROL);
            },
            &[
                (KeyCode::Char('x'), KeyModifiers::NONE),
                (KeyCode::Backspace, KeyModifiers::NONE),
            ],
            &[(KeyCode::Esc, KeyModifiers::NONE)],
        ),
        (
            &|app: &mut chibi_tui::app::App| {
                press(app, KeyCode::Char('d'), KeyModifiers::CONTROL);
            },
            &[(KeyCode::Char('x'), KeyModifiers::NONE)],
            &[(KeyCode::Esc, KeyModifiers::NONE)],
        ),
        (
            &|app: &mut chibi_tui::app::App| {
                press(app, KeyCode::Char('m'), KeyModifiers::CONTROL);
            },
            &[
                (KeyCode::Up, KeyModifiers::NONE),
                (KeyCode::Down, KeyModifiers::NONE),
                (KeyCode::PageUp, KeyModifiers::NONE),
                (KeyCode::PageDown, KeyModifiers::NONE),
                (KeyCode::Enter, KeyModifiers::NONE),
                (KeyCode::Char('x'), KeyModifiers::NONE),
            ],
            &[(KeyCode::Esc, KeyModifiers::NONE)],
        ),
        (
            &|app: &mut chibi_tui::app::App| {
                press(app, KeyCode::Char('f'), KeyModifiers::CONTROL);
            },
            &[
                (KeyCode::Char('x'), KeyModifiers::NONE),
                (KeyCode::Backspace, KeyModifiers::NONE),
                (KeyCode::Up, KeyModifiers::NONE),
                (KeyCode::Down, KeyModifiers::NONE),
                (KeyCode::Enter, KeyModifiers::NONE),
            ],
            &[(KeyCode::Esc, KeyModifiers::NONE)],
        ),
        (
            &|app: &mut chibi_tui::app::App| {
                press(
                    app,
                    KeyCode::Char('f'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                );
            },
            &[
                (KeyCode::Char('x'), KeyModifiers::NONE),
                (KeyCode::Backspace, KeyModifiers::NONE),
                (KeyCode::Up, KeyModifiers::NONE),
                (KeyCode::Down, KeyModifiers::NONE),
                (KeyCode::Enter, KeyModifiers::NONE),
            ],
            &[(KeyCode::Esc, KeyModifiers::NONE)],
        ),
        (
            &|app: &mut chibi_tui::app::App| {
                press(app, KeyCode::Char('g'), KeyModifiers::CONTROL);
            },
            &[
                (KeyCode::Up, KeyModifiers::NONE),
                (KeyCode::Down, KeyModifiers::NONE),
                (KeyCode::PageUp, KeyModifiers::NONE),
                (KeyCode::PageDown, KeyModifiers::NONE),
                (KeyCode::Char('g'), KeyModifiers::NONE),
                (KeyCode::Char('G'), KeyModifiers::NONE),
                (KeyCode::Char('w'), KeyModifiers::NONE),
                (KeyCode::Char('/'), KeyModifiers::NONE),
                (KeyCode::Char('n'), KeyModifiers::NONE),
                (KeyCode::Char('N'), KeyModifiers::NONE),
                (KeyCode::Char('y'), KeyModifiers::NONE),
                (KeyCode::Char('x'), KeyModifiers::NONE),
            ],
            &[(KeyCode::Esc, KeyModifiers::NONE)],
        ),
    ];
    for (open, stay, close) in cases {
        for (code, mods) in stay {
            let mut app = app_with_chats(2);
            type_in(&mut app, "draft");
            open(&mut app);
            let which = std::mem::discriminant(&app.mode);
            press(&mut app, *code, *mods);
            assert_eq!(
                std::mem::discriminant(&app.mode),
                which,
                "{code:?} must stay inside the popup"
            );
            assert!(
                app.input.lines().join("").starts_with("draft"),
                "{code:?} leaked into the draft"
            );
        }
        for (code, mods) in close {
            let mut app = app_with_chats(2);
            open(&mut app);
            press(&mut app, *code, *mods);
            assert!(app.mode.is_normal(), "{code:?} must close the popup");
        }
    }
}

#[test]
fn f1_toggles_the_help_modal_and_esc_closes() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "draft");
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    assert!(matches!(app.mode, chibi_tui::app::Mode::HelpViewing { .. }));
    // Same-chord toggle: F1 closes, F1 reopens.
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    assert!(matches!(app.mode, chibi_tui::app::Mode::HelpViewing { .. }));
    // Esc closes too; the draft survives the round trip.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert_eq!(app.input.lines().join(""), "draft");
    // Enter inside the modal is swallowed: it can never submit.
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(app.mode, chibi_tui::app::Mode::HelpViewing { .. }));
    assert_eq!(app.input.lines().join(""), "draft");
}

#[test]
fn f1_opens_the_help_modal_from_sidebar_focus() {
    use chibi_tui::app::Focus;
    let mut app = app_with_chats(2);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, Focus::Sidebar);
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    assert!(matches!(app.mode, chibi_tui::app::Mode::HelpViewing { .. }));
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert_eq!(app.focus, Focus::Chat, "closing a modal returns the editor");
}

#[test]
fn help_modal_does_not_open_over_other_popups() {
    // The modal family is strictly one-at-a-time: entry is Normal-only.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }),
        "the viewer keeps the keyboard; F1 is swallowed"
    );
}

#[test]
fn help_modal_scroll_keys_page_the_window() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    let total = chibi_tui::ui::help_modal_total_lines();
    assert!(total > 10, "the table must have content: {total}");
    let page = app.help_visible_rows.max(1) as usize;
    let scroll = |app: &chibi_tui::app::App| match &app.mode {
        chibi_tui::app::Mode::HelpViewing { state } => state.scroll,
        _ => panic!("modal must stay open"),
    };
    assert_eq!(scroll(&app), 0, "opens at the top");

    // ↓ walks one line at a time; ↑ back to the top clamp.
    app.help_scroll_down();
    assert_eq!(scroll(&app), 1);
    app.help_scroll_up();
    assert_eq!(scroll(&app), 0);
    app.help_scroll_up();
    assert_eq!(scroll(&app), 0, "clamped at the top, no wraparound");

    // PgDn pages by the render-fed viewport; PgUp returns; bottom clamp
    // pins the window at the table's last row.
    app.help_page_down();
    assert_eq!(scroll(&app), page);
    app.help_page_up();
    assert_eq!(scroll(&app), 0);
    let many = (total / page) + 2;
    for _ in 0..many {
        app.help_page_down();
    }
    assert_eq!(
        scroll(&app),
        total.saturating_sub(page),
        "clamped at the bottom edge"
    );
    // One-line scrolls respect the same bottom clamp.
    for _ in 0..3 {
        app.help_scroll_down();
    }
    assert_eq!(scroll(&app), total.saturating_sub(page));
}
