//! Tests for the shared config-screen component (state + render), plus the
//! masking / secrets-invariant contracts. Uses explicit `load_at(temp)`
//! paths — no real `~/chibi-bot/settings` is ever touched.

use super::*;
use ratatui::backend::TestBackend;

fn temp_settings_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-config-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("settings")
}

fn write_file(path: &PathBuf, body: &str) {
    std::fs::write(path, body).expect("write fixture");
}

fn state_at(tag: &str, body: &str) -> ConfigState {
    let path = temp_settings_path(tag);
    write_file(&path, body);
    ConfigState::load_at(path, false)
}

fn press(state: &mut ConfigState, code: KeyCode, mods: KeyModifiers) -> ConfigAction {
    state.handle_key(crossterm::event::KeyEvent::new(code, mods))
}

/// Navigate to a row by key name, switching groups as needed.
fn select_row(state: &mut ConfigState, key: &str) {
    for (g, group) in Group::ALL.iter().enumerate() {
        if let Some(pos) = group.keys().position(|def| def.key == key) {
            while state.group_index() != g {
                press(state, KeyCode::Right, KeyModifiers::NONE);
            }
            state.row = pos;
            return;
        }
    }
    panic!("key {key} not in the registry");
}

//------------------------------------------------------------------------
// masking

#[test]
fn mask_secret_shows_last_four_only_when_long() {
    assert_eq!(
        mask_secret("sk-1234567890abcd"),
        "\u{2022}\u{2022}\u{2022}\u{2022}\u{2026}abcd"
    );
    assert_eq!(mask_secret("short"), "\u{2022}\u{2022}\u{2022}\u{2022}");
    assert_eq!(
        mask_secret("12345678"),
        "\u{2022}\u{2022}\u{2022}\u{2022}\u{2026}5678"
    );
}

#[test]
fn list_view_masks_secrets_and_marks_placeholders() {
    let mut state = state_at(
        "mask",
        "OPENAI_API_KEY=sk-1234567890abcd\n\
         ANTHROPIC_API_KEY=\n\
         # GROK_API_KEY=oldvalue9876543\n",
    );
    let rendered = render_to_string(&mut state);
    assert!(
        rendered.contains("\u{2022}\u{2022}\u{2022}\u{2022}\u{2026}abcd"),
        "long secret masked with last4: {rendered}"
    );
    assert!(!rendered.contains("sk-1234567890abcd"), "raw secret leaked");
    assert!(
        !rendered.contains("oldvalue9876543"),
        "commented-out secret masked too"
    );
    assert!(
        rendered.contains("\u{2014} (empty)"),
        "empty-but-set renders as empty: {rendered}"
    );
    assert!(
        rendered.contains("\u{2014} (not set)") && rendered.contains("[default]"),
        "absent keys marked: {rendered}"
    );
    assert!(
        rendered.contains("[commented]"),
        "commented keys marked: {rendered}"
    );
}

/// Render one frame to a flat string for substring assertions.
fn render_to_string(state: &mut ConfigState) -> String {
    let theme = Theme::tokyo_night();
    let mut terminal = Terminal::new(TestBackend::new(110, 30)).expect("terminal");
    terminal
        .draw(|f| render(f, state, &theme, f.area()))
        .expect("draw");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

//------------------------------------------------------------------------
// editing semantics

#[test]
fn bool_rows_toggle_with_enter_and_space() {
    let mut state = state_at("bool", "FILESYSTEM_ACCESS=false\n");
    select_row(&mut state, "FILESYSTEM_ACCESS");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(state.dirty(), "Enter toggles a Bool row");
    press(&mut state, KeyCode::Char(' '), KeyModifiers::NONE);
    assert_eq!(
        state.display_value(state.rows()[state.row]),
        "[false]",
        "two toggles round-trip"
    );
}

#[test]
fn bool_toggle_flips_true_and_false() {
    let mut state = state_at("bool2", "FILESYSTEM_ACCESS=false\n");
    select_row(&mut state, "FILESYSTEM_ACCESS");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        state.display_value(state.rows()[state.row]),
        "[true]",
        "false → true"
    );
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        state.display_value(state.rows()[state.row]),
        "[false]",
        "true → false"
    );
}

