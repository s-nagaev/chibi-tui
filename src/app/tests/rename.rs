use super::support::*;
use crate::app::*;

#[test]
fn ctrl_r_enters_rename_mode_prefilled_with_current_name() {
    let mut app = app_with_chats(2);
    assert_eq!(app.mode, Mode::Normal);

    press_ctrl_r(&mut app);
    assert_eq!(
        app.mode,
        Mode::Renaming {
            buf: "chat-0".to_owned()
        },
        "draft must be pre-filled with the active chat's name"
    );
}

#[test]
fn rename_typing_and_backspace_edit_the_draft_only() {
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);

    for ch in " v2".chars() {
        app.rename_push(ch);
    }
    assert_eq!(
        app.mode,
        Mode::Renaming {
            buf: "chat-0 v2".to_owned()
        }
    );
    // Prompt input untouched by rename editing.
    assert!(app.input.lines().iter().all(|l| l.is_empty()));

    app.rename_backspace();
    app.rename_backspace();
    assert_eq!(app.rename_buf(), Some("chat-0 "));
    // Chat name unchanged while still drafting.
    assert_eq!(app.chats[0].name, "chat-0");
}

/// Enter commits the trimmed draft; the event loop persists afterwards.
#[test]
fn commit_rename_saves_trimmed_name() {
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);
    // Replace the pre-filled name with the new draft, then save.
    for _ in 0..app.rename_buf().unwrap_or_default().chars().count() {
        app.rename_backspace();
    }
    for ch in "  Deep Dive   ".chars() {
        app.rename_push(ch);
    }

    assert!(app.commit_rename());
    assert_eq!(app.chats[0].name, "Deep Dive", "name trimmed on save");
    assert_eq!(app.chat_title(), "Deep Dive");
    assert_eq!(app.mode, Mode::Normal, "commit closes the session");
}

#[test]
fn cancel_rename_discards_draft_and_keeps_old_name() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "half typed prompt");

    press_ctrl_r(&mut app);
    for ch in "junk draft".chars() {
        app.rename_push(ch);
    }

    assert!(app.cancel_rename());
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.chats[0].name, "chat-0", "old name survives Esc");
    // The prompt buffer content survived the whole rename round-trip
    // (rename mode owns its own draft).
    assert_eq!(app.input.lines().join(""), "half typed prompt");
    // Cancelling again without a session is a harmless no-op.
    assert!(!app.cancel_rename());
}

#[test]
fn empty_or_whitespace_names_are_rejected_silently() {
    for draft in ["", "   ", "\t\n "] {
        let mut app = app_with_chats(1);
        press_ctrl_r(&mut app);
        for ch in draft.chars() {
            app.rename_push(ch);
        }
        assert!(!app.commit_rename(), "draft {draft:?} rejected");
        assert_eq!(
            app.chats[0].name, "chat-0",
            "draft {draft:?}: old name kept"
        );
        assert_eq!(app.mode, Mode::Normal, "rejection still leaves rename mode");
    }
}

// busy-chat rename ------------------------------

/// Renaming must not care about request lifecycle: a busy thread's title
/// is independent of its in-flight work.
#[test]
fn renaming_a_busy_chat_works_and_keeps_lifecycle() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "long running question");
    assert!(app.is_busy());
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));

    press_ctrl_r(&mut app);
    // Replace the pre-filled name entirely, then type the new one.
    for _ in 0..app.rename_buf().unwrap_or_default().chars().count() {
        app.rename_backspace();
    }
    for ch in "renamed while busy".chars() {
        app.rename_push(ch);
    }
    assert!(app.commit_rename());

    assert_eq!(app.chats[0].name, "renamed while busy");
    assert_eq!(
        app.chats[0].lifecycle.request_id(),
        Some(submitted.request_id.as_str()),
        "in-flight request untouched by the rename"
    );
    assert_eq!(
        app.active_lifecycle(),
        &ChatLifecycle::Awaiting {
            request_id: submitted.request_id.clone()
        }
    );
}

