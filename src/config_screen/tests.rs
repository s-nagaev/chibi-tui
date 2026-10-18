//! Tests for the shared config-screen component (state + render), plus the
//! masking / secrets-invariant contracts. Uses explicit `load_at(temp)`
//! paths; no real `~/chibi-bot/settings` is ever touched.

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
        rendered.contains("- (empty)"),
        "empty-but-set renders as empty: {rendered}"
    );
    assert!(
        rendered.contains("- (not set)") && rendered.contains("[default]"),
        "absent keys marked: {rendered}"
    );
    assert!(
        rendered.contains("[commented]"),
        "commented keys marked: {rendered}"
    );
}

#[test]
fn valueless_rows_surface_the_registry_default() {
    let mut state = state_at("defaults", "");
    select_row(&mut state, "FAILOVER_CHAIN_MASTER");
    assert_eq!(
        state.display_value(state.rows()[state.row]),
        "- (default: auto)",
        "absent row with a non-empty default shows it"
    );
    let rendered = render_to_string(&mut state);
    assert!(
        rendered.contains("- (default: auto)") && rendered.contains("[default]"),
        "default placeholder and tag rendered together: {rendered}"
    );
    // An empty-default row keeps the plain placeholder.
    select_row(&mut state, "DEFAULT_MODEL");
    assert_eq!(
        state.display_value(state.rows()[state.row]),
        "- (not set)",
        "empty-default row stays (not set)"
    );
}

#[test]
fn empty_value_with_a_default_surfaces_it_too() {
    // Active-but-empty value: the default still applies.
    let mut state = state_at("defaultsempty", "FAILOVER_CHAIN_MASTER=\n");
    select_row(&mut state, "FAILOVER_CHAIN_MASTER");
    assert_eq!(
        state.display_value(state.rows()[state.row]),
        "- (default: auto)",
        "empty value falls back to the default"
    );
    // A real value always wins over the default.
    let mut state = state_at("defaultswin", "FAILOVER_CHAIN_MASTER=provider/model\n");
    select_row(&mut state, "FAILOVER_CHAIN_MASTER");
    assert_eq!(
        state.display_value(state.rows()[state.row]),
        "provider/model",
        "real value beats the default"
    );
}

#[test]
fn commented_empty_value_with_no_default_stays_empty_placeholder() {
    let mut state = state_at("defaultcommented", "# MODERATION_MODEL=\n");
    select_row(&mut state, "MODERATION_MODEL");
    let rendered = render_to_string(&mut state);
    assert!(
        rendered.contains("- (empty) [commented]"),
        "commented empty template key keeps its placeholder: {rendered}"
    );
}

/// Render one frame to a flat string for substring assertions.
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
    // Reveal dies with the edit session: commit, then re-open (masked).
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!render_to_string(&mut state).contains("sk-1234567890abcd"));
}

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
    // exercised inside ONE test; the var is process-global and the suite
    // runs in parallel threads.
    std::env::set_var("CHIBI_ENV", "dev");
    let mut state = state_at("devbanner", "DEFAULT_MODEL=x\n");
    let rendered = render_to_string(&mut state);
    assert!(
        rendered.contains("CHIBI_ENV=dev"),
        "dev warning banner in the header: {rendered}"
    );
    // Any value other than exactly `dev` (case-insensitive) leaves the file
    // live on the backend, so the banner must NOT show and auto-open stays on.
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
    // The modal hint and HOTKEY_ROWS advertise `s/Enter save`, so a bare `s`
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

// mouse seam: render-fed geometry

/// Render one frame on a TestBackend of the given size (geometry tests).
fn render_once_sized(state: &mut ConfigState, width: u16, height: u16) {
    let theme = Theme::tokyo_night();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|f| render(f, state, &theme, f.area()))
        .expect("draw");
}

