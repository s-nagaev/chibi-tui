use super::support::*;
use crate::app::*;

/// Model segment: `None` until a labelled result resolves, then the
/// newest label wins (per-message metadata reused, not a cached field).
#[test]
fn active_model_label_appears_on_result_resolution_and_updates() {
    let mut app = app_with_chats(1);
    assert_eq!(app.active_model_label(), None, "no label before any result");

    submit_text(&mut app, "first");
    finish_chat_with_model(&mut app, 0, Some("glm-5.2"));
    assert_eq!(app.active_model_label(), Some("glm-5.2"));

    submit_text(&mut app, "second");
    finish_chat_with_model(&mut app, 0, Some("kimi-k2.7"));
    assert_eq!(
        app.active_model_label(),
        Some("kimi-k2.7"),
        "each result resolution updates the label"
    );
}

/// An error resolution carries no model — the LAST KNOWN label of the
/// chat must survive it (rendered as the strip's model afterwards).
#[test]
fn error_resolution_keeps_last_known_model_label() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "good");
    finish_chat_with_model(&mut app, 0, Some("glm-5.2"));

    let submitted = submit_text(&mut app, "boom");
    app.apply_backend_event(BackendEvent::Error {
        request_id: event_id_of(&submitted.request_id),
        message: "backend exploded".into(),
        thread_id: Some(app.chats[0].id.clone()),
    });
    assert_eq!(
        app.active_model_label(),
        Some("glm-5.2"),
        "error resolution must not clear the last known label"
    );
}

/// Switching chats re-labels from THAT chat's last-known model — both
/// chats' labels stay independent.
#[test]
fn switching_chats_relabels_from_that_chats_last_model() {
    let mut app = app_with_chats(2);
    submit_text(&mut app, "chat0");
    {
        let i = idx(&app, "chat-0");
        finish_chat_with_model(&mut app, i, Some("glm-5.2"));
    }

    app.select_chat(idx(&app, "chat-1"));
    submit_text(&mut app, "chat1");
    {
        let i = idx(&app, "chat-1");
        finish_chat_with_model(&mut app, i, Some("kimi-k2.7"));
    }

    assert_eq!(app.active_model_label(), Some("kimi-k2.7"));
    app.select_chat(idx(&app, "chat-0"));
    assert_eq!(
        app.active_model_label(),
        Some("glm-5.2"),
        "switching back must re-label from chat 0's own last model"
    );
}

// invisible ACK / blank answers --------------