/// Ctrl+R with an already-open session is a no-op (never clobbers the
/// in-progress draft).
#[test]
fn ctrl_r_while_already_renaming_keeps_the_draft() {
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);
    for ch in "draft".chars() {
        app.rename_push(ch);
    }

    press_ctrl_r(&mut app);
    assert_eq!(
        app.mode,
        Mode::Renaming {
            buf: "chat-0draft".to_owned()
        },
        "re-press must not reset the draft"
    );
}

#[test]
fn mode_helpers_report_state() {
    let mut app = app_with_chats(1);
    assert!(app.mode.is_normal());
    assert!(app.rename_buf().is_none());

    press_ctrl_r(&mut app);
    assert!(!app.mode.is_normal());
    assert_eq!(app.rename_buf(), Some("chat-0"));

    app.cancel_rename();
    assert!(app.mode.is_normal());
}

#[test]
fn begin_rename_without_chats_is_a_no_op() {
    let mut app = App::new(Vec::new());
    press_ctrl_r(&mut app);
    assert_eq!(app.mode, Mode::Normal, "no chat ⇒ no rename session");
}

// editor block height helper ---------------------

// user-rename push (hidden `/rename_thread` exchange) --------------

fn rename_capable_app(n: usize) -> App {
    let mut app = app_with_chats(n);
    app.set_backend_commands(vec!["/stop".to_owned(), "/rename_thread".to_owned()]);
    app
}

/// Replace the pre-filled rename draft with `name` entirely.
fn rename_draft(app: &mut App, name: &str) {
    for _ in 0..app.rename_buf().unwrap_or_default().chars().count() {
        app.rename_backspace();
    }
    for ch in name.chars() {
        app.rename_push(ch);
    }
}

#[test]
fn commit_stages_the_rename_push_exactly_once() {
    let mut app = rename_capable_app(1);
    press_ctrl_r(&mut app);
    rename_draft(&mut app, "Deep Dive");
    assert!(app.commit_rename());

    let push = app.take_rename_submission().expect("push staged");
    assert_eq!(push.prompt, "/rename_thread Deep Dive");
    assert_eq!(push.thread_id, app.chats[0].id);
    // staging is one-shot: drained once, gone.
    assert!(app.take_rename_submission().is_none());
    // the push occupies the lifecycle like any hidden exchange.
    assert_eq!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting {
            request_id: push.request_id.clone()
        }
    );
    // the purpose registry suppresses the terminal event later.
    assert!(app.hidden_requests.contains_key(&push.request_id));
}

#[test]
fn cancel_rename_stages_nothing() {
    let mut app = rename_capable_app(1);
    press_ctrl_r(&mut app);
    rename_draft(&mut app, "never saved");
    assert!(app.cancel_rename());

    assert!(
        app.take_rename_submission().is_none(),
        "cancel pushes nothing"
    );
    assert_eq!(app.chats[0].name, "chat-0", "old name kept");
}

/// An old backend that never advertised `/rename_thread` gets a purely
/// local rename: title commits, nothing staged.
#[test]
fn no_commands_capability_means_no_staging() {
    let mut app = app_with_chats(1);
    assert!(!app.supports_rename_command(), "nothing advertised yet");

    press_ctrl_r(&mut app);
    rename_draft(&mut app, "Offline Title");
    assert!(app.commit_rename());

    assert_eq!(app.chats[0].name, "Offline Title", "local title commits");
    assert!(
        app.take_rename_submission().is_none(),
        "no push without the capability"
    );
}

#[test]
fn unrelated_commands_do_not_enable_the_push() {
    let mut app = app_with_chats(1);
    app.set_backend_commands(vec!["/stop".to_owned(), "/model".to_owned()]);
    assert!(!app.supports_rename_command());

    press_ctrl_r(&mut app);
    rename_draft(&mut app, "Still Local");
    assert!(app.commit_rename());
    assert!(app.take_rename_submission().is_none());
}

