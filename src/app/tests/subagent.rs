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

/// A confirmed `/stop` clears the stopped thread's counters, even though
/// the counter was still live when the stop was staged — the backend's
/// kill-flush frame may never arrive, so the TUI must not wait for it.
#[test]
fn stop_confirm_clears_the_subagent_counter() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "spawn helpers");
    agent_event(&mut app, 0, AgentEventKind::Started, 2, 4);
    assert_eq!(app.active_chat_subagents(), Some(2), "precondition");

    // Stage and confirm a /stop control request exactly as ^L does.
    app.set_backend_commands(vec![App::STOP_COMMAND.to_owned()]);
    app.begin_stop_confirm();
    app.confirm_stop_reset();
    let (ctrl_request_id, ctrl_thread_id) = {
        let pending = app.pending_control.as_ref().expect("stop staged");
        (pending.request_id.clone(), pending.thread_id.clone())
    };

    // The control's own terminal result frame resolves the stop.
    app.apply_backend_event(BackendEvent::Result {
        request_id: event_id_of(&ctrl_request_id),
        markdown: "Everything stopped.".into(),
        thread_id: ctrl_thread_id,
        model: None,
        usage: None,
        thoughts: None,
    });
    assert!(app.pending_control.is_none(), "stop resolved");
    assert!(
        app.chats[0].subagent_counts.is_empty(),
        "the stopped thread's counters must be cleared"
    );
    assert_eq!(
        app.active_chat_subagents(),
        None,
        "no counter after the stop"
    );
}

/// A connection-level Disconnected wipes every chat's counters: all
/// threads' subagents lived in the backend process that just went away.
#[test]
fn disconnect_clears_all_chats_subagent_counters() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "work");
    app.active = 1;
    submit_text(&mut app, "more work");
    let a = idx(&app, "chat-0");
    let b = idx(&app, "chat-1");
    agent_event(&mut app, a, AgentEventKind::Started, 2, 4);
    agent_event(&mut app, b, AgentEventKind::Started, 1, 4);

    app.apply_backend_event(BackendEvent::Disconnected);

    app.active = a;
    assert_eq!(
        app.active_chat_subagents(),
        None,
        "chat-0's counters died with the transport"
    );
    app.active = b;
    assert_eq!(
        app.active_chat_subagents(),
        None,
        "chat-1's counters died with the transport"
    );
    assert!(app.chats[a].subagent_counts.is_empty());
    assert!(app.chats[b].subagent_counts.is_empty());
}

/// A transport-level error frame clears the affected chat's counters (the
/// thread's agent frames ride the link that just failed) while OTHER
/// chats' counters — fed by the still-alive backend from earlier turns —
/// stay untouched. A non-transport error keeps the counters: the request's
/// agent pump survives an application error and the backend still owns
/// the subagents' lifecycle.
#[test]
fn transport_error_clears_only_the_affected_chats_counters() {
    let mut app = app_with_chats(2);
    let first = submit_text(&mut app, "work");
    app.active = 1;
    let second = submit_text(&mut app, "background work");
    let a = idx(&app, "chat-0");
    let b = idx(&app, "chat-1");
    agent_event(&mut app, a, AgentEventKind::Started, 2, 4);
    agent_event(&mut app, b, AgentEventKind::Started, 1, 4);

    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&first.request_id),
        message: "backend connection lost".into(),
        thread_id: Some(app.chats[a].id.clone()),
    });

    // The inline failure lifts the failed thread to the sidebar top:
    // re-resolve both positions by name before asserting.
    let a = idx(&app, "chat-0");
    let b = idx(&app, "chat-1");
    assert!(
        app.chats[a].subagent_counts.is_empty(),
        "the failed thread's counters must be cleared"
    );
    assert_eq!(
        app.chats[b]
            .subagent_counts
            .get(&event_id_of(&second.request_id)),
        Some(&(1, 4)),
        "another thread's live counter stays untouched"
    );
    assert_eq!(app.chats[b].running_subagents(), Some(1));
}

/// A late `finished(active == 0)` kill-flush arriving after a defensive
/// clear is an idempotent no-op: no panic, no resurrection.
#[test]
fn late_kill_flush_after_a_clear_is_an_idempotent_noop() {
    let mut app = app_with_chats(1);
    let first = submit_text(&mut app, "spawn helpers");
    agent_event(&mut app, 0, AgentEventKind::Started, 2, 4);
    app.chats[0].clear_subagent_counts();
    assert_eq!(app.active_chat_subagents(), None, "cleared");

    agent_frame(
        &mut app,
        0,
        event_id_of(&first.request_id),
        AgentEventKind::Finished,
        0,
        4,
    );
    assert!(
        app.chats[0].subagent_counts.is_empty(),
        "the flush must not resurrect anything"
    );
    assert_eq!(app.active_chat_subagents(), None);
}

// creation / switching --------------------------------
