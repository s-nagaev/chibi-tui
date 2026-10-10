//! Config-editor (F2) dispatch tests: modal isolation, the save → restart
//! staging round trip and the dirty-close grammar, driven through the REAL
//! `handle_key` path. States are injected via `begin_config_editor` with a
//! temp-path `ConfigState` — the user's real `~/chibi-bot/settings` is never
//! read or written here.

use crate::tests::support::*;
use chibi_tui::config_screen::{ConfigState, Confirm};

use crossterm::event::{KeyCode, KeyModifiers};
use std::path::PathBuf;

fn temp_settings_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-keymap-config-{tag}-{}-{}",
        std::process::id(),
        chibi_tui::history::new_thread_id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("settings")
}

fn open_editor(app: &mut chibi_tui::app::App, path: PathBuf) {
    app.begin_config_editor(ConfigState::load_at(path, true));
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfigEditor { .. }
    ));
}

/// FILESYSTEM_ACCESS is the FIRST row of the Core behaviour group. The group
/// is reached by navigating RELATIVE to its position in `Group::ALL` (not a
/// hardcoded arrow count), so registry re-orderings — like the 2231ad1
/// Integrations insertion — cannot silently land the selection on the wrong
/// group and leave the target key untogglable.
fn stage_a_bool_toggle(app: &mut chibi_tui::app::App) {
    let steps = chibi_tui::backend_settings::Group::ALL
        .iter()
        .position(|g| *g == chibi_tui::backend_settings::Group::CoreBehaviour)
        .expect("CoreBehaviour is listed in Group::ALL");
    for _ in 0..steps {
        press(app, KeyCode::Right, KeyModifiers::NONE);
    }
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

#[test]
fn config_editor_save_stages_the_reconnect_request() {
    let path = temp_settings_path("restart");
    std::fs::write(&path, "FILESYSTEM_ACCESS=false\n").expect("fixture");
    let mut app = app_with_chats(1);
    type_in(&mut app, "draft");
    open_editor(&mut app, path.clone());

    stage_a_bool_toggle(&mut app);
    // `s` saves (the dispatch performs the atomic write) and the idle
    // restart confirm opens.
    press(&mut app, KeyCode::Char('s'), KeyModifiers::NONE);
    let saved = std::fs::read_to_string(&path).expect("saved file");
    assert!(saved.contains("FILESYSTEM_ACCESS=true"), "written: {saved}");
    assert!(matches!(
        &app.mode,
        chibi_tui::app::Mode::ConfigEditor { state } if state.confirm() == Confirm::Restart
    ));
    assert!(
        app.reconnect_requested.is_none(),
        "no restart until the confirm"
    );

    // Enter (the shared quit-confirmation grammar) confirms: the editor
    // closes and the event-loop restart is staged.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.mode.is_normal(), "editor closed on restart");
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "editor pane back");
    assert!(
        app.reconnect_requested.is_some(),
        "reconnect staged for the event loop"
    );
    assert!(app.config_apply_pending, "one-shot toast armed");
    // The draft underneath was never touched.
    assert_eq!(app.input.lines().join(""), "draft");
}

#[test]
fn busy_save_offers_restart_when_idle_and_defers() {
    let path = temp_settings_path("defer");
    std::fs::write(&path, "FILESYSTEM_ACCESS=false\n").expect("fixture");
    let mut app = app_with_chats(1);
    open_editor(&mut app, path);
    // A request in flight → any_busy true.
    submit_text(&mut app, "running");
    assert!(app.any_busy());

    stage_a_bool_toggle(&mut app);
    press(&mut app, KeyCode::Char('s'), KeyModifiers::NONE);
    assert!(matches!(
        &app.mode,
        chibi_tui::app::Mode::ConfigEditor { state } if state.confirm() == Confirm::RestartBusy
    ));
    // `d`: restart when idle → the deferred flag, editor closed.
    press(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert!(app.config_restart_when_idle, "deferred restart staged");
    assert!(app.config_apply_pending);
    assert!(
        app.reconnect_requested.is_none(),
        "nothing fired while busy"
    );
}

#[test]
fn dirty_close_asks_and_d_discards() {
    let path = temp_settings_path("discard");
    std::fs::write(&path, "FILESYSTEM_ACCESS=false\n").expect("fixture");
    let mut app = app_with_chats(1);
    open_editor(&mut app, path.clone());

    stage_a_bool_toggle(&mut app);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    // Dirty close asks first: the editor stays open.
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfigEditor { .. }
    ));
    assert!(matches!(
        &app.mode,
        chibi_tui::app::Mode::ConfigEditor { state } if state.confirm() == Confirm::Discard
    ));
    // `d` discards (reloads the on-disk bytes) and closes.
    press(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert_eq!(
        std::fs::read_to_string(&path).expect("file"),
        "FILESYSTEM_ACCESS=false\n",
        "discard wrote nothing"
    );
}

#[test]
fn clean_close_returns_to_normal_with_focus_restored() {
    let path = temp_settings_path("clean");
    std::fs::write(&path, "DEFAULT_MODEL=\n").expect("fixture");
    let mut app = app_with_chats(1);
    open_editor(&mut app, path);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mode.is_normal());
    assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
    // F2 reopens on top of the fresh state (toggle round trip).
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfigEditor { .. }
    ));
}

#[test]
fn config_editor_does_not_open_over_other_popups() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
    // F2 is swallowed by the log viewer (modal family: one at a time).
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
}

#[test]
fn enter_inside_the_editor_never_submits_a_draft() {
    // The loop-level gate is covered by main.rs's snapshot list; this pins
    // the dispatch side: Enter inside the editor is consumed by the
    // component (edit staging / confirm grammar) and never reaches the
    // submit path.
    let path = temp_settings_path("enter");
    std::fs::write(&path, "DEFAULT_MODEL=\n").expect("fixture");
    let mut app = app_with_chats(1);
    type_in(&mut app, "draft");
    open_editor(&mut app, path);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfigEditor { .. }
    ));
    assert!(
        app.chats[0].messages.is_empty(),
        "no bubbles from the editor's Enter"
    );
}
