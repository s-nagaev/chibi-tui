use super::support::*;
use crate::*;

// thread_event_wire_id routing helper ----------------------------

/// The helper must recognize BOTH session-scoped wire-id events:
/// `BackgroundMessage` continuations (pre-existing) and `ThreadRenamed`
/// title changes (the rename fix), each returning its own wire id.
#[test]
fn thread_event_wire_id_covers_background_and_rename_events() {
    let bg = BackendEvent::BackgroundMessage {
        wire_thread_id: 42,
        markdown: "answer".into(),
        model: None,
        thoughts: None,
    };
    assert_eq!(thread_event_wire_id(&bg), Some(42));

    let renamed = BackendEvent::ThreadRenamed {
        wire_thread_id: 7,
        name: "New title".into(),
    };
    assert_eq!(thread_event_wire_id(&renamed), Some(7));
}

// chat_for_wire_id persist-target resolution ----------------------

/// The routed persist target: a rename landing for a BACKGROUND (non-
/// active) chat must resolve to THAT chat, never to `app.chats[active]`.
/// This is the regression the fix restores — reverting the loop arm to
/// the active-chat default makes the resolved id flip and the on-disk
/// assertions below fail.
#[test]
fn rename_persist_target_is_the_renamed_chat_not_the_active_one() {
    let dir = temp_history_dir("rename-routing");
    let mut app = app_with_chats(2); // chat 0 is active

    let active_uuid = app.chats[0].id.clone();
    let background_uuid = app.chats[1].id.clone();
    let background_wire = chibi_tui::live::wire_thread_id(&background_uuid);

    // Baseline: both snapshots on disk before the rename frame arrives.
    persist_chat(&app.chats[0], Some(&dir));
    persist_chat(&app.chats[1], Some(&dir));
    let active_path = dir.join(format!("threads/{active_uuid}.json"));
    let background_path = dir.join(format!("threads/{background_uuid}.json"));
    assert!(active_path.exists(), "active baseline snapshot exists");
    assert!(
        background_path.exists(),
        "background baseline snapshot exists"
    );
    let active_before = std::fs::read(&active_path).expect("active baseline readable");
    let background_before = std::fs::read(&background_path).expect("background baseline readable");

    // A rename frame for the background thread, applied through the real
    // app path exactly as the event loop does before routing persistence.
    let evt = BackendEvent::ThreadRenamed {
        wire_thread_id: background_wire,
        name: "Renamed in background".into(),
    };
    let wire_id = thread_event_wire_id(&evt).expect("a rename frame carries its wire id");
    app.apply_backend_event(evt);

    // The routed arm resolves the OWNING chat, which is not the active one.
    let resolved = chat_for_wire_id(&app.chats, wire_id).expect("renamed chat must resolve");
    assert_eq!(resolved.id, background_uuid, "resolves the renamed chat");
    assert_eq!(resolved.name, "Renamed in background");
    assert_ne!(
        app.chats.get(app.active).map(|c| c.id.as_str()),
        Some(resolved.id.as_str()),
        "the resolved chat must not be the active one (the bug this fix repairs)"
    );

    // Routing mirrors the loop: persist the resolved chat only.
    persist_chat(resolved, Some(&dir));

    // On disk the renamed background snapshot carries the new title while
    // the active chat's snapshot is byte-for-byte untouched.
    let background_after = std::fs::read(&background_path).expect("background snapshot readable");
    assert_ne!(
        background_before, background_after,
        "the renamed chat's snapshot is rewritten with the new title"
    );
    let stored = std::fs::read_to_string(&background_path).expect("snapshot is text");
    assert!(
        stored.contains("Renamed in background"),
        "the persisted snapshot carries the new title"
    );
    let active_after = std::fs::read(&active_path).expect("active snapshot readable");
    assert_eq!(
        active_before, active_after,
        "the active chat's snapshot stays untouched (a regression to the \
         active-chat default would rewrite it instead)"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// An unknown wire id resolves to no chat at all; the loop then persists
/// nothing, same shape as the pre-existing `BackgroundMessage` routing.
#[test]
fn chat_for_wire_id_returns_none_for_unknown_thread() {
    let app = app_with_chats(1);
    let known = chibi_tui::live::wire_thread_id(&app.chats[0].id);
    assert!(chat_for_wire_id(&app.chats, known).is_some());
    assert!(chat_for_wire_id(&app.chats, known + 1).is_none());
}
