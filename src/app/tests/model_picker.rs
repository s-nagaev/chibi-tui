use super::support::*;
use crate::app::*;

/// The REAL captured `/model` listing — the parser's and the picker's
/// ground truth (see `model_picker.rs` for provenance).
pub(super) const CAPTURED_LISTING: &str =
    include_str!("../../../tests/fixtures/model_listing_captured.txt");

/// Open the picker as ^M does and consume the staged hidden fetch the
/// way the event loop does, so the test controls delivery timing.
pub(super) fn open_picker(app: &mut App) {
    app.begin_model_picker();
    assert!(matches!(app.mode, Mode::ModelPicking { .. }));
    let bundle = app.take_picker_submission().expect("hidden fetch staged");
    assert_eq!(bundle.prompt, "/model");
    assert_eq!(bundle.thread_id, app.chats[app.active].id);
}

/// Deliver a terminal Result for the chat's current tracked request the
/// way the backend source would (same mock-event shape as
/// [`finish_chat_with_model`]).
fn deliver_hidden_result(app: &mut App, markdown: &str, model: Option<&str>) {
    let request_id = app.chats[app.active]
        .lifecycle
        .request_id()
        .unwrap()
        .to_owned();
    let thread_id = app.chats[app.active].id.clone();
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&request_id),
        markdown: markdown.to_owned(),
        thread_id,
        model: model.map(str::to_owned),
    });
}

/// Stamp a last-known model label onto the chat WITHOUT a live request
/// (what a finished reply leaves behind — session metadata is stripped
/// from storage but lives in memory).
fn seed_model_label(app: &mut App, label: &str) {
    let mut message = Message::assistant("seeded");
    message.model = Some(label.to_owned());
    app.chats[0].messages.push(message);
}

#[test]
fn opening_the_picker_stages_a_hidden_fetch_without_bubbles() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    assert!(
        matches!(
            &app.mode,
            Mode::ModelPicking {
                state: ModelPickerState {
                    phase: ModelPickerPhase::Loading,
                    entries,
                    ..
                }
            } if entries.is_empty()
        ),
        "popup opens immediately in Loading"
    );
    assert_eq!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting {
            request_id: app.chats[0].lifecycle.request_id().unwrap().to_owned()
        },
        "the fetch occupies the normal request lifecycle"
    );
    assert!(app.chats[0].messages.is_empty(), "no transcript bubbles");
    assert_eq!(
        app.hidden_requests.values().next(),
        Some(&HiddenPurpose::FetchListing)
    );
}

#[test]
fn hidden_listing_resolves_into_the_picker_without_bubbles() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    let Mode::ModelPicking { state } = &app.mode else {
        panic!("picker still open");
    };
    assert_eq!(state.phase, ModelPickerPhase::Ready);
    assert_eq!(state.entries.len(), 104, "every captured row is listed");
    assert_eq!(state.selected, 0, "fresh chat has no label to preselect");
    assert!(app.chats[0].messages.is_empty(), "no transcript bubbles");
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
    assert!(app.hidden_requests.is_empty(), "purpose consumed");
}

#[test]
fn preselection_lands_on_a_unique_label() {
    let mut app = app_with_chats(1);
    // `4. Qwen Plus (Alibaba)` is the ONLY "Qwen Plus" row in the
    // captured listing — a clean unambiguous preselection target.
    seed_model_label(&mut app, "Qwen Plus");
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    let Mode::ModelPicking { state } = &app.mode else {
        panic!("picker open");
    };
    assert_eq!(state.selected, 3, "0-based index of listing row 4");
}

#[test]
fn ambiguous_labels_disable_preselection() {
    let mut app = app_with_chats(1);
    seed_model_label(&mut app, "Glm 5.2");
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    let Mode::ModelPicking { state } = &app.mode else {
        panic!("picker open");
    };
    // "Glm 5.2" appears under Cheaper Inference (row 16) AND Melious
    // (row 41) — ambiguous ⇒ best-effort gives up, first row selected.
    assert_eq!(state.selected, 0);
}

#[test]
fn preselection_survives_case_and_whitespace() {
    assert_eq!(
        crate::app::preselect_model_index(
            &parse_model_listing("1. Alpha (A)\n2. beta (B)\n"),
            Some("  BETA ")
        ),
        Some(1)
    );
    assert_eq!(crate::app::preselect_model_index(&[], Some("Alpha")), None);
    assert_eq!(
        crate::app::preselect_model_index(&parse_model_listing("1. Alpha (A)\n"), Some("unknown")),
        None
    );
}

