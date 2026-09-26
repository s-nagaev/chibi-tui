use super::support::*;
use crate::app::*;

/// Submit a prompt on the ACTIVE chat and resolve it with a labelled LLM
/// answer carrying usage (the full latest-turn metadata a real chat turn
/// brings).
fn finish_llm_turn(app: &mut App, model: &str, usage: Usage) {
    let submitted = submit_text(app, "turn prompt");
    app.apply_backend_event(BackendEvent::Result {
        usage: Some(usage),
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "**answer**".into(),
        thread_id: submitted.thread_id,
        model: Some(model.to_owned()),
    });
}

#[test]
fn command_frame_keeps_last_known_ctx_and_model() {
    let mut app = app_with_chats(1);
    finish_llm_turn(&mut app, "glm-5.2", sample_usage());

    // A visible command exchange: the answer frame carries neither usage
    // nor model (command results are backend plumbing, not LLM turns).
    let command = submit_text(&mut app, "/help");
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&command.request_id),
        markdown: "available commands: /help /reset".into(),
        thread_id: command.thread_id,
        model: None,
    });

    assert_eq!(
        app.last_turn_usage,
        Some(sample_usage()),
        "ctx keeps the last known usage across the command frame"
    );
    assert_eq!(
        app.active_model_label(),
        Some("glm-5.2"),
        "panel keeps the last known model across the command frame"
    );
    let messages = &app.chats[0].messages;
    assert_eq!(
        messages[messages.len() - 1].model_label(),
        None,
        "the command answer carries no annotation"
    );
    assert_eq!(
        messages[messages.len() - 3].model_label(),
        Some("glm-5.2"),
        "the LLM answer keeps its own annotation"
    );
}

#[test]
fn model_change_mid_session_updates_panel_on_next_result() {
    let mut app = app_with_chats(1);
    finish_llm_turn(&mut app, "glm-5.2", sample_usage());
    assert_eq!(app.active_model_label(), Some("glm-5.2"));

    let switched = Usage {
        input_tokens: 900,
        output_tokens: 10,
        context_window: Some(131_072),
    };
    finish_llm_turn(&mut app, "kimi-k2.7", switched);

    assert_eq!(
        app.active_model_label(),
        Some("kimi-k2.7"),
        "panel follows the next frame that carries a model"
    );
    assert_eq!(
        app.last_turn_usage,
        Some(switched),
        "ctx follows the next frame that carries usage"
    );
}

#[test]
fn fresh_session_starts_with_unknown_ctx_and_model() {
    let app = app_with_chats(1);
    assert_eq!(app.last_turn_usage, None, "no usage before any frame");
    assert_eq!(
        app.active_model_label(),
        None,
        "no model label before any frame"
    );
    assert!(
        app.chats[0]
            .messages
            .iter()
            .all(|m| m.model_label().is_none()),
        "no annotation anywhere in a fresh session"
    );
}

#[test]
fn restart_restores_sticky_display_state_from_persisted_snapshot() {
    let mut app = app_with_chats(1);
    finish_llm_turn(&mut app, "glm-5.2", sample_usage());
    assert!(app.last_turn_usage.is_some());
    assert_eq!(app.active_model_label(), Some("glm-5.2"));

    // Restart seam: the event loop persists the chat and a new App loads
    // it. Per-message labels travel with their messages, while the
    // thread-level last-known usage/model pair rides along with the
    // snapshot and seeds the display state.
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-sticky-restart-{}-{}",
        std::process::id(),
        crate::history::new_thread_id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    crate::history::save_chat_in(Some(&dir), &app.chats[0]).expect("save");
    let restored = crate::history::load_chats_from(Some(&dir));
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(restored.len(), 1, "one snapshot file");
    assert_eq!(
        restored[0].messages.last().and_then(|m| m.model_label()),
        Some("glm-5.2"),
        "the answer's label persists with its message"
    );
    assert_eq!(
        restored[0].last_usage,
        Some(sample_usage()),
        "last-known usage persists with the thread"
    );
    assert_eq!(
        restored[0].last_model.as_deref(),
        Some("glm-5.2"),
        "last-known model persists with the thread"
    );

    let fresh = App::new(restored);
    assert_eq!(
        fresh.last_turn_usage,
        Some(sample_usage()),
        "ctx segment is seeded from the persisted usage"
    );
    assert_eq!(
        fresh.active_model_label(),
        Some("glm-5.2"),
        "panel model is seeded from the persisted label"
    );
    assert_eq!(
        fresh.chats[0].messages.last().and_then(|m| m.model_label()),
        Some("glm-5.2"),
        "restored answers keep their captured labels"
    );
}

