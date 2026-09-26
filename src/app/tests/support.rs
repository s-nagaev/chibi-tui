use crate::app::*;
pub(super) use crate::model::Role;

pub(super) fn app_with_chats(n: usize) -> App {
    let chats = (0..n)
        .map(|i| Chat::new(format!("chat-{i}")))
        .collect::<Vec<_>>();
    App::new(chats)
}

/// Current sidebar index of the chat named `name`. Activity lifts
/// threads to the top, so tests must look chats up by their stable
/// name instead of by a position captured earlier.
pub(super) fn idx(app: &App, name: &str) -> usize {
    app.chats
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("chat {name} not in the sidebar"))
}

pub(super) fn type_in(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.input.input(crate::input::Input {
            key: crate::input::Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        });
    }
}

/// Simulate the Ctrl+R hotkey exactly as `handle_key` receives it.
pub(super) fn press_ctrl_r(app: &mut App) {
    app.begin_rename();
}

pub(super) fn submit_text(app: &mut App, text: &str) -> Submitted {
    type_in(app, text);
    let submitted = app.take_input().expect("prompt taken");
    app.begin_request(&submitted);
    submitted
}

/// Numeric event id the live glue task would stamp for this request id.
pub(super) fn event_id_of(request_id: &str) -> u64 {
    crate::live::wire_thread_id(request_id) as u64
}

/// Deliver a terminal Result event to `index`'s tracked request id.
pub(super) fn finish_chat(app: &mut App, index: usize) {
    finish_chat_with_model(app, index, None);
}

// ---- latest-turn usage/thoughts retention -------

pub(super) fn sample_usage() -> Usage {
    Usage {
        input_tokens: 120,
        output_tokens: 45,
        context_window: Some(200_000),
    }
}

/// Unique marker line for global-buffer assertions: other tests append
/// to the same process-global stream concurrently, so assertions are
/// contains-based / monotonic, never exact-position.
pub(super) fn unique_line(tag: &str) -> String {
    format!(
        "app-test-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

pub(super) fn log_state(app: &App) -> &LogViewerState {
    match &app.mode {
        Mode::LogViewer { state } => state,
        other => panic!("expected LogViewer mode, got {other:?}"),
    }
}

/// [`finish_chat`] with a model label.
pub(super) fn finish_chat_with_model(app: &mut App, index: usize, model: Option<&str>) {
    let request_id = app.chats[index].lifecycle.request_id().unwrap().to_owned();
    let thread_id = app.chats[index].id.clone();
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&request_id),
        markdown: "**done**".into(),
        thread_id,
        model: model.map(str::to_owned),
    });
}

// subagent counter aggregation + gating ----------

/// [`finish_chat_with_model`] with arbitrary answer content.
pub(super) fn finish_chat_with_content(app: &mut App, index: usize, markdown: &str) {
    let request_id = app.chats[index].lifecycle.request_id().unwrap().to_owned();
    let thread_id = app.chats[index].id.clone();
    app.apply_backend_event(BackendEvent::Result {
        usage: None,
        thoughts: None,
        request_id: event_id_of(&request_id),
        markdown: markdown.to_owned(),
        thread_id,
        model: None,
    });
}