#[test]
fn navigation_clamps_at_both_list_edges() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    app.model_picker_select_prev();
    assert_eq!(app.model_picker_selected(), 0, "clamped at the top");
    for _ in 0..200 {
        app.model_picker_select_next();
    }
    assert_eq!(
        app.model_picker_selected(),
        103,
        "clamped at the last of 104 rows"
    );
    assert_eq!(app.model_picker_entries().len(), 104);
}

#[test]
fn confirm_stages_a_hidden_selection_and_toasts_the_confirmation() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    for _ in 0..2 {
        app.model_picker_select_next();
    }
    app.confirm_model_picker();
    assert_eq!(app.mode, Mode::Normal, "selection closes the popup");
    assert_eq!(app.focus, Focus::Chat);
    let bundle = app.take_picker_submission().expect("selection staged");
    assert_eq!(bundle.prompt, "/model 3", "the row's OWN listing number");
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));
    assert!(app.chats[0].messages.is_empty(), "no transcript bubbles");
    assert_eq!(
        app.hidden_requests.values().next(),
        Some(&HiddenPurpose::SelectModel)
    );

    deliver_hidden_result(&mut app, "Selected model: Qwen3.5 Flash (Alibaba)", None);
    let (msg, _) = app.status_message.as_ref().expect("toast shown");
    assert_eq!(msg, "model: Qwen3.5 Flash (Alibaba)");
    assert!(app.chats[0].messages.is_empty(), "still no bubbles");
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
}

#[test]
fn unrecognizable_confirmation_degrades_to_the_raw_text() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    app.confirm_model_picker();
    deliver_hidden_result(&mut app, "totally unexpected body", None);
    let (msg, _) = app.status_message.as_ref().expect("toast shown");
    assert_eq!(msg, "model: totally unexpected body");
}

#[test]
fn hidden_switch_updates_last_known_model_metadata() {
    let mut app = app_with_chats(1);
    seed_model_label(&mut app, "old-model");
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    app.confirm_model_picker();
    deliver_hidden_result(&mut app, "Selected model: GLM 5.2 (ZhipuAI)", None);
    assert_eq!(
        app.active_model_label(),
        Some("GLM 5.2 (ZhipuAI)"),
        "the hidden switch is visible to the status strip via the override"
    );

    // The chat's NEXT visible reply stamps its own label and retires the
    // override (DoD: switch confirmed by the next reply's label).
    type_in(&mut app, "next prompt");
    let submitted = app.take_input().expect("prompt taken");
    app.begin_request(&submitted);
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "**done**".into(),
        thread_id: submitted.thread_id,
        model: Some("glm-5.2".into()),
    });
    assert_eq!(
        app.active_model_label(),
        Some("glm-5.2"),
        "message-derived label is the fresher truth again"
    );
    assert!(app.picker_model_labels.is_empty(), "override retired");
}

#[test]
fn fieldless_visible_replies_keep_the_hidden_switch_override() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    app.confirm_model_picker();
    deliver_hidden_result(
        &mut app,
        "Selected model: GLM 5.2 (ZhipuAI)",
        Some("glm-5.2"),
    );
    assert_eq!(app.active_model_label(), Some("glm-5.2"), "wire field wins");

    type_in(&mut app, "next prompt");
    let submitted = app.take_input().expect("prompt taken");
    app.begin_request(&submitted);
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "**fieldless**".into(),
        thread_id: submitted.thread_id,
        model: None,
    });
    assert_eq!(
        app.active_model_label(),
        Some("glm-5.2"),
        "a fieldless reply is no signal the model reverted"
    );
}

#[test]
fn unparsable_listing_degrades_to_toast_plus_visible_exchange() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    deliver_hidden_result(&mut app, "No models available.", None);
    assert_eq!(app.mode, Mode::Normal, "the dead popup closes");
    let (msg, _) = app.status_message.as_ref().expect("info toast");
    assert_eq!(msg, "model list unavailable");
    let messages = &app.chats[0].messages;
    assert_eq!(messages.len(), 2, "the raw exchange becomes visible");
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[0].markdown, "/model");
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[1].markdown, "No models available.");
    assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
}

#[test]
fn a_listing_arriving_after_the_picker_closed_is_absorbed_silently() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    assert!(app.close_model_picker());
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.chats[0].messages.is_empty(), "plumbing nobody awaits");
    assert!(app.status_message.is_none(), "no spurious toast");
    assert!(app.picker_model_labels.is_empty());
}

