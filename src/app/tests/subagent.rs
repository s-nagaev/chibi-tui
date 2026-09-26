use super::support::*;
use crate::app::*;

/// Deliver a mid-turn `agent_event` to `index`'s tracked request id
/// (same correlation path the live glue task uses).
fn agent_event(app: &mut App, index: usize, event: AgentEventKind, active: u64, total: u64) {
    let request_id = app.chats[index].lifecycle.request_id().unwrap().to_owned();
    agent_frame(app, index, event_id_of(&request_id), event, active, total);
}

/// Deliver a mid-turn `agent_event` carrying an arbitrary numeric
/// request id — the shape of late frames (kill-flush, kept-request
/// overlap) whose id differs from the chat's currently tracked request.
fn agent_frame(
    app: &mut App,
    index: usize,
    request_id: u64,
    event: AgentEventKind,
    active: u64,
    total: u64,
) {
    let thread_id = app.chats[index].id.clone();
    app.apply_backend_event(BackendEvent::AgentProgress {
        request_id,
        thread_id,
        event,
        active,
        total,
    });
}

#[test]
fn subagent_events_aggregate_started_update_finished() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "spawn helpers");

    agent_event(&mut app, 0, AgentEventKind::Started, 2, 5);
    assert_eq!(app.active_chat_subagents(), Some(2));
    assert!(
        matches!(app.chats[0].lifecycle, ChatLifecycle::Awaiting { .. }),
        "mid-turn progress must never touch the lifecycle"
    );

    // A second spawn within the same request updates the entry in place.
    agent_event(&mut app, 0, AgentEventKind::Started, 3, 5);
    assert_eq!(app.active_chat_subagents(), Some(3));

    // Some subagents finish but the request keeps live ones.
    agent_event(&mut app, 0, AgentEventKind::Finished, 1, 5);
    assert_eq!(app.active_chat_subagents(), Some(1));

    // The final finish reports active == 0: the entry is removed.
    agent_event(&mut app, 0, AgentEventKind::Finished, 0, 5);
    assert!(
        app.chats[0].subagent_counts.is_empty(),
        "entry removed at active == 0"
    );
    assert_eq!(app.active_chat_subagents(), None);
}

#[test]
fn subagent_counter_shows_only_the_active_chats_count() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "active request");
    app.active = 1;
    submit_text(&mut app, "background request");

    // Both chats run concurrently with their own counters (sidebar
    // positions may have shifted — look them up by name).
    let a = idx(&app, "chat-0");
    let b = idx(&app, "chat-1");
    agent_event(&mut app, a, AgentEventKind::Started, 2, 4);
    agent_event(&mut app, b, AgentEventKind::Started, 1, 4);

    // The active chat (chat-1) sees only its own counter…
    app.select_chat(b);
    assert_eq!(app.active_chat_subagents(), Some(1));
    // …and switching back to chat-0 sees only chat-0's.
    app.select_chat(a);
    assert_eq!(app.active_chat_subagents(), Some(2));
}

#[test]
fn subagent_counter_outlives_the_result_frame_and_folds_late_frames() {
    let mut app = app_with_chats(1);
    let first = submit_text(&mut app, "first");
    agent_event(&mut app, 0, AgentEventKind::Started, 2, 4);
    assert_eq!(app.active_chat_subagents(), Some(2));

    // Terminal event → idle: the answer is rendered, but two background
    // subagents of the turn still run. The counter must stay visible
    // independently of the request lifecycle (the reported bug).
    finish_chat(&mut app, 0);
    assert!(matches!(app.chats[0].lifecycle, ChatLifecycle::Idle));
    assert_eq!(
        app.active_chat_subagents(),
        Some(2),
        "counter must outlive the result frame while subagents run"
    );

    // A LATE frame for the finished request folds under its own request
    // id — the count follows live and never touches another entry.
    agent_frame(
        &mut app,
        0,
        event_id_of(&first.request_id),
        AgentEventKind::Finished,
        1,
        4,
    );
    assert_eq!(app.active_chat_subagents(), Some(1));

    // The final finish reports active == 0: the entry is removed and
    // the idle chat's counter disappears.
    agent_frame(
        &mut app,
        0,
        event_id_of(&first.request_id),
        AgentEventKind::Finished,
        0,
        4,
    );
    assert_eq!(
        app.active_chat_subagents(),
        None,
        "the count dropping to 0 hides the counter"
    );
    assert!(app.chats[0].subagent_counts.is_empty());
}

#[test]
fn subagent_counter_sums_live_subagents_across_thread_requests() {
    let mut app = app_with_chats(1);
    let first = submit_text(&mut app, "first");
    agent_event(&mut app, 0, AgentEventKind::Started, 2, 4);
    finish_chat(&mut app, 0);

    // A new request starts while the previous turn's subagents still
    // run: frames fold per request id, the display sums the thread.
    let second = submit_text(&mut app, "second");
    agent_event(&mut app, 0, AgentEventKind::Started, 1, 1);
    assert_eq!(
        app.active_chat_subagents(),
        Some(3),
        "the old request's live subagents plus the new request's"
    );

    // The old request's late finish removes only its own entry.
    agent_frame(
        &mut app,
        0,
        event_id_of(&first.request_id),
        AgentEventKind::Finished,
        0,
        4,
    );
    assert_eq!(app.active_chat_subagents(), Some(1));

    agent_frame(
        &mut app,
        0,
        event_id_of(&second.request_id),
        AgentEventKind::Finished,
        0,
        1,
    );
    assert_eq!(app.active_chat_subagents(), None);
}

#[test]
fn subagent_counter_follows_the_thread_switched_to() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "work");
    agent_event(&mut app, 0, AgentEventKind::Started, 2, 4);
    finish_chat(&mut app, 0);
    assert_eq!(app.active_chat_subagents(), Some(2));

    // Switching to a thread without subagents hides the indicator —
    // the idle thread must not show another thread's count.
    app.active = 1;
    assert_eq!(
        app.active_chat_subagents(),
        None,
        "a thread with no subagents renders no counter"
    );

    app.active = 0;
    assert_eq!(
        app.active_chat_subagents(),
        Some(2),
        "switching back restores the owning thread's count"
    );
}

#[test]
fn subagent_counter_with_zero_active_renders_nothing() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "work");
    agent_event(&mut app, 0, AgentEventKind::Started, 0, 3);
    assert_eq!(
        app.active_chat_subagents(),
        None,
        "active == 0 → no counter"
    );
}

// creation / switching --------------------------------
