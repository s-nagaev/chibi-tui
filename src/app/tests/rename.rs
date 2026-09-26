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
