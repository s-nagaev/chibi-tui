use crate::tests::support::*;

use crate::*;

use crossterm::event::{KeyCode, KeyModifiers};

// ---- cancel hotkey -----------------------------------------------------

#[test]
fn ctrl_c_while_busy_requests_cancel_instead_of_quit() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "long running");
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);

    assert!(app.pending_cancel.is_some(), "cancel requested");
    let (request_id, _) = app.pending_cancel.clone().unwrap();
    assert_eq!(Some(request_id.as_str()), app.active_request_id());
    assert_eq!(request_id, submitted.request_id);
    assert!(!app.should_quit, "busy Ctrl+C must not quit");
    assert!(
        !app.quit_confirm,
        "busy Ctrl+C must not open the quit confirmation"
    );
}

/// Ctrl+C with queued prompts still cancels only the in-flight request;
/// the queue is untouched (it drains via terminal events).
#[test]
fn ctrl_c_with_queued_prompts_cancels_inflight_only() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "in flight");
    // Enqueue a second prompt while busy: type the text, then submit via
    // the same path the event loop uses (take_input enqueues because the
    // chat is busy).
    for ch in "queued".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    assert!(app.take_input().is_none(), "busy chat enqueues");
    assert_eq!(app.active_queue_len(), 1);

    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(
        app.pending_cancel.is_some(),
        "cancel targets in-flight only"
    );
    assert_eq!(app.active_queue_len(), 1, "queue survives the cancel");
    assert!(!app.should_quit);
    assert!(!app.quit_confirm, "cancelling must not open the confirm");
}

/// Idle Ctrl+C no longer quits directly: it opens the quit
/// confirmation; `y`/Enter then performs the same shutdown transition
/// the old direct quit used to.
#[test]
fn ctrl_c_when_idle_opens_quit_confirmation() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.pending_cancel.is_none(), "nothing to cancel");
    assert!(app.quit_confirm, "idle Ctrl+C must open the confirmation");
    assert!(!app.should_quit, "opening the confirm must not quit");

    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(app.should_quit, "y confirms the quit");
    assert!(!app.quit_confirm, "popup closed by the confirm");
}

/// Enter confirms the quit too (same grammar as the other popups).
#[test]
fn quit_confirm_enter_quits() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.should_quit, "Enter confirms the quit");
}

/// Enter over a NON-EMPTY draft with the quit confirm open must confirm
/// the quit WITHOUT also submitting the draft underneath (integration
/// leak: the popup's confirming Enter used to reach the run_loop
/// submission chain and send the message right before shutdown). The
/// loop-gate replica below mirrors the run_loop snapshot + submission
/// chain exactly.
#[test]
fn quit_confirm_enter_with_nonempty_draft_does_not_submit() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "precious draft");
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm, "precondition: popup open over a draft");

    // The run_loop gate replica: snapshot BEFORE the key, then the
    // submission chain conditions in the same order.
    let key = key_event(KeyCode::Enter, KeyModifiers::NONE);
    let was_quit_confirm = app.quit_confirm;
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let enter_consumed_by_quit_confirm = was_quit_confirm && key.code == KeyCode::Enter;
    let would_submit =
        app.error_popup.is_none() && should_submit(&key) && !enter_consumed_by_quit_confirm;

    assert!(app.should_quit, "Enter still confirms the quit");
    assert!(!app.quit_confirm, "popup closed by the confirm");
    assert!(
        !would_submit,
        "the confirming Enter must be consumed by the popup, never submitted"
    );
    assert!(
        app.take_input().is_some(),
        "the draft was never sent — it is still intact in the buffer"
    );
    assert_eq!(app.active_queue_len(), 0, "nothing enqueued either");
}