#[test]
fn enter_on_plain_row_opens_editor_and_stages_value() {
    let mut state = state_at("edit", "DEFAULT_MODEL=\n");
    select_row(&mut state, "DEFAULT_MODEL");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "glm-5.3".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    assert!(!state.dirty(), "nothing staged until commit");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(state.dirty(), "commit stages the edit");
    assert!(
        state.doc.render().contains("DEFAULT_MODEL=glm-5.3"),
        "staged value present: {}",
        state.doc.render()
    );
    // Dirty close asks first; Enter (shared grammar) saves and closes.
    let action = press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(action, ConfigAction::None, "dirty Esc opens the confirm");
    assert_eq!(state.confirm(), Confirm::Discard);
    let action = press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        action,
        ConfigAction::Save {
            close: true,
            overwrite: false
        }
    );
}

#[test]
fn esc_cancels_the_edit_without_staging() {
    let mut state = state_at("editcancel", "DEFAULT_MODEL=old\n");
    select_row(&mut state, "DEFAULT_MODEL");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut state, KeyCode::Char('x'), KeyModifiers::NONE);
    press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!state.dirty());
    assert_eq!(
        state.display_value(state.rows()[state.row]),
        "old",
        "value untouched"
    );
}

#[test]
fn invalid_value_keeps_the_editor_open_with_an_inline_error() {
    let mut state = state_at("invalid", "TEMPERATURE=1.0\n");
    select_row(&mut state, "TEMPERATURE");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "not-a-number".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!state.dirty(), "rejected value never stages");
    let rendered = render_to_string(&mut state);
    assert!(
        rendered.contains("expected a number"),
        "inline validation error rendered: {rendered}"
    );
    // Esc leaves the editor; the previous value survives.
    press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(state.display_value(state.rows()[state.row]), "1.0");
}

#[test]
fn dollar_var_values_are_rejected_inline() {
    let mut state = state_at("dollar", "DEFAULT_MODEL=x\n");
    select_row(&mut state, "DEFAULT_MODEL");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "a${SECRET}b".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!state.dirty(), "${{VAR}}-bearing value rejected");
    assert!(state.edit.is_some(), "editor stays open with the error");
}

#[test]
fn empty_secret_commit_clears_the_key() {
    let mut state = state_at("clear", "OPENAI_API_KEY=sk-1234567890abcd\n");
    select_row(&mut state, "OPENAI_API_KEY");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for _ in 0..20 {
        press(&mut state, KeyCode::Backspace, KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(state.dirty());
    let rendered = render_to_string(&mut state);
    assert!(
        rendered.contains("[commented]"),
        "cleared key renders its commented marker: {rendered}"
    );
    assert!(
        state.doc.render().contains("# OPENAI_API_KEY="),
        "clear comments the line out: {}",
        state.doc.render()
    );
}

#[test]
fn v_reveals_the_secret_only_while_editing() {
    let mut state = state_at("reveal", "OPENAI_API_KEY=sk-1234567890abcd\n");
    // List view: masked, always.
    select_row(&mut state, "OPENAI_API_KEY");
    assert!(!render_to_string(&mut state).contains("sk-1234567890abcd"));
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        !render_to_string(&mut state).contains("sk-1234567890abcd"),
        "editing without reveal stays masked"
    );
    press(&mut state, KeyCode::Char('v'), KeyModifiers::NONE);
    assert!(
        render_to_string(&mut state).contains("sk-1234567890abcd"),
        "v reveals the value while editing"
    );
    press(&mut state, KeyCode::Char('v'), KeyModifiers::NONE);
    assert!(
        !render_to_string(&mut state).contains("sk-1234567890abcd"),
        "v toggles back to masked"
    );
    // Reveal dies with the edit session: commit, then re-open — masked.
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!render_to_string(&mut state).contains("sk-1234567890abcd"));
}

//------------------------------------------------------------------------
// navigation

