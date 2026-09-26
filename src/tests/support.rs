use crossterm::event::{KeyCode, KeyModifiers};

pub(crate) use chibi_tui::app::Chat;
pub(crate) use chibi_tui::model::Message;

pub(crate) fn key_event(code: KeyCode, modifiers: KeyModifiers) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, modifiers)
}

pub(crate) fn press(app: &mut chibi_tui::app::App, code: KeyCode, modifiers: KeyModifiers) {
    crate::keymap::handle_key(app, key_event(code, modifiers));
}

pub(crate) fn app_with_chats(n: usize) -> chibi_tui::app::App {
    let chats = (0..n)
        .map(|i| Chat::new(format!("chat-{i}")))
        .collect::<Vec<_>>();
    chibi_tui::app::App::new(chats)
}

pub(crate) fn submit_text(app: &mut chibi_tui::app::App, text: &str) -> chibi_tui::app::Submitted {
    for ch in text.chars() {
        app.input.input(chibi_tui::input::Input {
            key: chibi_tui::input::Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        });
    }
    let submitted = app.take_input().expect("prompt taken");
    app.begin_request(&submitted);
    submitted
}

/// Type plain characters through the full `handle_key` path (as a real
/// keyboard would), so tests cover the routing, not just the textarea.
pub(crate) fn type_in(app: &mut chibi_tui::app::App, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
}

pub(crate) fn temp_history_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-last-thread-{tag}-{}-{}",
        std::process::id(),
        chibi_tui::history::new_thread_id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

pub(crate) fn persisted_chat(
    name: &str,
    usage: Option<chibi_tui::protocol::Usage>,
) -> chibi_tui::app::Chat {
    let mut chat = Chat::new(name);
    chat.messages.push(Message::user("question"));
    chat.last_usage = usage;
    chat
}

use chibi_tui::app::{ModelPickerPhase, ModelPickerState};
use chibi_tui::model_picker::ModelEntry;

/// A Ready picker injected directly (the listing resolution itself is
/// covered by the app-level tests; here only KEY ROUTING matters).
pub(crate) fn inject_ready_picker(app: &mut chibi_tui::app::App, rows: usize) {
    let entries = (1..=rows)
        .map(|n| ModelEntry {
            number: n,
            name: format!("model-{n}"),
            provider: Some("prov".to_owned()),
            active: false,
        })
        .collect();
    app.mode = chibi_tui::app::Mode::ModelPicking {
        state: ModelPickerState {
            phase: ModelPickerPhase::Ready,
            entries,
            selected: 0,
        },
    };
}
