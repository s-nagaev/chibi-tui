use super::support::*;
use crate::app::*;

/// Wire thread id of the chat at `index` (same mapping the live glue uses).
fn wire_of(app: &App, index: usize) -> i64 {
    crate::live::wire_thread_id(&app.chats[index].id)
}

/// A `thread_renamed` backend event as `live.rs` fans it out.
fn renamed_frame(wire_thread_id: i64, name: &str) -> BackendEvent {
    BackendEvent::ThreadRenamed {
        wire_thread_id,
        name: name.to_owned(),
    }
}

/// The frame is a name-only mutation: the matching chat's `name` changes,
/// every other piece of app and chat state stays byte-identical.
#[test]
fn renamed_frame_updates_matching_chat_title_only() {
    let mut app = app_with_chats(2);
    app.select_chat(1);
    app.scroll = 7;
    let (updated0, updated1) = (app.chats[0].updated_at, app.chats[1].updated_at);

    app.apply_backend_event(renamed_frame(wire_of(&app, 1), "Cycling plans"));

    assert_eq!(app.chats[1].name, "Cycling plans");
    assert_eq!(app.chats[0].name, "chat-0", "unrelated chat untouched");
    assert_eq!(app.active, 1, "selection untouched");
    assert_eq!(app.focus, Focus::Chat, "focus untouched");
    assert_eq!(app.mode, Mode::Normal, "mode untouched");
    assert_eq!(app.scroll, 7, "scroll untouched");
    assert_eq!(app.chats[0].updated_at, updated0, "no sidebar re-order");
    assert_eq!(app.chats[1].updated_at, updated1, "no sidebar re-order");
    assert!(
        !app.chats[0].unread && !app.chats[1].unread,
        "no unread raise"
    );
    assert!(app.thread_cwds.is_empty(), "no second title/cwd store");
}

/// Empty or whitespace-only titles are rejected: the current name survives.
#[test]
fn blank_renamed_frame_is_a_no_op() {
    let mut app = app_with_chats(1);

    app.apply_backend_event(renamed_frame(wire_of(&app, 0), ""));
    assert_eq!(app.chats[0].name, "chat-0", "empty title rejected");

    app.apply_backend_event(renamed_frame(wire_of(&app, 0), "   "));
    assert_eq!(
        app.chats[0].name, "chat-0",
        "whitespace-only title rejected"
    );
}

/// A frame carrying the chat's current title is an idempotent no-op: this
/// makes the backend echo of the user's own rename safe.
#[test]
fn duplicate_renamed_frame_is_an_idempotent_no_op() {
    let mut app = app_with_chats(1);
    app.chats[0].last_usage = Some(sample_usage());
    app.chats[0].last_model = Some("some-model".into());
    let updated = app.chats[0].updated_at;

    app.apply_backend_event(renamed_frame(wire_of(&app, 0), "chat-0"));

    assert_eq!(app.chats[0].name, "chat-0");
    assert_eq!(app.chats[0].updated_at, updated);
    assert!(app.chats[0].last_usage.is_some(), "sticky ctx untouched");
    assert_eq!(app.chats[0].last_model.as_deref(), Some("some-model"));
}

/// A frame for a wire id that maps to no chat is dropped silently: unknown
/// threads get no entry anywhere (no ghost titles, no panic).
#[test]
fn renamed_frame_for_unknown_wire_id_is_dropped_silently() {
    let mut app = app_with_chats(2);
    let ghost = wire_of(&app, 0).wrapping_add(123_456);

    app.apply_backend_event(renamed_frame(ghost, "ghost title"));

    assert_eq!(app.chats[0].name, "chat-0");
    assert_eq!(app.chats[1].name, "chat-1");
    assert!(app.thread_cwds.is_empty(), "unknown id creates nothing");
}

/// Last-writer-wins, order 1 (OQ1): the frame lands while the user's rename
/// draft is open. The frame applies to the stored `name`, the open draft
/// survives untouched, and the user's later commit is the last writer.
#[test]
fn renamed_frame_under_open_draft_applies_then_user_commit_wins() {
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);
    for ch in "my title".chars() {
        app.rename_push(ch);
    }

    // The agent renames its own thread while the user keeps typing.
    app.apply_backend_event(renamed_frame(wire_of(&app, 0), "agent title"));

    assert_eq!(
        app.chats[0].name, "agent title",
        "the frame applies to the stored name"
    );
    assert_eq!(
        app.mode,
        Mode::Renaming {
            buf: "chat-0my title".to_owned()
        },
        "the open draft survives the frame untouched"
    );

    // The user's commit is the last writer and wins.
    assert!(app.commit_rename());
    assert_eq!(app.chats[0].name, "chat-0my title");
    assert_eq!(app.mode, Mode::Normal);
}

/// Last-writer-wins, order 2 (OQ1): the user commits first, then the backend
/// echo of the user's own push arrives with the same title — the idempotent
/// no-op keeps the user's committed name exactly.
#[test]
fn echo_after_local_commit_is_a_no_op() {
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);
    for ch in "user title".chars() {
        app.rename_push(ch);
    }
    assert!(app.commit_rename());
    assert_eq!(app.chats[0].name, "chat-0user title");

    // The backend echoes the pushed title back.
    app.apply_backend_event(renamed_frame(wire_of(&app, 0), "chat-0user title"));

    assert_eq!(
        app.chats[0].name, "chat-0user title",
        "echo changed nothing"
    );
}

/// A background (non-active) chat is renamed while a different chat is
/// active: the title changes, the running turn, sticky state, selection and
/// unread markers are all untouched.
#[test]
fn background_chat_rename_while_other_chat_is_active() {
    let mut app = app_with_chats(2);
    let _busy = submit_text(&mut app, "busy turn"); // chat 0 Awaiting
    app.chats[0].last_usage = Some(sample_usage());
    app.select_chat(1);
    app.scroll = 3;
    let updated = app.chats[0].updated_at;
    let sticky = app.last_turn_usage;

    app.apply_backend_event(renamed_frame(wire_of(&app, 0), "bg new name"));

    assert_eq!(app.chats[0].name, "bg new name");
    assert_eq!(app.active, 1, "selection untouched");
    assert_eq!(app.scroll, 3, "scroll untouched");
    assert_eq!(
        app.chats[0].updated_at, updated,
        "no sidebar lift for the renamed thread"
    );
    assert!(!app.chats[0].unread, "a rename is not fresh activity");
    assert_eq!(app.last_turn_usage, sticky, "sticky ctx untouched");
    assert!(
        matches!(app.chats[0].lifecycle, ChatLifecycle::Awaiting { .. }),
        "running turn untouched"
    );
    assert_eq!(
        app.chats[0].messages.len(),
        2,
        "transcript untouched (user prompt + pending placeholder)"
    );
}