#[test]
fn render_populates_the_geometry_snapshot() {
    let mut state = state_at("geom", "DEFAULT_MODEL=glm-5\nFILESYSTEM_ACCESS=false\n");
    assert!(
        state.geometry.is_none(),
        "no geometry before the first render"
    );
    render_once_sized(&mut state, 110, 30);
    let geo = state.geometry.expect("geometry after the first render");
    assert_eq!(geo.groups.x, 1, "left pane inside the outer border");
    assert_eq!(
        geo.groups.width, 26,
        "left pane keeps its 26-col constraint"
    );
    assert_eq!(
        geo.keys_inner.x,
        geo.groups.x + geo.groups.width + 1,
        "inner rows rect skips the Borders::LEFT divider column"
    );
    assert_eq!(geo.keys_inner.y, geo.groups.y, "panes share the top");
    assert_eq!(
        geo.keys_inner.height, geo.groups.height,
        "panes share the height"
    );
    assert_eq!(
        geo.visible, geo.keys_inner.height as usize,
        "visible = pane height"
    );
    assert_eq!(geo.row_offset, state.row_offset, "snapshot of the offset");
}

#[test]
fn pane_at_distinguishes_panes_divider_and_outside() {
    let mut state = state_at("panehit", "DEFAULT_MODEL=x\n");
    render_once_sized(&mut state, 110, 30);
    let geo = state.geometry.expect("rendered");
    assert_eq!(
        state.pane_at(geo.groups.x, geo.groups.y),
        Some(ConfigPane::Groups),
        "top-left of the groups pane"
    );
    assert_eq!(
        state.pane_at(geo.keys_inner.x, geo.keys_inner.y),
        Some(ConfigPane::Rows),
        "top-left of the rows pane"
    );
    assert_eq!(
        state.pane_at(geo.keys_inner.x - 1, geo.keys_inner.y),
        None,
        "the Borders::LEFT divider column is neither pane"
    );
    assert_eq!(state.pane_at(0, 0), None, "outer border / outside");
    assert_eq!(
        state.pane_at(geo.groups.x, geo.groups.y + geo.groups.height),
        None,
        "one row below the panes"
    );
}

#[test]
fn mouse_selections_map_and_keep_the_offset_snapshot_consistent() {
    let mut state = state_at("geomclick", "DEFAULT_MODEL=x\n");
    // 12-row terminal: the routing group (12 rows) does NOT fit the ~8
    // visible rows, so clicks must scroll the window via clamp_offset.
    render_once_sized(&mut state, 60, 12);
    let geo = state.geometry.expect("rendered");
    let routing = Group::ALL
        .iter()
        .position(|g| *g == Group::ModelRouting)
        .expect("Model Routing group");
    assert!(
        state.row_count() > geo.visible,
        "the group overflows the pane so scrolling is exercised"
    );
    // Click the group row in the LEFT pane (no scroll offset there).
    state.select_group_at(geo.groups.y + routing as u16);
    assert_eq!(state.group_index(), routing);
    assert_eq!(state.row, 0, "group switch resets the row");
    assert_eq!(state.geometry.unwrap().row_offset, 0);
    // Click 10 rows down: idx 10 selects and scrolls the window.
    let geo = state.geometry.unwrap();
    state.select_row_at(geo.keys_inner.y + 10);
    assert_eq!(state.row, 10);
    let expected_offset = 10 + 1 - geo.visible;
    assert_eq!(
        state.geometry.unwrap().row_offset,
        expected_offset,
        "snapshot follows clamp_offset"
    );
    // A SECOND click without a re-render maps through the refreshed
    // snapshot: the pane top is now row `expected_offset`, not row 0.
    let geo = state.geometry.unwrap();
    state.select_row_at(geo.keys_inner.y);
    assert_eq!(state.row, expected_offset);
    assert_eq!(state.geometry.unwrap().row_offset, expected_offset);
}