/// Documented choice: the PUSHED payload is truncated to 128 chars while
/// the local title stays exactly as the user typed it.
#[test]
fn long_name_pushes_truncated_payload_keeps_local_title() {
    let mut app = rename_capable_app(1);
    let long = "x".repeat(200);
    press_ctrl_r(&mut app);
    rename_draft(&mut app, &long);
    assert!(app.commit_rename());

    assert_eq!(app.chats[0].name, long, "local title stays full");
    let push = app.take_rename_submission().expect("push staged");
    assert_eq!(push.prompt, format!("/rename_thread {}", "x".repeat(128)));
}

#[test]
fn name_at_the_limit_pushes_untruncated() {
    let mut app = rename_capable_app(1);
    let exact = "y".repeat(128);
    press_ctrl_r(&mut app);
    rename_draft(&mut app, &exact);
    assert!(app.commit_rename());

    let push = app.take_rename_submission().expect("push staged");
    assert_eq!(push.prompt, format!("/rename_thread {exact}"));
}

/// Guard: a raw newline cannot ride the one-line title exchange; the local
/// title keeps whatever the user typed.
#[test]
fn newline_name_pushes_nothing_but_keeps_title() {
    let mut app = rename_capable_app(1);
    press_ctrl_r(&mut app);
    rename_draft(&mut app, "two\nlines");
    assert!(app.commit_rename());

    assert_eq!(app.chats[0].name, "two\nlines");
    assert!(app.take_rename_submission().is_none());
}

/// A busy chat parks the push in the hidden FIFO; the idle drain dispatches
/// it with the `RenameThread` purpose inferred from the `/rename_thread`
/// prefix (same registry, same suppression).
#[test]
fn busy_chat_parks_the_push_and_the_drain_infers_the_purpose() {
    let mut app = rename_capable_app(1);
    submit_text(&mut app, "long running question");
    assert!(app.is_busy());

    press_ctrl_r(&mut app);
    rename_draft(&mut app, "Parked Title");
    assert!(app.commit_rename());
    assert!(app.take_rename_submission().is_none(), "busy chat parks it");

    // The visible turn ends first; the idle drain then dispatches the
    // parked push (the event loop's QueueDrain glue does this in prod).
    finish_chat(&mut app, 0);
    let thread_id = app.chats[0].id.clone();
    let deferred = app
        .take_deferred_hidden_request(&thread_id)
        .expect("parked push dispatched on the idle drain");
    assert_eq!(deferred.prompt, "/rename_thread Parked Title");
    assert_eq!(deferred.thread_id, thread_id);
    assert!(
        app.hidden_requests.contains_key(&deferred.request_id),
        "deferred purpose registered"
    );

    // The terminal result is suppressed: no bubble, lifecycle resolves.
    let bubbles_before = app.chats[0].messages.len();
    app.apply_backend_event(BackendEvent::Result {
        request_id: event_id_of(&deferred.request_id),
        markdown: "/rename_thread Parked Title".to_owned(),
        thread_id: app.chats[0].id.clone(),
        model: None,
        usage: None,
        thoughts: None,
    });
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
    assert_eq!(
        app.chats[0].messages.len(),
        bubbles_before,
        "push result adds no bubble"
    );
}

/// Fire-and-forget guard: a failed push (error frame) resolves silently —
/// no error popup, lifecycle back to Idle, local title untouched.
#[test]
fn failed_push_never_reverts_local_title() {
    let mut app = rename_capable_app(1);
    press_ctrl_r(&mut app);
    rename_draft(&mut app, "Kept Title");
    assert!(app.commit_rename());
    let push = app.take_rename_submission().expect("push staged");

    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&push.request_id),
        message: "command refused".to_owned(),
        thread_id: Some(app.chats[0].id.clone()),
    });

    assert_eq!(app.chats[0].name, "Kept Title", "title never reverts");
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
    assert!(
        app.error_popup.is_none(),
        "fire-and-forget: no error popup for the push"
    );
}
