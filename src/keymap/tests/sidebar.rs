use crate::tests::support::*;

use crate::*;

use crossterm::event::{KeyCode, KeyModifiers};

/// Plain ↑/↓ move the text cursor vertically inside the editor — never    /// Plain ↑/↓ move the text cursor vertically inside the editor — never
/// switching threads. Column preserved across rows (readline-native
/// mapping), clamped at the first/last row. This exercises the grown
/// (MAX_INPUT_LINES-capped) block's caret navigation path.
#[test]
fn plain_vertical_arrows_move_caret_not_thread() {
    let mut app = app_with_chats(3);
    type_in(&mut app, "one");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT); // caret → row 1
    type_in(&mut app, "two");
    assert_eq!(app.input.cursor(), (1, 3));
    assert_eq!(app.input.lines(), ["one", "two"]);

    // Down on the LAST row: clamped no-op; absolutely no thread change.
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active, 0);
    assert_eq!(app.input.cursor(), (1, 3));

    // Up moves to the previous row, column preserved.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active, 0, "plain Up must not switch threads");
    assert_eq!(app.input.cursor(), (0, 3));

    // Up on the FIRST row: clamped no-op.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active, 0);
    assert_eq!(app.input.cursor(), (0, 3));

    // Down returns the caret to where it was.
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.input.cursor(), (1, 3));
    assert_eq!(app.active, 0);
}

/// Required regression guard: in a SINGLE-LINE input, plain ↑ / ↓ neither
/// change the active thread nor leak into the submission machinery —
/// buffer stays verbatim, no request begins/cancels/quits, and the
/// loop-level gate still rejects every arrow variant.
#[test]
fn plain_arrows_single_line_no_thread_change_and_no_submit() {
    let mut app = app_with_chats(2);
    type_in(&mut app, "abc");
    assert_eq!(app.input.cursor(), (0, 3));

    for code in [KeyCode::Up, KeyCode::Down] {
        press(&mut app, code, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "{code:?} must not switch threads");
        assert_eq!(app.input.lines(), ["abc"], "{code:?} must not edit");
        assert!(
            app.active_request_id().is_none(),
            "{code:?} must not submit"
        );
        assert!(app.pending_cancel.is_none());
        assert!(!app.should_quit);
    }
    // And the event-loop gate agrees: arrows are never submit keys.
    for code in [KeyCode::Up, KeyCode::Down] {
        assert!(!should_submit(&key_event(code, KeyModifiers::NONE)));
    }
}

// key routing --------------------------------------

/// THE round-trip criterion through the full key path: Ctrl+T toggles
/// pane FOCUS — Chat → Sidebar → Chat. It must not move the selection
/// itself (the old wrap-cycling semantics were rejected outright).
#[test]
fn ctrl_t_toggles_pane_focus_and_round_trips() {
    let mut app = app_with_chats(3);
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "default focus");

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(
        app.focus,
        chibi_tui::app::Focus::Chat,
        "Ctrl+T twice round-trips"
    );

    // Pure focus flip: no navigation happened.
    assert_eq!(app.active, 0);
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(app.chats.len(), 3);

    // Arrows remain clamped in Normal mode (regression for the removed
    // wrap semantics — clamping never depended on Ctrl+T).
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(app.active, 1);
    app.active = 2;
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(app.active, 2, "Ctrl+Down still clamps at the last thread");
}

/// Sidebar-focused arrows navigate the selection with LIVE active-chat
/// switching and CLAMP at the edges; each switch resets the chat scroll
/// to follow-bottom (same mechanics as today's normal-mode arrows).
#[test]
fn sidebar_arrows_navigate_live_switch_and_clamp() {
    let mut app = app_with_chats(3);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

    app.scroll = 42; // detach from bottom before switching
    assert!(!app.at_bottom());

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active, 1, "sidebar ↓ selects the next chat");
    assert_eq!(app.scroll, 0, "chat view reset to follow-bottom");

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active, 2);
    // Clamp at the last edge…
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active, 2, "↓ clamps at the last thread");
    // …and back up to the first.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active, 0, "↑ clamps at the first thread");
}