#[test]
fn select_row_at_ignores_clicks_past_the_last_row() {
    let mut state = state_at("geombelow", "DEFAULT_MODEL=x\n");
    render_once_sized(&mut state, 110, 30);
    let geo = state.geometry.expect("rendered");
    let last = state.row_count().saturating_sub(1);
    state.row = last;
    // idx == len (one past the final row) → ignored.
    state.select_row_at(geo.keys_inner.y + state.row_count() as u16);
    assert_eq!(state.row, last, "click one past the last row ignored");
    // Far below the pane → ignored too.
    state.select_row_at(geo.keys_inner.y + geo.keys_inner.height + 5);
    assert_eq!(state.row, last, "click far below the pane ignored");
    // Empty-group branch: with an out-of-window snapshot the mapped index
    // is always past the last row (the same `idx >= row_count` comparison
    // an empty group hits) → ignored.
    state.row_offset = 1_000;
    if let Some(geo) = &mut state.geometry {
        geo.row_offset = 1_000;
    }
    state.select_row_at(geo.keys_inner.y);
    assert_eq!(state.row, last, "unmappable index ignored");
}

#[test]
fn select_group_at_ignores_out_of_range_and_routes_in_range() {
    let mut state = state_at("geomgroup", "DEFAULT_MODEL=x\n");
    render_once_sized(&mut state, 110, 30);
    let geo = state.geometry.expect("rendered");
    let n = Group::ALL.len();
    state.select_group_at(geo.groups.y);
    assert_eq!(state.group_index(), 0, "first group row selects group 0");
    // Out of range (delta-based switch_group would wrap!) → ignored ENTIRELY.
    state.select_group_at(geo.groups.y + n as u16 + 2);
    assert_eq!(state.group_index(), 0, "below the last group ignored");
    assert_eq!(state.row, 0);
    state.select_group_at(geo.groups.y.saturating_sub(1));
    assert_eq!(state.group_index(), 0, "above the pane ignored");
    // In range → routed through switch_group.
    let target = 2;
    state.select_group_at(geo.groups.y + target as u16);
    assert_eq!(state.group_index(), target);
    assert_eq!(state.row, 0, "group switch resets the row");
}

#[test]
fn mouse_seam_is_a_no_op_before_the_first_render() {
    let mut state = state_at("geomnoop", "DEFAULT_MODEL=x\n");
    assert!(state.geometry.is_none());
    state.select_row_at(5);
    state.select_group_at(1);
    assert_eq!(state.pane_at(0, 0), None, "no geometry → no hit");
    assert_eq!(state.row, 0);
    assert_eq!(state.group_index(), 0);
}

#[test]
fn geometry_is_cloned_but_excluded_from_equality() {
    let mut a = state_at("geomeq", "DEFAULT_MODEL=x\n");
    render_once_sized(&mut a, 110, 30);
    let b = a.clone();
    assert!(b.geometry.is_some(), "Clone copies the geometry");
    assert_eq!(a, b, "equal states compare equal with geometry present");
    let mut c = a.clone();
    c.geometry = None;
    assert_eq!(a, c, "geometry divergence does not affect PartialEq");
}

// mouse handling: wheel + click routing (the shared lib entry point)

/// Hand-built mouse event at a terminal position, as crossterm delivers it.
fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn wheel_over_the_rows_pane_moves_the_selection_with_clamping() {
    let mut state = state_at("mouserows", "DEFAULT_MODEL=x\n");
    // 60x12: the pane shows ~8 of the 31 provider rows, so both the
    // selection movement and the window scrolling are exercised.
    render_once_sized(&mut state, 60, 12);
    let geo = state.geometry.expect("rendered");
    let visible = geo.keys_inner.height.max(1) as usize;
    assert!(state.row_count() > visible, "the group overflows the pane");
    let cx = geo.keys_inner.x + geo.keys_inner.width / 2;
    let cy = geo.keys_inner.y + 1;

    // One down-notch moves the SELECTION by WHEEL_STEP (not the window).
    state.handle_mouse(mouse_at(MouseEventKind::ScrollDown, cx, cy));
    assert_eq!(state.row, 3, "wheel down = +WHEEL_STEP rows");
    assert_eq!(
        state.geometry.unwrap().row_offset,
        0,
        "window untouched yet"
    );

    // Six more notches: row 21, the window follows via clamp_offset and the
    // stored snapshot is refreshed WITHOUT a re-render.
    for _ in 0..6 {
        state.handle_mouse(mouse_at(MouseEventKind::ScrollDown, cx, cy));
    }
    assert_eq!(state.row, 21);
    let expected_offset = 21 + 1 - visible;
    assert_eq!(
        state.geometry.unwrap().row_offset,
        expected_offset,
        "snapshot follows clamp_offset between renders"
    );

    // Wheel up past the top clamps the selection at row 0 (no underflow).
    for _ in 0..10 {
        state.handle_mouse(mouse_at(MouseEventKind::ScrollUp, cx, cy));
    }
    assert_eq!(state.row, 0, "top edge clamps");
    assert_eq!(state.geometry.unwrap().row_offset, 0);

    // ...and wheel down past the bottom clamps at the last row.
    for _ in 0..20 {
        state.handle_mouse(mouse_at(MouseEventKind::ScrollDown, cx, cy));
    }
    assert_eq!(state.row, state.row_count() - 1, "bottom edge clamps");
    let expected_offset = state.row_count() - visible;
    assert_eq!(state.geometry.unwrap().row_offset, expected_offset);
}