// restore activation -------------------------

/// A restored pointer must behave exactly like the user picking the
/// thread: the selection moves, and the sticky ctx segment is re-seeded
/// from THAT thread's snapshot (not the first chat's). A dangling id
/// changes nothing at all.
#[test]
fn activate_thread_opens_restored_thread_and_seeds_its_sticky_state() {
    let mut alpha = Chat::new("alpha");
    alpha.last_usage = Some(Usage {
        input_tokens: 10,
        output_tokens: 1,
        context_window: Some(100_000),
    });
    let mut beta = Chat::new("beta");
    beta.last_usage = Some(Usage {
        input_tokens: 900_000,
        output_tokens: 1,
        context_window: Some(1_000_000),
    });
    beta.last_model = Some("glm-5.2".to_owned());

    let mut app = App::new(vec![alpha, beta]);
    assert_eq!(app.active, 0, "default startup selects the first chat");
    assert_eq!(
        app.last_turn_usage.map(|u| u.input_tokens),
        Some(10),
        "ctx segment seeded from the first chat"
    );

    let restored_id = app.chats[1].id.clone();
    app.activate_thread(&restored_id);
    assert_eq!(app.active_thread_id(), Some(restored_id.as_str()));
    assert_eq!(app.chat_title(), "beta");
    assert_eq!(
        app.last_turn_usage.map(|u| u.input_tokens),
        Some(900_000),
        "ctx segment re-seeded from the restored thread's snapshot"
    );
    assert_eq!(
        app.active_model_label(),
        Some("glm-5.2"),
        "panel model staged for the restored thread"
    );

    // A dangling pointer is a silent no-op: selection and sticky state
    // are untouched.
    app.activate_thread(&crate::history::new_thread_id());
    assert_eq!(app.active_thread_id(), Some(restored_id.as_str()));
    assert_eq!(app.chat_title(), "beta");
    assert_eq!(app.last_turn_usage.map(|u| u.input_tokens), Some(900_000));
}

#[test]
fn switching_threads_updates_ctx_value() {
    let mut alpha = Chat::new("alpha");
    alpha.last_usage = Some(Usage {
        input_tokens: 10,
        output_tokens: 1,
        context_window: Some(100_000),
    });
    let mut beta = Chat::new("beta");
    beta.last_usage = Some(Usage {
        input_tokens: 900_000,
        output_tokens: 1,
        context_window: Some(1_000_000),
    });

    let mut app = App::new(vec![alpha, beta]);
    assert_eq!(
        app.last_turn_usage.map(|u| u.input_tokens),
        Some(10),
        "ctx segment seeded from the startup chat"
    );

    app.select_next();
    assert_eq!(
        app.last_turn_usage.map(|u| u.input_tokens),
        Some(900_000),
        "switching threads re-seeds the ctx segment from the entered thread"
    );

    app.select_prev();
    assert_eq!(
        app.last_turn_usage.map(|u| u.input_tokens),
        Some(10),
        "switching back re-seeds from the entered thread again"
    );
}