/// While the quit confirm is open nothing leaks: no typing reaches the
/// textarea, no global binding fires, and `q` — which can never confirm
/// a quit — dismisses the popup (documented choice: `q` is treated as
/// "stay", matching the unbound treatment it gets in the destructive
/// popups).
#[test]
fn quit_confirm_isolates_keystrokes_and_q_dismisses() {
    let mut app = app_with_chats(3);
    type_in(&mut app, "precious draft");
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm);

    // Typing must not reach the textarea (letters a/b/c avoid the
    // popup's own y/n decision keys).
    type_in(&mut app, "abc");
    // Global bindings suspended.
    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(app.chats.len(), 3, "Ctrl+N must not fire mid-popup");
    assert_eq!(
        app.mode,
        chibi_tui::app::Mode::Normal,
        "Ctrl+D must not fire"
    );
    assert_eq!(app.active, 0, "thread switching must not fire");
    assert!(app.quit_confirm, "popup must stay open");

    // q is a DISMISS here, never a quit.
    press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(!app.quit_confirm, "q dismisses the confirm");
    assert!(!app.should_quit, "q must never quit from the confirm");

    // The prior state is exactly as it was.
    assert_eq!(
        app.input.lines().join(""),
        "precious draft",
        "no keystroke leaked into the textarea"
    );
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
}

/// Dismissing (n/Esc) restores the exact prior state: focus, mode and
/// the draft all survive untouched — the popup is a flag, so opening
/// it never changed anything to restore.
#[test]
fn quit_confirm_dismiss_restores_prior_state() {
    // Sidebar focus + a draft + a rename-cancelled... keep it simple:
    // sidebar focus, draft text, then idle Ctrl+C → Esc.
    let mut app = app_with_chats(3);
    type_in(&mut app, "half-typed prompt");
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.quit_confirm);
    assert!(!app.should_quit);
    assert_eq!(
        app.focus,
        chibi_tui::app::Focus::Sidebar,
        "focus restored untouched"
    );
    assert_eq!(
        app.input.lines().join(""),
        "half-typed prompt",
        "draft restored untouched"
    );
    assert_eq!(app.mode, chibi_tui::app::Mode::Normal);

    // And 'n' dismisses identically.
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(!app.quit_confirm);
    assert!(!app.should_quit);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
}

/// Opening the confirm is cancel-safe: with a request in flight the
/// quit confirm (reached through the error popup's q, since busy
/// Ctrl+C is a cancellation) must NOT touch the request — cancellation
/// still goes through its own Ctrl+C path only.
#[test]
fn opening_quit_confirm_does_not_cancel_inflight_request() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "long running");
    app.show_error("boom");

    press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(app.quit_confirm);
    assert!(app.pending_cancel.is_none(), "no cancellation was staged");
    assert_eq!(
        app.active_request_id(),
        Some(submitted.request_id.as_str()),
        "the in-flight request keeps streaming"
    );

    // Dismiss: the request is STILL untouched.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.quit_confirm);
    assert_eq!(app.active_request_id(), Some(submitted.request_id.as_str()));

    // The own cancel path still works: dismiss the error popup first,
    // then busy Ctrl+C cancels — it never opens the confirm.
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(app.error_popup.is_none(), "popup dismissed");
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.pending_cancel.is_some(), "cancel path intact");
    assert!(!app.quit_confirm, "busy Ctrl+C never opens the confirm");
}

// ^P routing --------------------------------------

#[test]
fn ctrl_p_routes_to_clone_flow_when_supported() {
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/new_thread_with_current_context".to_owned()]);
    press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);

    assert!(
        app.take_clone_submission().is_some(),
        "^P stages the clone request"
    );
}

#[test]
fn ctrl_p_without_capability_shows_popup_and_stages_nothing() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);

    assert!(app.error_popup.is_some(), "informative popup");
    assert!(app.take_clone_submission().is_none());
}

#[test]
fn ctrl_p_works_from_sidebar_focus() {
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/new_thread_with_current_context".to_owned()]);
    app.focus = Focus::Sidebar;
    press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);

    assert!(
        app.take_clone_submission().is_some(),
        "sidebar holds focus but the service chord still fires"
    );
}