/// Bare Enter on a Sidebar-focused UI applies and returns focus to Chat
/// WITHOUT submitting the draft or disturbing it (the event loop's
/// `enter_consumed_by_sidebar` gate suppresses submission for exactly
/// this key — handle_key must leave no side effects behind).
#[test]
fn sidebar_enter_returns_focus_without_submitting() {
    let mut app = app_with_chats(3);
    type_in(&mut app, "half typed");
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
    assert_eq!(app.input.lines(), ["half typed"], "draft untouched");
    assert!(app.chats.iter().all(|c| c.messages.is_empty()), "no submit");
    assert!(app.active_request_id().is_none());
    assert_eq!(app.active_queue_len(), 0);
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    // Loop-gate parity: bare Enter stays a submit-shaped key; only the
    // was_sidebar_focused snapshot can consume it — covered there.
    assert!(should_submit(&key_event(
        KeyCode::Enter,
        KeyModifiers::NONE
    )));
}

/// Esc on a Sidebar-focused UI returns focus to Chat WITHOUT clearing
/// the draft — deliberately different from Normal-mode Esc (which clears
/// non-empty input).
#[test]
fn sidebar_esc_returns_focus_and_preserves_draft() {
    let mut app = app_with_chats(3);
    type_in(&mut app, "keep me");
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
    assert_eq!(
        app.input.lines(),
        ["keep me"],
        "Esc must NOT clear the draft"
    );
}

/// Typing and text-editing keystrokes are SWALLOWED while the sidebar is
/// focused — nothing reaches the textarea: plain chars, backspace,
/// Shift+Enter newline inserts, even Space.
#[test]
fn typing_is_swallowed_while_sidebar_focused() {
    let mut app = app_with_chats(3);
    type_in(&mut app, "draft");
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT); // newline insert
    press(&mut app, KeyCode::Enter, KeyModifiers::ALT); // alt-newline

    assert_eq!(app.input.lines(), ["draft"], "textarea untouched");
    assert_eq!(
        app.focus,
        chibi_tui::app::Focus::Sidebar,
        "swallowed keys keep the focus"
    );
}

/// PgUp/PgDn STILL scroll the CHAT pane while the sidebar holds focus
/// (documented choice: reading works regardless of focus).
#[test]
fn pgup_pgdn_scroll_chat_while_sidebar_focused() {
    let mut app = app_with_chats(2);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(app.scroll, 20, "one page up by visible rows");
    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(app.scroll, 0, "back to follow-bottom");
}

/// Global service chords stay live with their exact Normal-mode
/// semantics while the sidebar holds focus: ^F / ^⇧F open searches,
/// ^D opens the guarded confirm popup, ^L opens the stop confirm popup,
/// ^R enters rename — and closing any of them hands focus back to Chat.
#[test]
fn service_chords_stay_live_while_sidebar_focused() {
    // Each chord gets its own fresh app so lifecycles can't interfere.

    // ^F in-thread search opens and closes back onto Chat.
    let mut app = app_with_chats(1);
    submit_text(&mut app, "hello world");
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
    app.search_push('h');
    app.search_push('e');
    app.search_push('l');
    app.search_push('l');
    app.search_push('o');
    assert!(!app.search_matches().is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // jump & close
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "popup close resets");

    // ^⇧F global search opens from the sidebar too.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::SearchingAll { .. }
    ));
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // popup dismisses
    assert!(app.mode.is_normal());
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "popup close resets");

    // ^D opens the confirm popup from the sidebar (idle chat).
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
    assert_eq!(app.chats.len(), 1, "nothing deleted yet");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // cancel via modal arm
    assert!(app.mode.is_normal());
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "popup close resets");

    // ^L opens the stop confirm while Sidebar focused (idle chat would be
    // a no-op; a busy chat opens the popup — parity with Normal mode).
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);
    submit_text(&mut app, "in flight");
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Stop
        }
    ));
    app.cancel_stop_reset();

    // ^R renames from the sidebar; committing returns focus to Chat.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Renaming { .. }));
    app.rename_push('z');
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save via modal arm
    assert!(app.mode.is_normal());
    assert_eq!(app.chat_title(), "chat-0z");
    assert_eq!(
        app.focus,
        chibi_tui::app::Focus::Chat,
        "rename close resets"
    );
}

/// ^N new-chat works under Sidebar focus and lands focus on Chat with
/// the fresh chat selected (editor-bound action).
#[test]
fn ctrl_n_from_sidebar_lands_focus_on_chat() {
    let mut app = app_with_chats(2);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);

    assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "^N lands on Chat");
    assert_eq!(app.active, 0, "the new chat is selected (and on top)");
    assert_eq!(app.chats.len(), 3);
    assert!(app.at_bottom(), "scroll reset to follow-bottom");
}