#[test]
fn switching_to_thread_without_usage_shows_neutral_ctx() {
    let mut alpha = Chat::new("alpha");
    alpha.last_usage = Some(Usage {
        input_tokens: 10,
        output_tokens: 1,
        context_window: Some(100_000),
    });
    let mut beta = Chat::new("beta");
    // Deterministic order: alpha on top, beta behind (newest-first).
    beta.updated_at = alpha.updated_at.saturating_sub(1);

    let mut app = App::new(vec![alpha, beta]);
    assert_eq!(app.last_turn_usage.map(|u| u.input_tokens), Some(10));

    app.select_next();
    assert_eq!(
        app.last_turn_usage, None,
        "a thread without usage resets the ctx segment to neutral, not stale"
    );

    app.select_prev();
    assert_eq!(
        app.last_turn_usage.map(|u| u.input_tokens),
        Some(10),
        "switching back restores the carrying thread's value"
    );
}

#[test]
fn usage_frame_after_switch_still_updates_ctx() {
    let mut alpha = Chat::new("alpha");
    alpha.last_usage = Some(Usage {
        input_tokens: 10,
        output_tokens: 1,
        context_window: Some(100_000),
    });
    let mut beta = Chat::new("beta");
    // Deterministic order: alpha on top, beta behind (newest-first).
    beta.updated_at = alpha.updated_at.saturating_sub(1);

    let mut app = App::new(vec![alpha, beta]);
    // Selections go by NAME: activity lifts threads to the top, so
    // positional arrows are not stable here.
    app.select_chat(idx(&app, "beta"));
    assert_eq!(app.last_turn_usage, None, "neutral after the switch");

    finish_llm_turn(&mut app, "glm-5.2", sample_usage());
    assert_eq!(
        app.last_turn_usage,
        Some(sample_usage()),
        "an incoming usage frame overwrites the ctx segment after a switch"
    );

    app.select_chat(idx(&app, "alpha"));
    assert_eq!(
        app.last_turn_usage.map(|u| u.input_tokens),
        Some(10),
        "switching away reads the per-thread mirror, not the sticky field"
    );
    app.select_chat(idx(&app, "beta"));
    assert_eq!(
        app.last_turn_usage,
        Some(sample_usage()),
        "the live turn's usage is mirrored onto its thread for later switches"
    );
}

#[test]
fn terminal_turn_persists_last_known_usage_and_model() {
    let mut app = app_with_chats(1);
    finish_llm_turn(&mut app, "glm-5.2", sample_usage());

    // The event-loop persist seam (terminal events and the final
    // snapshot before shutdown both go through save_chat_in) must carry
    // the thread's last-known pair into the JSON document itself.
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-persist-last-{}-{}",
        std::process::id(),
        crate::history::new_thread_id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = crate::history::save_chat_in(Some(&dir), &app.chats[0]).expect("save");
    let raw = std::fs::read_to_string(&path).expect("read snapshot");
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        doc["last_model"], "glm-5.2",
        "model recorded on the terminal turn: {raw}"
    );
    assert_eq!(
        doc["last_usage"]["input_tokens"], 120,
        "usage recorded on the terminal turn: {raw}"
    );
    assert_eq!(doc["last_usage"]["output_tokens"], 45);
    assert_eq!(doc["last_usage"]["context_window"], 200_000);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn model_change_is_reflected_in_the_snapshot_on_next_persist() {
    let mut app = app_with_chats(1);
    finish_llm_turn(&mut app, "glm-5.2", sample_usage());
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-model-switch-{}-{}",
        std::process::id(),
        crate::history::new_thread_id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    crate::history::save_chat_in(Some(&dir), &app.chats[0]).expect("first save");

    let switched = Usage {
        input_tokens: 900,
        output_tokens: 10,
        context_window: Some(131_072),
    };
    finish_llm_turn(&mut app, "kimi-k2.7", switched);
    crate::history::save_chat_in(Some(&dir), &app.chats[0]).expect("resave");

    let loaded = crate::history::load_chats_from(Some(&dir));
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(loaded.len(), 1, "same thread id, same snapshot file");
    assert_eq!(
        loaded[0].last_model.as_deref(),
        Some("kimi-k2.7"),
        "the next persist carries the switched model"
    );
    assert_eq!(
        loaded[0].last_usage,
        Some(switched),
        "the next persist carries the switched turn's usage"
    );
}