#[test]
fn esc_closes_without_acting_and_drops_a_parked_fetch() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "long running prompt");
    let submitted = app.take_input().expect("prompt taken");
    app.begin_request(&submitted); // chat is busy now

    app.begin_model_picker(); // fetch is PARKED (busy rules)
    assert!(app.take_picker_submission().is_none(), "busy ⇒ not staged");
    assert_eq!(
        app.hidden_queue.len(),
        1,
        "the fetch waits in the hidden FIFO"
    );
    assert!(matches!(app.mode, Mode::ModelPicking { .. }));

    assert!(app.close_model_picker());
    assert_eq!(app.mode, Mode::Normal);
    assert!(
        app.hidden_queue.is_empty(),
        "nobody awaits the parked fetch anymore"
    );
    // The chat's own visible request is untouched.
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "**done**".into(),
        thread_id: submitted.thread_id,
        model: None,
    });
    assert!(app.status_message.is_none(), "no leak from dropped fetch");
}

#[test]
fn busy_chat_parks_fetch_and_the_idle_drain_dispatches_it() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "long running prompt");
    let submitted = app.take_input().expect("prompt taken");
    app.begin_request(&submitted);

    app.begin_model_picker();
    let thread_id = app.chats[0].id.clone();
    assert!(
        app.take_deferred_hidden_request(&thread_id).is_none(),
        "busy chat: the hidden fetch stays parked"
    );

    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&submitted.request_id),
        markdown: "**done**".into(),
        thread_id: submitted.thread_id,
        model: None,
    });
    let deferred = app
        .take_deferred_hidden_request(&thread_id)
        .expect("idle drain dispatches the parked fetch");
    assert_eq!(deferred.prompt, "/model");
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));
    assert_eq!(
        app.chats[0]
            .messages
            .iter()
            .filter(|m| !m.pending && m.role == Role::Assistant)
            .count(),
        1,
        "only the visible prompt's own bubble; the hidden fetch adds none"
    );
    assert_eq!(
        app.hidden_requests.values().next(),
        Some(&HiddenPurpose::FetchListing)
    );
}

#[test]
fn a_confirmed_selection_survives_busy_and_the_drain_sends_it() {
    let mut app = app_with_chats(1);
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    // Force the busy state underneath the open popup (what a queued
    // drain or another chat's activity would produce).
    app.chats[0].lifecycle = ChatLifecycle::Awaiting {
        request_id: "busy-marker".to_owned(),
    };
    app.model_picker_select_next();
    app.confirm_model_picker(); // parks the selection, closes the popup
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.hidden_queue.len(), 1, "selection parked while busy");
    assert!(app.take_picker_submission().is_none());

    // The drain fires after the chat's terminal event resolves it to
    // Idle (white-box: the same state a real Result leaves behind).
    app.chats[0].lifecycle = ChatLifecycle::Idle;
    let thread_id = app.chats[0].id.clone();
    let deferred = app
        .take_deferred_hidden_request(&thread_id)
        .expect("Esc-drop must NOT touch a confirmed selection");
    assert_eq!(deferred.prompt, "/model 2");
}

#[test]
fn end_to_end_picker_flow_with_mock_events_leaves_no_bubbles() {
    let mut app = app_with_chats(1);
    // ^M: hidden fetch.
    open_picker(&mut app);
    // Backend answers the bare `/model` with the REAL captured listing.
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    // Enter: hidden `/model <n>`.
    app.confirm_model_picker();
    let bundle = app.take_picker_submission().expect("selection staged");
    assert_eq!(bundle.prompt, "/model 1");
    // Backend confirms.
    deliver_hidden_result(&mut app, "Selected model: Qwen3.8 Max (Alibaba)", None);
    // The whole exchange was plumbing: transcript untouched, feedback
    // arrived as a toast + last-known-model metadata.
    assert!(app.chats[0].messages.is_empty());
    let (msg, _) = app.status_message.as_ref().expect("toast");
    assert_eq!(msg, "model: Qwen3.8 Max (Alibaba)");
    assert_eq!(app.active_model_label(), Some("Qwen3.8 Max (Alibaba)"));
    // Reopening the picker preselects the just-switched model
    // (best-effort: the display tail matches listing row 1 uniquely).
    open_picker(&mut app);
    deliver_hidden_result(&mut app, CAPTURED_LISTING, None);
    let Mode::ModelPicking { state } = &app.mode else {
        panic!("picker open");
    };
    assert_eq!(state.selected, 0, "preselected via the staged override");
}

// ---- sidebar ordering: updated_at DESC, activity lifts to top -------
