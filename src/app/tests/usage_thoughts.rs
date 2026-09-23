use super::support::*;
use crate::app::*;

#[test]
fn result_retains_latest_turn_usage_and_thoughts() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    assert_eq!(app.last_turn_usage, None, "no turn yet");
    app.apply_backend_event(BackendEvent::Result {
        usage: Some(sample_usage()),
        thoughts: Some("thinking...".into()),
        request_id: event_id_of(&submitted.request_id),
        markdown: "answer".into(),
        thread_id: submitted.thread_id,
        model: None,
    });
    assert_eq!(app.last_turn_usage, Some(sample_usage()));
    assert_eq!(app.chats[0].last_thoughts.as_deref(), Some("thinking..."));
}

/// A CHAIN of thoughts — the turn's terminal result plus every
/// background continuation's reasoning delta — must ACCUMULATE in the
/// chat's retained trace (append, never overwrite), so every chain
/// member stays visible above the latest answer. Regression for the
/// owner report: with plain overwrite only one payload of the chain
/// was ever retained.
#[test]
fn thought_chain_accumulates_across_result_and_continuations() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "do the thing");
    let thread_id = submitted.thread_id.clone();
    app.apply_backend_event(BackendEvent::Result {
        request_id: event_id_of(&submitted.request_id),
        markdown: "step one done".into(),
        thread_id: thread_id.clone(),
        model: None,
        usage: None,
        thoughts: Some("first thought".into()),
    });
    assert_eq!(
        app.chats[0].last_thoughts.as_deref(),
        Some("first thought"),
        "the result frame seeds the chain"
    );

    // Two continuation answers, each carrying its reasoning delta: the
    // chain must GROW, not replace.
    for (markdown, thought) in [
        ("step two done", "second thought"),
        ("step three done", "third thought"),
    ] {
        app.apply_backend_event(BackendEvent::BackgroundMessage {
            wire_thread_id: crate::live::wire_thread_id(&thread_id),
            markdown: markdown.into(),
            model: None,
            thoughts: Some(thought.into()),
        });
    }
    assert_eq!(
        app.chats[0].last_thoughts.as_deref(),
        Some("first thought\nsecond thought\nthird thought"),
        "every chain member must be retained in arrival order"
    );

    // The next visible request in THIS chat still resets the chain.
    let _next = submit_text(&mut app, "again");
    assert_eq!(
        app.chats[0].last_thoughts, None,
        "a new visible request clears the accumulated chain"
    );
}

/// A whitespace-only thoughts payload must neither extend the retained
/// chain nor wipe it: the block the user is reading survives blanks.
#[test]
fn blank_thoughts_payload_never_wipes_the_chain() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    let thread_id = submitted.thread_id.clone();
    app.apply_backend_event(BackendEvent::Result {
        request_id: event_id_of(&submitted.request_id),
        markdown: "answer".into(),
        thread_id: thread_id.clone(),
        model: None,
        usage: None,
        thoughts: Some("real reasoning".into()),
    });
    app.apply_backend_event(BackendEvent::BackgroundMessage {
        wire_thread_id: crate::live::wire_thread_id(&thread_id),
        markdown: "continuation".into(),
        model: None,
        thoughts: Some("   \n  ".into()),
    });
    assert_eq!(
        app.chats[0].last_thoughts.as_deref(),
        Some("real reasoning"),
        "a blank payload must not destroy the retained chain"
    );
}

#[test]
fn new_request_start_keeps_usage_and_clears_thoughts() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    app.apply_backend_event(BackendEvent::Result {
        usage: Some(sample_usage()),
        thoughts: Some("thinking...".into()),
        request_id: event_id_of(&submitted.request_id),
        markdown: "answer".into(),
        thread_id: submitted.thread_id,
        model: None,
    });
    assert!(app.last_turn_usage.is_some());
    submit_text(&mut app, "next prompt");
    assert_eq!(
        app.last_turn_usage,
        Some(sample_usage()),
        "usage survives a new request start"
    );
    assert_eq!(
        app.chats[0].last_thoughts, None,
        "cleared on new request start"
    );
}

#[test]
fn dequeued_prompt_keeps_usage_and_clears_thoughts() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "hello");
    let thread_id = submitted.thread_id.clone();
    // Queue a follow-up while the chat is busy (this does NOT start it).
    type_in(&mut app, "queued prompt");
    assert!(app.take_input().is_none(), "busy chat enqueues, no start");
    app.apply_backend_event(BackendEvent::Result {
        usage: Some(sample_usage()),
        thoughts: Some("thinking...".into()),
        request_id: event_id_of(&submitted.request_id),
        markdown: "answer".into(),
        thread_id,
        model: None,
    });
    assert!(
        app.last_turn_usage.is_some(),
        "terminal result retains the turn"
    );
    let thread_id = app.chats[0].id.clone();
    app.dequeue_next_for(&thread_id)
        .expect("queued prompt drained");
    assert_eq!(
        app.last_turn_usage,
        Some(sample_usage()),
        "usage survives a dequeued request start"
    );
    assert_eq!(
        app.chats[0].last_thoughts, None,
        "cleared on dequeued start"
    );
}