#[test]
fn wheel_over_the_groups_pane_moves_the_group_selection() {
    let mut state = state_at("mousegroups", "DEFAULT_MODEL=x\n");
    render_once_sized(&mut state, 110, 30);
    let geo = state.geometry.expect("rendered");
    let cx = geo.groups.x;
    let cy = geo.groups.y;

    // Park the selection mid-group first: the group switch must reset it.
    press(&mut state, KeyCode::Down, KeyModifiers::NONE);
    press(&mut state, KeyCode::Down, KeyModifiers::NONE);
    press(&mut state, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(state.row, 3);

    state.handle_mouse(mouse_at(MouseEventKind::ScrollDown, cx, cy));
    assert_eq!(state.group_index(), 1, "one notch = one group");
    assert_eq!(state.row, 0, "group switch resets the row (mirrors →)");
    assert_eq!(state.row_offset, 0, "and the window offset too");

    state.handle_mouse(mouse_at(MouseEventKind::ScrollDown, cx, cy));
    assert_eq!(state.group_index(), 2);
    // Wheel up twice lands on group 0 and CLAMPS there, no rem_euclid
    // wraparound to the last group.
    state.handle_mouse(mouse_at(MouseEventKind::ScrollUp, cx, cy));
    state.handle_mouse(mouse_at(MouseEventKind::ScrollUp, cx, cy));
    assert_eq!(state.group_index(), 0);
    state.handle_mouse(mouse_at(MouseEventKind::ScrollUp, cx, cy));
    assert_eq!(state.group_index(), 0, "top edge clamps, no wrap");
}

#[test]
fn click_selects_rows_and_groups_through_handle_mouse() {
    let mut state = state_at("mouseclick", "DEFAULT_MODEL=x\n");
    render_once_sized(&mut state, 110, 30);
    let geo = state.geometry.expect("rendered");

    // Click the 4th group row in the LEFT pane.
    state.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        geo.groups.x,
        geo.groups.y + 3,
    ));
    assert_eq!(state.group_index(), 3);

    // Click the 3rd visible row in the RIGHT pane.
    state.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        geo.keys_inner.x,
        geo.keys_inner.y + 2,
    ));
    assert_eq!(state.row, 2);

    // The Borders::LEFT divider column is neither pane → no-op.
    state.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        geo.keys_inner.x - 1,
        geo.keys_inner.y,
    ));
    assert_eq!(state.row, 2);

    // Outside both panes (the outer border) → no-op.
    state.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 0, 0));
    assert_eq!(state.row, 2);

    // One past the last row → ignored (out-of-range rule, not clamped).
    state.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        geo.keys_inner.x,
        geo.keys_inner.y + state.row_count() as u16,
    ));
    assert_eq!(state.row, 2);

    // A stale snapshot that cannot map to a row (the empty-group branch of
    // the out-of-range rule) → ignored too.
    state.row_offset = 1_000;
    if let Some(geo) = &mut state.geometry {
        geo.row_offset = 1_000;
    }
    state.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        geo.keys_inner.x,
        geo.keys_inner.y,
    ));
    assert_eq!(state.row, 2);
}

