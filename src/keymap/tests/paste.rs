use crate::tests::support::*;

use crate::*;

use crossterm::event::{KeyCode, KeyModifiers};

/// A bracketed multi-line paste inserts ALL lines into the draft as hard
/// newlines and NEVER submits — the paste event carries no key, so bare
/// Enter (the only submit path) can never fire from a paste. Regression
/// for the hand-tested bug: a three-line paste used to fire three
/// submits because the terminal delivered the paste as raw keystrokes
/// (no bracketed-paste mode) and each embedded newline decoded into a
/// submitting Enter.
#[test]
fn bracketed_multiline_paste_inserts_lines_without_submitting() {
    let mut app = app_with_chats(1);
    handle_paste(&mut app, "line one\nline two\nline three");
    assert_eq!(
        app.input.lines(),
        ["line one", "line two", "line three"],
        "the whole paste lands in the draft, newlines intact"
    );
    assert_eq!(app.input.cursor(), (2, 10));
    // No submit happened: the draft is still there for the user to send.
    assert!(app.take_input().is_some());
}

/// Windows (`\r\n`) and classic-Mac (`\r`) line endings normalize to
/// `\n` on paste — a foreign clipboard never leaves stray `\r` chars in
/// the draft.
#[test]
fn paste_normalizes_crlf_and_bare_cr_to_newlines() {
    let mut app = app_with_chats(1);
    handle_paste(&mut app, "alpha\r\nbeta\rgamma");
    assert_eq!(app.input.lines(), ["alpha", "beta", "gamma"]);
}

/// Pasting into a non-empty draft splits lines at the caret: the tail of
/// the caret's line wraps to the last pasted line, exactly like
/// tui-textarea 0.7's `insert_str`.
#[test]
fn paste_mid_text_splits_lines_at_caret() {
    let mut app = app_with_chats(1);
    for ch in "hello".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    // Walk the caret back into the middle of "hello" (between 'l' and 'o').
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    handle_paste(&mut app, "X\nY");
    assert_eq!(app.input.lines(), ["hellX", "Yo"]);
    assert_eq!(app.input.cursor(), (1, 1));
}

/// Paste is editor-scoped: while the sidebar owns the keyboard, a
/// bracketed paste must be swallowed — it can never fill a draft the
/// sidebar's own Enter would submit.
#[test]
fn paste_into_sidebar_focus_is_swallowed() {
    let mut app = app_with_chats(1);
    app.focus = Focus::Sidebar;
    handle_paste(&mut app, "should not land\nanywhere");
    assert!(app.input.lines().iter().all(|l| l.is_empty()));
}

/// A bracketed paste while the quit-confirm popup is open is swallowed:
/// the popup is a flag (not a Mode), so `handle_paste` checks it
/// explicitly — pasted text must never mutate the draft sitting under
/// the popup (a paste-then-Enter there used to SEND the pasted text on
/// quit-confirm).
#[test]
fn paste_while_quit_confirm_is_open_is_swallowed() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "keep me");
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm, "precondition");

    handle_paste(&mut app, "pasted\njunk");
    assert_eq!(
        app.input.lines(),
        ["keep me"],
        "the paste must not reach the draft under the popup"
    );
    assert!(app.quit_confirm, "the popup itself is unaffected");

    // And a dismissal leaves the exact prior draft.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.quit_confirm);
    assert_eq!(app.input.lines(), ["keep me"]);
}

/// A Shift+Enter-typed two-line draft submits the prompt with the
/// newline intact. Pins the submit chain end to end: whatever the
/// editor shows (`["alpha", "beta"]`) is what `take_input` extracts —
/// the backend receives `alpha\nbeta`, never the word-glued
/// `alphabeta`.
#[test]
fn shift_enter_multiline_draft_submits_prompt_with_newlines() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "alpha");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_in(&mut app, "beta");
    assert_eq!(app.input.lines(), ["alpha", "beta"], "precondition");

    let submitted = app.take_input().expect("prompt taken");
    assert_eq!(
        submitted.prompt, "alpha\nbeta",
        "the submitted prompt carries the Shift+Enter newline verbatim"
    );
}

/// A pasted multi-line draft submits the prompt with the newlines
/// intact. Regression pin for the reported word-gluing: the OLD
/// Ctrl+V path on main filtered `\n`/`\r` out of the clipboard text
/// outright, so `line one\nline two` reached the backend as the glued
/// single word-merged string `line oneline two`. The paste now inserts
/// hard newlines and the extraction joins with `\n`.
#[test]
fn pasted_multiline_draft_submits_prompt_with_newlines() {
    let mut app = app_with_chats(1);
    handle_paste(&mut app, "line one\nline two");
    assert_eq!(app.input.lines(), ["line one", "line two"], "precondition");

    let submitted = app.take_input().expect("prompt taken");
    assert_eq!(
        submitted.prompt, "line one\nline two",
        "paste newlines must reach the wire prompt, not be stripped away"
    );
    assert!(
        !submitted.prompt.contains("oneline"),
        "words from consecutive lines must never glue together"
    );
}

/// A multi-line prompt submitted while a request is in flight enqueues
/// with its newline intact and drains verbatim — the queued path
/// preserves newlines exactly like the immediate-send path.
#[test]
fn queued_multiline_prompt_preserves_newlines_through_the_fifo() {
    let mut app = app_with_chats(1);
    let first = submit_text(&mut app, "in flight");

    // Busy chat: the multi-line draft goes to the FIFO, not the wire.
    type_in(&mut app, "queued one");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_in(&mut app, "queued two");
    assert!(
        app.take_input().is_none(),
        "busy chat enqueues, no immediate send"
    );

    let thread = app.chats[0].id.clone();
    let drained = app
        .dequeue_next_for(&thread)
        .expect("queued prompt drained");
    assert_eq!(
        drained.prompt, "queued one\nqueued two",
        "the queued prompt keeps its newline through enqueue and drain"
    );
    assert_ne!(drained.request_id, first.request_id);
}