// sticky per-thread reasoning -------------------

/// Deliver a terminal Result carrying reasoning to a chat's tracked
/// request (the full thoughts payload a real LLM turn brings).
fn finish_turn_with_thoughts(app: &mut App, index: usize, thoughts: &str) {
    app.select_chat(index);
    let submitted = submit_text(app, "turn prompt");
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: Some(thoughts.to_owned()),
        request_id: event_id_of(&submitted.request_id),
        markdown: "answer".into(),
        thread_id: submitted.thread_id,
        model: None,
    });
}

/// A result frame routed to a BACKGROUND chat lands in that chat's own
/// reasoning mirror only — the chat the user is reading keeps its own
/// (stale) state, and no global field exists for a background reply to
/// pollute.
#[test]
fn background_result_routes_thoughts_to_the_owning_chat() {
    let mut app = app_with_chats(2);
    finish_turn_with_thoughts(&mut app, 0, "alpha reasoning");

    app.select_chat(idx(&app, "chat-1"));
    let background = submit_text(&mut app, "background question");
    app.select_chat(idx(&app, "chat-0"));
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: Some("beta reasoning".into()),
        request_id: event_id_of(&background.request_id),
        markdown: "beta answer".into(),
        thread_id: background.thread_id,
        model: None,
    });

    assert_eq!(
        app.chats[idx(&app, "chat-1")].last_thoughts.as_deref(),
        Some("beta reasoning"),
        "the owning chat mirrors its own reasoning"
    );
    assert_eq!(
        app.chats[idx(&app, "chat-0")].last_thoughts.as_deref(),
        Some("alpha reasoning"),
        "the viewed chat must not show a background chat's thoughts"
    );
}

/// A visible request start clears THIS chat's reasoning only — another
/// chat mid-turn (or holding its last trace) is untouched.
#[test]
fn new_visible_request_clears_only_the_owning_chats_thoughts() {
    let mut app = app_with_chats(2);
    finish_turn_with_thoughts(&mut app, 0, "alpha reasoning");
    finish_turn_with_thoughts(&mut app, 1, "beta reasoning");

    app.select_chat(idx(&app, "chat-0"));
    submit_text(&mut app, "second question");

    assert_eq!(
        app.chats[idx(&app, "chat-0")].last_thoughts,
        None,
        "a new request start clears the requesting chat's thoughts"
    );
    assert_eq!(
        app.chats[idx(&app, "chat-1")].last_thoughts.as_deref(),
        Some("beta reasoning"),
        "other chats' thoughts are not the requester's business"
    );
}

/// A dequeued prompt is a visible request start for ITS chat only.
#[test]
fn dequeued_prompt_clears_only_that_chats_thoughts() {
    let mut app = app_with_chats(2);
    finish_turn_with_thoughts(&mut app, 0, "alpha reasoning");

    app.select_chat(idx(&app, "chat-1"));
    let first = submit_text(&mut app, "first");
    type_in(&mut app, "queued behind it");
    assert!(app.take_input().is_none(), "busy chat enqueues");
    app.select_chat(idx(&app, "chat-0"));
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: Some("beta reasoning".into()),
        request_id: event_id_of(&first.request_id),
        markdown: "beta answer".into(),
        thread_id: first.thread_id,
        model: None,
    });

    let background_thread = app.chats[idx(&app, "chat-1")].id.clone();
    let drained = app
        .dequeue_next_for(&background_thread)
        .expect("queued prompt drained");
    assert_eq!(drained.prompt, "queued behind it");
    assert_eq!(
        app.chats[idx(&app, "chat-1")].last_thoughts,
        None,
        "the dequeued start clears that chat's thoughts"
    );
    assert_eq!(
        app.chats[idx(&app, "chat-0")].last_thoughts.as_deref(),
        Some("alpha reasoning"),
        "the other chat keeps its trace"
    );
}

/// A hidden exchange (model-picker listing) resolves through the same
/// request pipeline but must not touch the chat's sticky thoughts — not
/// even when the frame hypothetically carries a thoughts field.
#[test]
fn hidden_exchange_never_touches_sticky_thoughts() {
    let mut app = app_with_chats(1);
    finish_turn_with_thoughts(&mut app, 0, "alpha reasoning");

    app.begin_model_picker();
    let bundle = app.take_picker_submission().expect("fetch staged");
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: Some("picker reasoning leak".into()),
        request_id: event_id_of(&bundle.request_id),
        markdown: "1. glm-5.2\n2. kimi-k2.7".into(),
        thread_id: bundle.thread_id,
        model: None,
    });

    assert_eq!(
        app.chats[0].last_thoughts.as_deref(),
        Some("alpha reasoning"),
        "a hidden exchange must never touch the visible thoughts"
    );
}

// ---- sticky last-known display state (ctx + model) --------------------