#[test]
fn arrows_switch_rows_and_groups() {
    let mut state = ConfigState::load_at(temp_settings_path("nav"), false);
    let group0 = state.group_index();
    let rows0 = state.row_count();
    assert!(rows0 > 1, "group 1 has provider rows");
    press(&mut state, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(state.row, 1);
    press(&mut state, KeyCode::Char('j'), KeyModifiers::NONE);
    assert_eq!(state.row, 2, "vim letters move too");
    press(&mut state, KeyCode::Char('g'), KeyModifiers::NONE);
    assert_eq!(state.row, 0);
    press(&mut state, KeyCode::Char('G'), KeyModifiers::NONE);
    assert_eq!(state.row, rows0 - 1, "G jumps to the last row");
    press(&mut state, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(state.group_index(), group0 + 1, "→ switches the group");
    assert_eq!(state.row, 0, "group switch resets the row");
    press(&mut state, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(state.group_index(), group0);
}

#[test]
fn page_keys_move_by_the_visible_window() {
    let mut state = ConfigState::load_at(temp_settings_path("page"), false);
    state.rows_visible = 5;
    press(&mut state, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(state.row, 5);
    press(&mut state, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(state.row, 0);
}

//------------------------------------------------------------------------
// save pipeline

#[test]
fn save_writes_atomically_and_clears_dirty() {
    let path = temp_settings_path("save");
    write_file(&path, "DEFAULT_MODEL=\n");
    let mut state = ConfigState::load_at(path.clone(), false);
    select_row(&mut state, "DEFAULT_MODEL");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "glm-5".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    let outcome = state.perform_save(false);
    assert_eq!(outcome, SaveOutcome::Saved);
    assert!(!state.dirty());
    let on_disk = std::fs::read_to_string(&path).expect("saved file");
    assert!(
        on_disk.contains("DEFAULT_MODEL=glm-5"),
        "value written: {on_disk}"
    );
}

#[test]
fn save_refuses_invalid_rows_and_reports_inline() {
    // An invalid value already in the file (hand-edited): the save-time
    // validation refuses the write and flags the row inline.
    let mut state = state_at("saverefuse", "TEMPERATURE=99\n");
    select_row(&mut state, "TEMPERATURE");
    let outcome = state.perform_save(false);
    assert_eq!(outcome, SaveOutcome::ValidationFailed);
    assert!(!state.dirty(), "nothing was staged, nothing written");
    assert!(
        state.errors.contains_key("TEMPERATURE"),
        "row error recorded"
    );
    assert!(
        render_to_string(&mut state).contains("out of range"),
        "inline error rendered on the row: "
    );
    // Fixing the row clears its error and the save succeeds.
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for _ in 0..2 {
        press(&mut state, KeyCode::Backspace, KeyModifiers::NONE);
    }
    for ch in "1.5".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(state.perform_save(false), SaveOutcome::Saved);
    assert!(state.errors.is_empty());
}

#[test]
fn external_change_opens_the_reload_overwrite_modal() {
    let path = temp_settings_path("changed");
    write_file(&path, "DEFAULT_MODEL=old\n");
    let mut state = ConfigState::load_at(path.clone(), false);
    select_row(&mut state, "DEFAULT_MODEL");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "new".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    // External edit between load and save.
    write_file(&path, "DEFAULT_MODEL=external\n");
    assert_eq!(state.perform_save(false), SaveOutcome::ChangedOnDisk);
    assert_eq!(state.confirm(), Confirm::FileChanged);
    // `o` overwrites anyway (guard dropped).
    let action = press(&mut state, KeyCode::Char('o'), KeyModifiers::NONE);
    assert_eq!(
        action,
        ConfigAction::Save {
            close: false,
            overwrite: true
        }
    );
    assert_eq!(state.perform_save(true), SaveOutcome::Saved);
    assert!(state.confirm() == Confirm::None);
    // A later `r` reloads the on-disk bytes, dropping edits.
    let mut state2 = ConfigState::load_at(path.clone(), false);
    select_row(&mut state2, "DEFAULT_MODEL");
    press(&mut state2, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "zzz".chars() {
        press(&mut state2, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state2, KeyCode::Enter, KeyModifiers::NONE);
    write_file(&path, "DEFAULT_MODEL=second\n");
    assert_eq!(state2.perform_save(false), SaveOutcome::ChangedOnDisk);
    press(&mut state2, KeyCode::Char('r'), KeyModifiers::NONE);
    assert!(!state2.dirty(), "reload drops the staged edits");
    assert_eq!(state2.confirm(), Confirm::None);
}

#[test]
fn missing_file_saves_create_it_with_sibling_dirs() {
    let path = temp_settings_path("missing");
    let mut state = ConfigState::load_at(path.clone(), false);
    assert!(state.file_missing());
    select_row(&mut state, "OPENAI_API_KEY");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "sk-test-0001".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(state.perform_save(false), SaveOutcome::Saved);
    let on_disk = std::fs::read_to_string(&path).expect("created");
    assert!(on_disk.contains("OPENAI_API_KEY=sk-test-0001"));
    assert!(
        path.parent().unwrap().join("data").exists(),
        "sibling dirs ensured"
    );
}

//------------------------------------------------------------------------
// restart staging (Shell B)

#[test]
fn after_save_stages_the_restart_confirm_when_idle() {
    let mut state = state_at("restart", "FILESYSTEM_ACCESS=false\n");
    state.restart_available = true;
    select_row(&mut state, "FILESYSTEM_ACCESS");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(state.perform_save(false), SaveOutcome::Saved);
    state.after_save(false);
    assert_eq!(state.confirm(), Confirm::Restart);
    let action = press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(action, ConfigAction::RestartNow);
}

#[test]
fn busy_save_offers_restart_anyway_or_when_idle() {
    let mut state = state_at("restartbusy", "FILESYSTEM_ACCESS=false\n");
    state.restart_available = true;
    select_row(&mut state, "FILESYSTEM_ACCESS");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(state.perform_save(false), SaveOutcome::Saved);
    state.after_save(true);
    assert_eq!(state.confirm(), Confirm::RestartBusy);
    // `d`: restart when idle (deferred).
    let action = press(&mut state, KeyCode::Char('d'), KeyModifiers::NONE);
    assert_eq!(action, ConfigAction::RestartWhenIdle);
    // `y`: restart anyway (in-flight lost).
    state.after_save(true);
    let action = press(&mut state, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(action, ConfigAction::RestartNow);
    // Esc: stay.
    state.after_save(true);
    let action = press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(action, ConfigAction::None);
    assert_eq!(state.confirm(), Confirm::None);
}

#[test]
fn no_restart_offer_without_restart_availability() {
    let mut state = state_at("norestart", "FILESYSTEM_ACCESS=false\n");
    // restart_available stays false (Shell A / mock sessions).
    select_row(&mut state, "FILESYSTEM_ACCESS");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(state.perform_save(false), SaveOutcome::Saved);
    state.after_save(false);
    assert_eq!(state.confirm(), Confirm::None, "no restart modal");
}

//------------------------------------------------------------------------
// secrets invariant

#[test]
fn debug_impl_never_exposes_secret_values() {
    let secret = "sk-super-secret-value-9999";
    let mut state = state_at("debugredact", &format!("OPENAI_API_KEY={secret}\n"));
    select_row(&mut state, "OPENAI_API_KEY");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    let dump = format!("{state:?}");
    assert!(!dump.contains(secret), "Debug leaked the document secret");
    assert!(!dump.contains("sk-"), "Debug leaked an edit-buffer secret");
    // The EditState Debug is redacted too.
    let edit = format!("{:?}", state.edit);
    assert!(!edit.contains(secret), "EditState Debug leaked the buffer");
}

#[test]
fn provider_key_configured_detects_set_keys() {
    let path = temp_settings_path("configured");
    // Missing file → not configured (load fails).
    assert!(load_settings(&path).is_err());
    write_file(&path, "OPENAI_API_KEY=\n");
    let doc = load_settings(&path).expect("parse").0;
    let none_set = !Group::ProviderKeys.keys().any(
        |def| matches!(doc.get(def.key), ValueState::Active { value } if !value.trim().is_empty()),
    );
    assert!(none_set, "empty key is not configured");
    write_file(&path, "OPENAI_API_KEY=sk-12345678\n");
    let doc = load_settings(&path).expect("parse").0;
    let some_set = Group::ProviderKeys.keys().any(
        |def| matches!(doc.get(def.key), ValueState::Active { value } if !value.trim().is_empty()),
    );
    assert!(some_set, "a real key IS configured");
}

//------------------------------------------------------------------------
// render smoke

#[test]
fn screen_renders_groups_descriptions_and_dirty_marker() {
    let mut state = state_at("render", "DEFAULT_MODEL=glm-5\nFILESYSTEM_ACCESS=false\n");
    select_row(&mut state, "DEFAULT_MODEL");
    let rendered = render_to_string(&mut state);
    assert!(
        rendered.contains("Backend configuration"),
        "title: {rendered}"
    );
    assert!(rendered.contains("Providers"), "group list: {rendered}");
    assert!(
        rendered.contains("Default LLM model when not specified"),
        "footer shows the DESCRIPTION: {rendered}"
    );
    assert!(!state.dirty());
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut state, KeyCode::Char('x'), KeyModifiers::NONE);
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(state.dirty());
    let dirty_render = render_to_string(&mut state);
    assert!(
        dirty_render.contains("unsaved"),
        "dirty marker in the title"
    );
}

#[test]
fn dev_banner_tracks_exact_chibi_env_value() {
    // SAFETY (single-threaded test binary ordering): setting an env var for
    // a render assertion; the render reads it per frame. Both values are
    // exercised inside ONE test — the var is process-global and the suite
    // runs in parallel threads.
    std::env::set_var("CHIBI_ENV", "dev");
    let mut state = state_at("devbanner", "DEFAULT_MODEL=x\n");
    let rendered = render_to_string(&mut state);
    assert!(
        rendered.contains("CHIBI_ENV=dev"),
        "dev warning banner in the header: {rendered}"
    );
    // Any value other than exactly `dev` (case-insensitive) leaves the file
    // live on the backend — the banner must NOT show and auto-open stays on.
    std::env::set_var("CHIBI_ENV", "production");
    let mut state = state_at("devbanner-prod", "DEFAULT_MODEL=x\n");
    let rendered = render_to_string(&mut state);
    std::env::set_var("CHIBI_ENV", "DEV");
    let mut state = state_at("devbanner-upper", "DEFAULT_MODEL=x\n");
    let rendered_upper = render_to_string(&mut state);
    std::env::remove_var("CHIBI_ENV");
    assert!(
        !rendered.contains("CHIBI_ENV=dev"),
        "no banner for CHIBI_ENV=production: {rendered}"
    );
    assert!(
        !crate::config_screen::is_dev_mode(),
        "is_dev_mode() is false for non-dev values"
    );
    assert!(
        rendered_upper.contains("CHIBI_ENV=dev"),
        "banner is case-insensitive on the value: {rendered_upper}"
    );
}

#[test]
fn discard_confirm_covers_save_discard_and_stay() {
    let mut state = state_at("discard", "DEFAULT_MODEL=old\n");
    select_row(&mut state, "DEFAULT_MODEL");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "new".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(state.confirm(), Confirm::Discard);
    // Esc/n stay.
    press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(state.confirm(), Confirm::None);
    assert!(state.dirty());
    // `d` discards and closes.
    press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    let action = press(&mut state, KeyCode::Char('d'), KeyModifiers::NONE);
    assert_eq!(action, ConfigAction::Closed);
    assert!(!state.dirty(), "discard reloaded the on-disk bytes");
    assert_eq!(state.display_value(state.rows()[state.row]), "old");
}

#[test]
fn discard_confirm_s_is_a_save_alias() {
    // The modal hint and HOTKEY_ROWS advertise `s/Enter save` — a bare `s`
    // must confirm-save-and-close, not fall through as a no-op (regression:
    // `s` used to hit the shared grammar's Swallow arm and the modal stayed
    // open forever).
    let mut state = state_at("discard-save", "DEFAULT_MODEL=old\n");
    select_row(&mut state, "DEFAULT_MODEL");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    for ch in "new".chars() {
        press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(state.confirm(), Confirm::Discard);
    let action = press(&mut state, KeyCode::Char('s'), KeyModifiers::NONE);
    assert_eq!(
        action,
        ConfigAction::Save {
            close: true,
            overwrite: false
        },
        "`s` saves and closes from the Discard modal"
    );
    assert_eq!(state.confirm(), Confirm::None);
    assert!(state.dirty(), "save action leaves the staged change intact");
}
