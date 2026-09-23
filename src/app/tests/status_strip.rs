use super::support::*;
use crate::app::*;

/// The thoughts block starts visible (ON by contract) and toggling
/// round-trips without ever touching the retained reasoning.
#[test]
fn thoughts_toggle_defaults_on_and_round_trips() {
    let mut app = app_with_chats(1);
    assert!(app.thoughts_visible, "thoughts must start visible (ON)");
    app.chats[0].last_thoughts = Some("step by step".into());
    app.toggle_thoughts();
    assert!(!app.thoughts_visible);
    assert_eq!(
        app.chats[0].last_thoughts.as_deref(),
        Some("step by step"),
        "toggle is render-only: thoughts must survive it"
    );
    app.toggle_thoughts();
    assert!(app.thoughts_visible);
}

// strip state + segments --------------------------

/// Default visible; ^O flips visibility round-trip.
#[test]
fn status_strip_starts_visible_and_toggles_round_trip() {
    let mut app = app_with_chats(1);
    assert!(
        app.status_strip_visible,
        "strip must start visible (task contract)"
    );
    app.toggle_status_strip();
    assert!(!app.status_strip_visible);
    app.toggle_status_strip();
    assert!(app.status_strip_visible);
}

/// View state like Focus: opening and closing every modal family must
/// leave the visibility flag untouched.
#[test]
fn status_strip_visibility_survives_modal_open_close() {
    let mut app = app_with_chats(1);

    app.begin_search();
    assert!(matches!(app.mode, Mode::Searching { .. }));
    assert!(
        app.status_strip_visible,
        "opening the search popup must not hide the strip"
    );
    assert!(app.cancel_search());
    assert!(app.mode.is_normal());
    assert!(
        app.status_strip_visible,
        "closing a modal must not reset it"
    );

    app.begin_delete_confirm();
    assert!(matches!(app.mode, Mode::ConfirmDelete));
    assert!(app.status_strip_visible);
    assert!(app.cancel_delete());
    assert!(app.status_strip_visible);

    app.begin_log_viewer();
    assert!(matches!(app.mode, Mode::LogViewer { .. }));
    assert!(app.status_strip_visible);
    assert!(app.close_log_viewer());
    assert!(app.status_strip_visible, "log viewer close must keep it");
}

/// The cwd segment is the workspace root's path TAIL: the last three
/// components with a leading `/`; shorter paths show what they have
/// and pathless roots degrade to the raw string.
#[test]
fn status_cwd_is_the_workspace_path_tail() {
    let mut app = app_with_chats(1);
    assert_eq!(app.status_cwd(), None, "unwired root yields None");

    app.workspace_root = Some("/Users/sergio/Develop/personal/chibi-tui".into());
    assert_eq!(
        app.status_cwd().as_deref(),
        Some("/Develop/personal/chibi-tui")
    );

    // Exactly three components: the whole path behind the prefix.
    app.workspace_root = Some("/home/sergio/chibi-tui".into());
    assert_eq!(app.status_cwd().as_deref(), Some("/home/sergio/chibi-tui"));

    app.workspace_root = Some("/Users/sergio/".into());
    assert_eq!(
        app.status_cwd().as_deref(),
        Some("/Users/sergio"),
        "trailing slash tolerated"
    );

    app.workspace_root = Some("/chibi-tui".into());
    assert_eq!(app.status_cwd().as_deref(), Some("/chibi-tui"));

    app.workspace_root = Some("/".into());
    assert_eq!(
        app.status_cwd().as_deref(),
        Some("/"),
        "pathless root degrades to raw"
    );

    app.workspace_root = Some(".".into());
    assert_eq!(app.status_cwd().as_deref(), Some("."));
}

/// The cwd segment prefers the LIVE effective cwd the backend reported
/// for the ACTIVE thread via `cwd_update` frames, falls back to the CLI
/// workspace root when the thread has no report yet, and follows the
/// thread when the active chat switches.
#[test]
fn status_cwd_prefers_live_thread_cwd() {
    let mut app = app_with_chats(2);
    app.workspace_root = Some("/Users/sergio/Develop/personal/chibi-tui".into());
    let active_wire = crate::live::wire_thread_id(app.active_thread_id().unwrap());

    // A report for an unrelated thread must not move the readout.
    app.apply_backend_event(BackendEvent::CwdUpdate {
        wire_thread_id: active_wire.wrapping_add(99),
        cwd: "/elsewhere".into(),
    });
    assert_eq!(
        app.status_cwd().as_deref(),
        Some("/Develop/personal/chibi-tui"),
        "no report for the active thread yet → workspace fallback"
    );

    // The active thread's live report wins over the workspace root.
    app.apply_backend_event(BackendEvent::CwdUpdate {
        wire_thread_id: active_wire,
        cwd: "/Users/sergio/Develop/personal/chibi".into(),
    });
    assert_eq!(
        app.status_cwd().as_deref(),
        Some("/Develop/personal/chibi"),
        "live thread cwd takes precedence"
    );

    // Switching to a chat with no report falls back, and its own report
    // takes over once it arrives.
    app.select_chat(1);
    assert_eq!(
        app.status_cwd().as_deref(),
        Some("/Develop/personal/chibi-tui"),
        "the entered thread has no report → workspace fallback"
    );
    let other_wire = crate::live::wire_thread_id(app.active_thread_id().unwrap());
    app.apply_backend_event(BackendEvent::CwdUpdate {
        wire_thread_id: other_wire,
        cwd: "/Users/sergio/other".into(),
    });
    assert_eq!(app.status_cwd().as_deref(), Some("/Users/sergio/other"));
}

#[test]
fn status_toast_expires_after_ticks() {
    let mut app = app_with_chats(1);
    app.show_status("can't delete — busy");
    assert!(app.status_message.is_some());
    for _ in 0..STATUS_MSG_TICKS {
        app.tick_status_message();
    }
    assert!(app.status_message.is_none(), "toast auto-cleared");
    // Further ticks are harmless no-ops.
    app.tick_status_message();
    assert!(app.status_message.is_none());
}

// popup state logic ----------------------------
