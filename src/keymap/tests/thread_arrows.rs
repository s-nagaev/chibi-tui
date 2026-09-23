use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

// thread switching + caret movement ------------

/// Ctrl+↑ / Ctrl+↓ switch the active thread with EXACTLY the old plain-
/// arrow semantics: bounds-clamped selection move + chat-scroll reset.
/// (The sidebar dot/focus refresh derives from `active` at draw time.)
#[test]
fn ctrl_up_and_ctrl_down_switch_active_thread_like_plain_arrows_did() {
    let mut app = app_with_chats(3);

    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(app.active, 1);
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(app.active, 2);
    // Bounds clamp at the list end — no wrap-around.
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(app.active, 2);

    // Scroll-reset semantics: stored chat scroll clears on any switch.
    app.scroll = 9;
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(app.active, 1);
    assert_eq!(app.scroll, 0, "thread switch resets chat scroll");

    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(app.active, 0);
    // Bounds clamp at the top — saturating, no panic.
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(app.active, 0);
}

/// Alt+↑ / Alt+↓ are a FULL SYNONYM of Ctrl+↑/↓ —
/// identical semantics in both directions, bounds-clamped at the list
/// edges, chat-scroll reset on every switch. (macOS Mission Control
/// hijacks Ctrl+arrows system-wide, so this is the stock-macOS path.)
#[test]
fn alt_up_and_alt_down_switch_active_thread_like_ctrl_arrows() {
    let mut app = app_with_chats(3);

    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.active, 1);
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.active, 2);
    // Bounds clamp at the list end — no wrap-around.
    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.active, 2);

    // Scroll-reset semantics: stored chat scroll clears on any switch.
    app.scroll = 9;
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(app.active, 1);
    assert_eq!(app.scroll, 0, "thread switch resets chat scroll");

    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(app.active, 0);
    // Bounds clamp at the top — saturating, no panic.
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(app.active, 0);
}

/// Alt-synonym parity: Alt+arrows work with a live multi-line
/// draft and never disturb it — buffer verbatim, nothing submitted/
/// queued/in-flight. And plain ↑/↓ STILL move only the caret (regression:
/// the alt synonym must not leak into the plain-arrow path).
#[test]
fn alt_arrows_switch_threads_without_disturbing_multiline_draft() {
    let mut app = app_with_chats(2);
    type_in(&mut app, "line one");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_in(&mut app, "line two");
    let draft_before = app.input.lines().to_vec();
    assert_eq!(draft_before, ["line one", "line two"]);

    press(&mut app, KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.active, 1, "Alt+Down switched threads");
    press(&mut app, KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(app.active, 0, "Alt+Up switched back");

    assert_eq!(app.input.lines(), draft_before, "draft untouched");
    assert!(app.chats.iter().all(|c| c.messages.is_empty()));
    assert!(app.active_request_id().is_none());
    assert_eq!(app.active_queue_len(), 0);

    // Plain arrows remain caret-only after alt navigation — the caret
    // still sits at the end of row 1 (the draft is 2 lines tall).
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active, 0, "plain Down must not switch threads");
    assert_eq!(
        app.input.cursor(),
        (1, 8),
        "plain Down must stay caret-level (clamped on the last row)"
    );
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active, 0, "plain Up must not switch threads");
    assert_eq!(
        app.input.cursor(),
        (0, 8),
        "plain Up must move the caret to the previous row"
    );
}

/// Ctrl+arrows work with a live multi-line draft and never disturb it:
/// buffer verbatim, nothing submitted/queued/in-flight.
#[test]
fn ctrl_arrows_switch_threads_without_disturbing_multiline_draft() {
    let mut app = app_with_chats(2);
    type_in(&mut app, "line one");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_in(&mut app, "line two");
    let draft_before = app.input.lines().to_vec();
    assert_eq!(draft_before, ["line one", "line two"]);

    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(app.active, 1, "Ctrl+Down switched threads");
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(app.active, 0, "Ctrl+Up switched back");

    assert_eq!(app.input.lines(), draft_before, "draft untouched");
    assert!(app.chats.iter().all(|c| c.messages.is_empty()));
    assert!(app.active_request_id().is_none());
    assert_eq!(app.active_queue_len(), 0);
}

/// Simulated Windows Terminal / partial-kitty event shapes: terminals
/// with incomplete modifier reporting may deliver the Ctrl+↑/↓ chords
/// with EXTRA modifiers riding along (SHIFT when the terminal reports
/// the raw shift state, ALT when both chord flavors are registered).
/// The thread-switch arms match CONTROL with `KeyModifiers::contains`,
/// so every superset shape must still switch threads. (The plain legacy
/// shapes — bare CONTROL on Up/Down, the `ESC[1;5A`/`ESC[1;5B`
/// encodings — are pinned by the ctrl-arrows tests above; both encodings
/// decode into the very same `KeyEvent`.)
#[test]
fn ctrl_arrow_shapes_with_extra_riding_modifiers_still_switch_threads() {
    for mods in [
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT,
    ] {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Down, mods);
        assert_eq!(app.active, 1, "Down + {mods:?} must switch threads");
        assert_eq!(
            app.scroll, 0,
            "Down + {mods:?} keeps the scroll-reset semantics"
        );
        app.scroll = 7;
        press(&mut app, KeyCode::Up, mods);
        assert_eq!(app.active, 0, "Up + {mods:?} must switch threads back");
        assert_eq!(app.scroll, 0, "Up + {mods:?} resets the chat scroll");
    }
}

/// The Alt synonym under the same partial-kitty shapes: SHIFT riding on
/// Alt+↑/↓ must not break the thread switch (kitty-capable terminals
/// report the shift state on the alt flavor too).
#[test]
fn alt_arrow_shapes_with_shift_riding_along_still_switch_threads() {
    for mods in [
        KeyModifiers::ALT | KeyModifiers::SHIFT,
        KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL,
    ] {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Down, mods);
        assert_eq!(app.active, 1, "Alt-flavored Down + {mods:?} switches");
        press(&mut app, KeyCode::Up, mods);
        assert_eq!(app.active, 0, "Alt-flavored Up + {mods:?} switches back");
    }
}