#[test]
fn mouse_events_are_noops_before_the_first_render() {
    let mut state = state_at("mousenogeom", "DEFAULT_MODEL=x\n");
    assert!(state.geometry.is_none());
    state.handle_mouse(mouse_at(MouseEventKind::ScrollDown, 10, 5));
    state.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 10, 5));
    assert_eq!(state.row, 0);
    assert_eq!(state.group_index(), 0);
}

#[test]
fn mouse_events_are_swallowed_while_the_edit_input_is_open() {
    let mut state = state_at("mouseedit", "DEFAULT_MODEL=x\n");
    select_row(&mut state, "DEFAULT_MODEL");
    render_once_sized(&mut state, 110, 30);
    let geo = state.geometry.expect("rendered");
    press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    assert!(state.edit.is_some(), "precondition: editor open");

    // Wheel and clicks anywhere (even on a group row) change nothing.
    state.handle_mouse(mouse_at(
        MouseEventKind::ScrollDown,
        geo.groups.x,
        geo.groups.y,
    ));
    state.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        geo.groups.x,
        geo.groups.y + 4,
    ));
    state.handle_mouse(mouse_at(
        MouseEventKind::ScrollUp,
        geo.keys_inner.x,
        geo.keys_inner.y,
    ));
    state.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        geo.keys_inner.x,
        geo.keys_inner.y,
    ));
    assert_eq!(
        state.group_index(),
        Group::ALL
            .iter()
            .position(|g| *g == Group::ModelRouting)
            .unwrap()
    );
    assert_eq!(
        state.row,
        state
            .rows()
            .iter()
            .position(|d| d.key == "DEFAULT_MODEL")
            .unwrap()
    );
    assert!(state.edit.is_some(), "editor stays open");
    assert!(!state.dirty(), "the click never staged anything");
    assert_eq!(
        state.doc.render(),
        SettingsDoc::parse("DEFAULT_MODEL=x\n").render(),
        "the document is untouched"
    );
}

#[test]
fn mouse_events_are_swallowed_while_each_confirm_is_open() {
    let variants = [
        Confirm::Discard,
        Confirm::Restart,
        Confirm::RestartBusy,
        Confirm::FileChanged,
    ];
    for variant in variants {
        let mut state = state_at("mouseconfirm", "DEFAULT_MODEL=x\n");
        render_once_sized(&mut state, 110, 30);
        let geo = state.geometry.expect("rendered");
        state.confirm = variant;
        state.handle_mouse(mouse_at(
            MouseEventKind::ScrollDown,
            geo.groups.x,
            geo.groups.y,
        ));
        state.handle_mouse(mouse_at(
            MouseEventKind::Up(MouseButton::Left),
            geo.groups.x,
            geo.groups.y + 4,
        ));
        state.handle_mouse(mouse_at(
            MouseEventKind::ScrollDown,
            geo.keys_inner.x,
            geo.keys_inner.y,
        ));
        state.handle_mouse(mouse_at(
            MouseEventKind::Up(MouseButton::Left),
            geo.keys_inner.x,
            geo.keys_inner.y,
        ));
        assert_eq!(state.confirm(), variant, "{variant:?}: modal stays open");
        assert_eq!(state.group_index(), 0, "{variant:?}: groups untouched");
        assert_eq!(state.row, 0, "{variant:?}: rows untouched");
        assert!(!state.dirty(), "{variant:?}: nothing staged");
    }
}

#[test]
fn handle_mouse_ignores_other_buttons_and_press_states() {
    let mut state = state_at("mouseignore", "DEFAULT_MODEL=x\n");
    render_once_sized(&mut state, 110, 30);
    let geo = state.geometry.expect("rendered");
    let cx = geo.keys_inner.x;
    let cy = geo.keys_inner.y;
    let kinds = [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Up(MouseButton::Right),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Moved,
    ];
    for kind in kinds {
        state.handle_mouse(mouse_at(kind, cx, cy));
    }
    assert_eq!(state.row, 0, "press/drag/motion never select");
    assert_eq!(state.group_index(), 0);
    assert!(!state.dirty());
}