/// Busy chats participate normally under Sidebar navigation: switching
/// away from a running chat is allowed and the
/// highlighted thread follows the work — nothing in the old cycle path
/// cared about lifecycles either.
#[test]
fn sidebar_navigation_across_busy_chats() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "in flight"); // chat 0 busy
    assert!(app.is_busy());

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active, 1, "switching away from a busy chat is allowed");
    assert!(!app.is_busy(), "active chat is the idle one");
    assert!(app.any_busy(), "background chat still runs");

    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active, 0);
    assert!(app.is_busy(), "back onto the busy chat");
}

/// Single chat: toggling is still a valid pure focus flip — and with the
/// sidebar focused the arrow navigation is a graceful clamp-noop.
#[test]
fn ctrl_t_single_chat_toggles_focus_without_navigating() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
    assert_eq!(app.active, 0);

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active, 0, "clamped nav around one chat");
    assert!(app.status_message.is_none(), "no toast spam");
    assert!(app.error_popup.is_none(), "no popup");
    assert!(!app.should_quit);
}

/// Zero chats: toggle and navigation stay silent no-ops through the full
/// key path — no panic anywhere.
#[test]
fn ctrl_t_zero_chats_is_silent_noop() {
    let mut app = chibi_tui::app::App::new(Vec::new());
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active, 0);
    assert!(!app.should_quit);
    assert!(app.status_message.is_none());

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
}

/// Modal swallowing: with the GLOBAL search popup open, Ctrl+T goes to
/// the popup (swallowed, popup stays, focus unchanged) — never to the
/// focus flip or navigation.
#[test]
fn ctrl_t_swallowed_by_global_search_popup() {
    let mut app = app_with_chats(3);
    press(
        &mut app,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::SearchingAll { .. }
    ));

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    assert!(
        matches!(app.mode, chibi_tui::app::Mode::SearchingAll { .. }),
        "popup must stay open"
    );
    assert_eq!(
        app.focus,
        chibi_tui::app::Focus::Chat,
        "no focus flip through the popup"
    );
    assert_eq!(app.active, 0, "Ctrl+T must not navigate through the popup");
}

/// Modal swallowing: the delete-confirm popup swallows Ctrl+T too.
#[test]
fn ctrl_t_swallowed_by_delete_confirm_popup() {
    let mut app = app_with_chats(3);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
    assert_eq!(app.active, 0, "no navigation through the popup");
    assert_eq!(app.chats.len(), 3, "nothing deleted");
}

/// Modal swallowing: the in-thread search popup swallows Ctrl+T too.
#[test]
fn ctrl_t_swallowed_by_in_thread_search_popup() {
    let mut app = app_with_chats(3);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
    assert_eq!(app.active, 0, "no navigation through the popup");
}

/// Renaming blocks Ctrl+T like every other chord: its branch takes
/// precedence (the rename branch returns before the focus branch runs),
/// so the chord never flips focus while the rename editor is open.
/// The letter lands in the draft instead — the rename arm pushes ANY
/// `Char` regardless of modifiers, exactly like Ctrl+N pushes 'n' there
/// (documented branch behavior, unchanged by this feature).
#[test]
fn ctrl_t_blocked_while_renaming() {
    let mut app = app_with_chats(3);
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::Renaming { .. }));

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    assert!(
        matches!(app.mode, chibi_tui::app::Mode::Renaming { .. }),
        "rename session must stay open"
    );
    assert_eq!(app.active, 0, "no thread switch mid-rename");
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
    // The rename branch consumed the chord: the letter went into the
    // draft (the arm accepts any Char), never into navigation.
    assert_eq!(
        app.rename_buf(),
        Some("chat-0t"),
        "rename branch precedence: 't' lands in the draft"
    );
}

/// Ctrl+T works with a live multi-line draft and never disturbs it:
/// buffer verbatim, nothing submitted/queued/in-flight.
#[test]
fn ctrl_t_switches_focus_without_disturbing_multiline_draft() {
    let mut app = app_with_chats(2);
    type_in(&mut app, "line one");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_in(&mut app, "line two");
    let draft_before = app.input.lines().to_vec();
    assert_eq!(draft_before, ["line one", "line two"]);

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar, "focus flipped");

    assert_eq!(app.input.lines(), draft_before, "draft untouched");
    assert!(app.chats.iter().all(|c| c.messages.is_empty()));
    assert!(app.active_request_id().is_none());
    assert_eq!(app.active_queue_len(), 0);
}
