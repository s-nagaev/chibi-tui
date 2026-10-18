//! Backend-configuration screen: ONE state + render + key-handling component
//! consumed by TWO thin IO shells (plan.md v2 Architecture):
//!
//! * **Shell B (in-app)** — the [`crate::app::Mode::ConfigEditor`] modal over a
//!   running session. The component lives here; `App`/`Mode` only carry it.
//! * **Shell A (pre-loop)** — [`run`], a standalone mini-app structurally
//!   cloned from `setup_screen::run`: no backend process, no `App`, sync
//!   `std::fs` only. It never spawns the backend; the caller's setup loop
//!   respawns after a save.
//!
//! The component has NO `App` dependency: state/render in this module, never
//! forked into `App`/`Mode` handlers. Editing semantics ride on the
//! [`crate::backend_settings`] engine: uncomment-on-edit, comment-out on
//! clear, atomic write (temp+fsync+rename+.bak+chmod 600), mtime/len
//! external-change guard.
//!
//! # Secrets invariant
//!
//! Secret values render masked (`mask_secret`); the full value is visible
//! ONLY while its row is being edited AND only after an explicit `v` reveal,
//! which dies with the edit session (commit or cancel). No `Debug`/`Display`
//! impl here ever exposes a value, nothing is logged, and no copy path
//! exists. `ConfigState`'s manual `Debug` omits the document and the edit
//! buffer entirely.
//!
//! # Mouse seam
//!
//! `render` snapshots the rendered pane geometry into
//! [`ConfigState::geometry`] so mouse handlers (task 2) can map cursor
//! positions to group/row selections without duplicating layout math.
//!
//! # Path resolution
//!
//! `CHIBI_TUI_SETTINGS_PATH` overrides the settings file location (test/CI
//! seam, same pattern as `CHIBI_TUI_HOME` in `history.rs`); otherwise the
//! engine's canonical `~/chibi-bot/settings` (the backend hardcodes it — no
//! XDG).

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;

use crossterm::event::{
    Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::prelude::CrosstermBackend;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{quit_decision, QuitDecision};
use crate::backend_settings::{
    ensure_chibi_bot_dirs, load_settings, save_settings, settings_path, stat_settings,
    validate_doc, validate_value, write_generated_settings, FileMeta, Group, KeyDef, Kind,
    SettingsDoc, SettingsError, ValueState,
};
use crate::theme::Theme;

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

/// The canonical settings file path with the `CHIBI_TUI_SETTINGS_PATH`
/// override applied. `None` only when the home directory is undeterminable.
pub fn resolve_settings_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHIBI_TUI_SETTINGS_PATH") {
        return Some(PathBuf::from(path));
    }
    settings_path().ok()
}

/// True when at least one Group-1 provider key is actively set to a
/// non-empty value in the settings file. A missing/unreadable file is NOT
/// configured — the auto-open signal of both shells.
pub fn provider_key_configured() -> bool {
    let Some(path) = resolve_settings_path() else {
        return false;
    };
    match load_settings(&path) {
        Ok((doc, _)) => Group::ProviderKeys.keys().any(|def| {
            matches!(doc.get(def.key), ValueState::Active { value } if !value.trim().is_empty())
        }),
        Err(_) => false,
    }
}

/// Outcome of the Shell A pre-loop screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// The user saved and closed the screen: the caller respawns the backend.
    Saved,
    /// Cancel / quit (including a quit-confirmed exit): nothing more to do.
    Cancelled,
}

/// What the key handler asks the OWNING shell to do. Purely declarative —
/// the shells own the I/O (the save write in Shell B's dispatch, the restart
/// in the event loop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigAction {
    /// Key consumed, nothing to hand over.
    None,
    /// Validate + write the file. `close`: leave the screen after a
    /// successful save; `overwrite`: ignore the on-disk mtime guard (the
    /// user chose overwrite in the file-changed modal).
    Save { close: bool, overwrite: bool },
    /// Restart the backend now (event loop → `reconnect_requested`).
    RestartNow,
    /// Restart once the last in-flight request resolves (deferred flag).
    RestartWhenIdle,
    /// Close the screen (nothing to save).
    Closed,
}

/// Result of [`ConfigState::perform_save`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written atomically; the mtime guard advanced.
    Saved,
    /// Per-row validation failed; nothing was written.
    ValidationFailed,
    /// The file changed on disk since load; the reload/overwrite/cancel
    /// modal is open.
    ChangedOnDisk,
    /// I/O failure (permissions, vanished directory, …); message staged.
    Error(String),
}

/// Modal decision state of the screen (the shared quit-confirmation grammar
/// drives the two-option popups; the third options ride dedicated keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    None,
    /// Esc on a dirty screen: save / discard / stay.
    Discard,
    /// After a clean save: restart the backend now? [Y/n]
    Restart,
    /// Same, with a request in flight: restart now (in-flight lost) /
    /// when idle / stay.
    RestartBusy,
    /// The engine refused the write (mtime/len changed on disk):
    /// reload / overwrite / cancel.
    FileChanged,
}

/// Wheel step for the mouse seam (lib-side): rows advanced per wheel tick.
/// The bin-side `mouse.rs` keeps its own constant.
pub const WHEEL_STEP: u16 = 3;

/// Render-fed geometry of the config screen's two panes, snapshotted by
/// [`render`] AFTER the row-window normalization so mouse handlers can map
/// cursor positions onto group/row selections (see the module's
/// "Mouse seam" section). Fields are private: query via
/// [`ConfigState::pane_at`] / [`ConfigState::select_row_at`] /
/// [`ConfigState::select_group_at`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigGeometry {
    /// The LEFT groups list rect as rendered (no scroll offset there).
    groups: Rect,
    /// The INNER right-pane rect: the `Borders::LEFT` divider column is a
    /// border, not a row.
    keys_inner: Rect,
    /// First visible row of the right pane's window at snapshot time.
    row_offset: usize,
    /// Visible row count (`keys_inner.height`, normalized to >= 1).
    visible: usize,
}

/// Which pane a mouse point landed in ([`ConfigState::pane_at`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigPane {
    /// The left groups list.
    Groups,
    /// The right key rows list (inner area, divider excluded).
    Rows,
}

/// Inclusive-exclusive point-in-rect test (`Rect` may be empty).
fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// The open single-line value editor (the `Renaming` pattern, scoped to one
/// row). The `Debug` impl is manual and redacts `buf` — it may hold a secret
/// between keystrokes.
struct EditState {
    buf: String,
    /// `v` toggle: show the secret as typed. Dies with the edit session.
    reveal: bool,
    /// Inline commit-rejection message (validation, `${VAR}`, …).
    error: Option<String>,
}

impl Clone for EditState {
    fn clone(&self) -> Self {
        Self {
            buf: self.buf.clone(),
            reveal: self.reveal,
            error: self.error.clone(),
        }
    }
}

impl fmt::Debug for EditState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditState")
            .field("buf_len", &self.buf.chars().count())
            .field("reveal", &self.reveal)
            .field("error", &self.error)
            .finish()
    }
}

/// State of the open configuration screen (both shells). Owns the parsed
/// document, the cursor positions, the staged-edit flag and every modal.
pub struct ConfigState {
    path: PathBuf,
    doc: SettingsDoc,
    /// External-change guard captured on load; `None` while the file does
    /// not exist yet (the first save creates it).
    guard: Option<FileMeta>,
    /// The file was absent at load: the first save creates it (dirs ensured).
    file_missing: bool,
    /// The file exists but could not be read/parsed: the screen degrades to
    /// read-only-with-error instead of risking a destructive rewrite.
    load_error: Option<String>,
    /// Index into [`Group::ALL`] (left pane).
    group: usize,
    /// Selected row within the current group (right pane).
    row: usize,
    /// First visible row of the right pane's window.
    row_offset: usize,
    /// Render-fed visible row count (same seam as `picker_visible_rows`).
    rows_visible: usize,
    /// Render-fed pane geometry for the mouse seam (the "Mouse seam" module
    /// section): `None` until the first config render — mouse events that
    /// precede it are no-ops. Excluded from `PartialEq` (like
    /// `rows_visible`): pure render output, not user-visible state.
    pub geometry: Option<ConfigGeometry>,
    edit: Option<EditState>,
    /// Staged (in-memory) edits not yet written.
    dirty: bool,
    confirm: Confirm,
    /// Per-row validation failures from the last save attempt (key → message).
    errors: HashMap<&'static str, String>,
    /// Last save/reload I/O failure (rendered inline, never logged).
    save_error: Option<String>,
    /// Transient status note (saved / reloaded / file-missing hint).
    note: Option<String>,
    /// Shell B: whether a restart offer makes sense (live session). Shell A
    /// and mock sessions run with `false` — no restart prompt after a save.
    restart_available: bool,
}

impl Clone for ConfigState {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            doc: self.doc.clone(),
            guard: self.guard,
            file_missing: self.file_missing,
            load_error: self.load_error.clone(),
            group: self.group,
            row: self.row,
            row_offset: self.row_offset,
            rows_visible: self.rows_visible,
            geometry: self.geometry,
            edit: self.edit.clone(),
            dirty: self.dirty,
            confirm: self.confirm,
            errors: self.errors.clone(),
            save_error: self.save_error.clone(),
            note: self.note.clone(),
            restart_available: self.restart_available,
        }
    }
}

impl fmt::Debug for ConfigState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted on purpose: the document and the edit buffer hold secret
        // values (secrets invariant) — never rendered through Debug.
        f.debug_struct("ConfigState")
            .field("path", &self.path)
            .field("group", &self.group)
            .field("row", &self.row)
            .field("dirty", &self.dirty)
            .field("edit_open", &self.edit.is_some())
            .field("geometry", &self.geometry)
            .field("confirm", &self.confirm)
            .field("errors", &self.errors.len())
            .field("restart_available", &self.restart_available)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ConfigState {
    fn eq(&self, other: &Self) -> bool {
        // Document equality via its rendered bytes (cheap enough for the
        // derive-driven Mode comparisons this exists for).
        self.path == other.path
            && self.doc.render() == other.doc.render()
            && self.guard == other.guard
            && self.group == other.group
            && self.row == other.row
            && self.dirty == other.dirty
            && self.confirm == other.confirm
            && self.restart_available == other.restart_available
    }
}
impl Eq for ConfigState {}

impl ConfigState {
    /// Load the resolved settings file (Shell B's F2 / auto-open path). A
    /// missing file is NOT generated here: the screen starts empty and the
    /// first save creates it — a mere keypress in the app must never write
    /// the user's disk.
    pub fn load(restart_available: bool) -> ConfigState {
        match resolve_settings_path() {
            Some(path) => Self::load_at(path, restart_available),
            // No home dir: degrade to a relative path; saves will fail with
            // an inline error instead of panicking.
            None => Self::load_at(PathBuf::from("settings"), restart_available),
        }
    }

    /// Load an explicit path (Shell A + tests).
    pub fn load_at(path: PathBuf, restart_available: bool) -> ConfigState {
        let mut state = Self {
            path,
            doc: SettingsDoc::parse(""),
            guard: None,
            file_missing: false,
            load_error: None,
            group: 0,
            row: 0,
            row_offset: 0,
            rows_visible: 10,
            geometry: None,
            edit: None,
            dirty: false,
            confirm: Confirm::None,
            errors: HashMap::new(),
            save_error: None,
            note: None,
            restart_available,
        };
        match stat_settings(&state.path) {
            Ok(None) => {
                state.file_missing = true;
                state.note =
                    Some("settings file not found — it will be created on save".to_owned());
            }
            Ok(Some(meta)) => match load_settings(&state.path) {
                Ok((doc, _)) => {
                    state.doc = doc;
                    state.guard = Some(meta);
                }
                Err(e) => state.load_error = Some(e.to_string()),
            },
            Err(e) => state.load_error = Some(e.to_string()),
        }
        state
    }

    /// The file was absent at load time.
    pub fn file_missing(&self) -> bool {
        self.file_missing
    }

    /// Staged (unwritten) edits exist.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Current modal decision state.
    pub fn confirm(&self) -> Confirm {
        self.confirm
    }

    fn rows(&self) -> Vec<&'static KeyDef> {
        Group::ALL[self.group].keys().collect()
    }

    fn selected_def(&self) -> Option<&'static KeyDef> {
        self.rows().get(self.row).copied()
    }

    fn switch_group(&mut self, delta: isize) {
        let n = Group::ALL.len() as isize;
        self.group = (self.group as isize + delta).rem_euclid(n) as usize;
        self.row = 0;
        self.row_offset = 0;
    }

    fn row_count(&self) -> usize {
        self.rows().len()
    }

    fn row_up(&mut self, amount: usize) {
        self.row = self.row.saturating_sub(amount);
        self.clamp_offset();
    }

    fn row_down(&mut self, amount: usize) {
        self.row = (self.row + amount).min(self.row_count().saturating_sub(1));
        self.clamp_offset();
    }

    /// Keep the selected row inside the visible window (render-fed
    /// [`ConfigState::rows_visible`]).
    fn clamp_offset(&mut self) {
        let visible = self.rows_visible.max(1);
        if self.row < self.row_offset {
            self.row_offset = self.row;
        } else if self.row >= self.row_offset + visible {
            self.row_offset = self.row + 1 - visible;
        }
    }

    /// Loader-visible value of a row (the decoded value of the active line,
    /// the carried value of a commented one, or nothing).
    fn current_value(&self, def: &KeyDef) -> Option<String> {
        match self.doc.get(def.key) {
            ValueState::Active { value } => Some(value),
            ValueState::Commented { value } => value,
            ValueState::Absent => None,
        }
    }

    /// Text rendered in the value column of the list (masking included).
    fn display_value(&self, def: &KeyDef) -> String {
        match self.current_value(def) {
            None => "\u{2014} (not set)".to_owned(),
            Some(v) if v.is_empty() => "\u{2014} (empty)".to_owned(),
            Some(v) => match def.kind {
                Kind::Secret => mask_secret(&v),
                Kind::Bool => format!("[{}]", v.trim().to_ascii_lowercase()),
                _ => v,
            },
        }
    }

    /// Space/Enter on a Bool row: flip between the two canonical literals.
    fn toggle_bool(&mut self, def: &KeyDef) {
        let next = match self.current_value(def) {
            Some(v) if is_bool_true(&v) => "false",
            _ => "true",
        };
        self.doc.set(def, next);
        self.dirty = true;
        self.errors.remove(def.key);
    }

    fn open_edit(&mut self) {
        let Some(def) = self.selected_def() else {
            return;
        };
        let buf = self.current_value(def).unwrap_or_default();
        self.edit = Some(EditState {
            buf,
            reveal: false,
            error: None,
        });
    }

    /// Enter in the editor: validate + stage into the in-memory document.
    /// A `${VAR}`-bearing value is rejected inline (v1 scope); an emptied
    /// secret clears the key (comment-out, the engine's clearing semantic).
    fn commit_edit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        let Some(def) = self.selected_def() else {
            return;
        };
        let value = edit.buf.trim().to_string();
        if value.contains("${") {
            self.edit = Some(EditState {
                buf: edit.buf,
                reveal: edit.reveal,
                error: Some(
                    "${VAR} interpolation is not supported here — paste the literal value"
                        .to_owned(),
                ),
            });
            return;
        }
        if value.is_empty() && def.secret {
            // Clearing = comment the line out (never `KEY=`, which would be
            // *set* to the empty string).
            self.doc.clear(def.key);
            self.dirty = true;
            self.errors.remove(def.key);
            return;
        }
        match validate_value(&def.kind, &value) {
            Ok(()) => {
                self.doc.set(def, &value);
                self.dirty = true;
                self.errors.remove(def.key);
            }
            Err(msg) => {
                self.edit = Some(EditState {
                    buf: value,
                    reveal: edit.reveal,
                    error: Some(msg),
                });
            }
        }
    }

    /// Esc/F2 on a clean screen closes; on a dirty screen the save/discard/
    /// stay confirm opens first.
    fn request_close(&mut self) -> ConfigAction {
        if self.dirty {
            self.confirm = Confirm::Discard;
            ConfigAction::None
        } else {
            ConfigAction::Closed
        }
    }

    /// Drop staged edits: reload the on-disk bytes (or the empty model for a
    /// never-yet-written file).
    fn reload_from_disk(&mut self) {
        if self.file_missing {
            self.doc = SettingsDoc::parse("");
            self.guard = None;
            self.dirty = false;
            self.errors.clear();
            self.edit = None;
            return;
        }
        match load_settings(&self.path) {
            Ok((doc, meta)) => {
                self.doc = doc;
                self.guard = Some(meta);
                self.dirty = false;
                self.errors.clear();
                self.edit = None;
                self.file_missing = false;
                self.note = Some("Reloaded from disk".to_owned());
            }
            Err(e) => self.save_error = Some(e.to_string()),
        }
    }

    fn handle_discard_confirm(&mut self, key: KeyEvent, ctrl: bool) -> ConfigAction {
        // `d` (discard & close) and `s` (save & close) ride outside the shared
        // two-decision grammar — the modal hint and HOTKEY_ROWS advertise
        // `s/Enter save · d discard · Esc/n stay`.
        if !ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) {
            self.reload_from_disk();
            self.note = None;
            return ConfigAction::Closed;
        }
        if !ctrl && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) {
            self.confirm = Confirm::None;
            return ConfigAction::Save {
                close: true,
                overwrite: false,
            };
        }
        match quit_decision(key) {
            QuitDecision::Confirm => {
                self.confirm = Confirm::None;
                ConfigAction::Save {
                    close: true,
                    overwrite: false,
                }
            }
            QuitDecision::Dismiss => {
                self.confirm = Confirm::None;
                ConfigAction::None
            }
            QuitDecision::Swallow => ConfigAction::None,
        }
    }

    fn handle_restart_confirm(&mut self, key: KeyEvent, ctrl: bool) -> ConfigAction {
        let busy_variant = self.confirm == Confirm::RestartBusy;
        // `d` (restart when idle) rides outside the shared grammar.
        if busy_variant && !ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) {
            self.confirm = Confirm::None;
            return ConfigAction::RestartWhenIdle;
        }
        match quit_decision(key) {
            QuitDecision::Confirm => {
                self.confirm = Confirm::None;
                ConfigAction::RestartNow
            }
            QuitDecision::Dismiss => {
                self.confirm = Confirm::None;
                ConfigAction::None
            }
            QuitDecision::Swallow => ConfigAction::None,
        }
    }

    fn handle_file_changed_confirm(&mut self, key: KeyEvent) -> ConfigAction {
        match key.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.confirm = Confirm::None;
                self.reload_from_disk();
                ConfigAction::None
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                self.confirm = Confirm::None;
                ConfigAction::Save {
                    close: false,
                    overwrite: true,
                }
            }
            KeyCode::Esc => {
                self.confirm = Confirm::None;
                ConfigAction::None
            }
            _ => ConfigAction::None,
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool) {
        let secret = self.selected_def().is_some_and(|def| def.secret);
        match key.code {
            KeyCode::Enter => self.commit_edit(),
            KeyCode::Esc => self.edit = None,
            KeyCode::Backspace if !ctrl && !alt => {
                if let Some(edit) = &mut self.edit {
                    edit.buf.pop();
                }
            }
            KeyCode::Char('v') if !ctrl && !alt && secret => {
                if let Some(edit) = &mut self.edit {
                    edit.reveal = !edit.reveal;
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Some(edit) = &mut self.edit {
                    edit.buf.push(c);
                    edit.error = None;
                }
            }
            _ => {}
        }
    }

    /// Route one key press. Pure state mutation: the returned
    /// [`ConfigAction`] tells the owning shell what I/O to perform.
    pub fn handle_key(&mut self, key: KeyEvent) -> ConfigAction {
        if key.kind == KeyEventKind::Release {
            return ConfigAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match self.confirm {
            Confirm::None => {}
            Confirm::Discard => return self.handle_discard_confirm(key, ctrl),
            Confirm::Restart | Confirm::RestartBusy => {
                return self.handle_restart_confirm(key, ctrl)
            }
            Confirm::FileChanged => return self.handle_file_changed_confirm(key),
        }
        if self.edit.is_some() {
            self.handle_edit_key(key, ctrl, alt);
            return ConfigAction::None;
        }
        match key.code {
            KeyCode::F(2) | KeyCode::Esc => self.request_close(),
            KeyCode::Up | KeyCode::Char('k') if !ctrl && !alt => {
                self.row_up(1);
                ConfigAction::None
            }
            KeyCode::Down | KeyCode::Char('j') if !ctrl && !alt => {
                self.row_down(1);
                ConfigAction::None
            }
            KeyCode::PageUp => {
                self.row_up(self.rows_visible.max(1));
                ConfigAction::None
            }
            KeyCode::PageDown => {
                self.row_down(self.rows_visible.max(1));
                ConfigAction::None
            }
            KeyCode::Char('g') if !ctrl && !alt => {
                self.row = 0;
                self.clamp_offset();
                ConfigAction::None
            }
            KeyCode::Char('G') if !ctrl && !alt => {
                self.row = self.row_count().saturating_sub(1);
                self.clamp_offset();
                ConfigAction::None
            }
            KeyCode::Left | KeyCode::BackTab if !ctrl => {
                self.switch_group(-1);
                ConfigAction::None
            }
            KeyCode::Right | KeyCode::Tab if !ctrl => {
                self.switch_group(1);
                ConfigAction::None
            }
            KeyCode::Enter if !ctrl => {
                if self
                    .selected_def()
                    .is_some_and(|def| def.kind == Kind::Bool)
                {
                    if let Some(def) = self.selected_def() {
                        self.toggle_bool(def);
                    }
                } else {
                    self.open_edit();
                }
                ConfigAction::None
            }
            KeyCode::Char(' ') if !ctrl && !alt => {
                if let Some(def) = self.selected_def() {
                    if def.kind == Kind::Bool {
                        self.toggle_bool(def);
                    }
                }
                ConfigAction::None
            }
            KeyCode::Char('s') | KeyCode::Char('S') if !ctrl && !alt => ConfigAction::Save {
                close: false,
                overwrite: false,
            },
            _ => ConfigAction::None,
        }
    }

    /// Validate every registry key currently set in the document, then write
    /// atomically (the engine's temp+fsync+rename+.bak+chmod 600 pipeline
    /// with the mtime/len external-change guard). Validation failures stage
    /// per-row inline errors and write nothing.
    pub fn perform_save(&mut self, overwrite: bool) -> SaveOutcome {
        if let Some(err) = &self.load_error {
            return SaveOutcome::Error(err.clone());
        }
        let failures = validate_doc(&self.doc);
        if !failures.is_empty() {
            self.errors = failures.into_iter().map(|e| (e.key, e.message)).collect();
            let n = self.errors.len();
            self.note = Some(format!("{n} invalid value(s) — fix them before saving"));
            return SaveOutcome::ValidationFailed;
        }
        self.errors.clear();
        // The first save may create the tree the backend generator owns
        // (~/chibi-bot/{data,skills,home}).
        if let Some(base) = self.path.parent() {
            let _ = ensure_chibi_bot_dirs(base);
        }
        let guard = if overwrite { None } else { self.guard };
        match save_settings(&self.doc, &self.path, guard) {
            Ok(meta) => {
                self.guard = Some(meta);
                self.file_missing = false;
                self.dirty = false;
                self.save_error = None;
                self.note = Some("Saved".to_owned());
                SaveOutcome::Saved
            }
            Err(SettingsError::ChangedOnDisk) => {
                self.confirm = Confirm::FileChanged;
                SaveOutcome::ChangedOnDisk
            }
            Err(e) => {
                let msg = e.to_string();
                self.save_error = Some(msg.clone());
                SaveOutcome::Error(msg)
            }
        }
    }

    /// Post-save modal staging (Shell B only): with a restart available, ask
    /// about the backend restart — the plain variant when idle, the
    /// in-flight-warning variant when a request is running. Called by the
    /// dispatch after a successful [`ConfigState::perform_save`].
    pub fn after_save(&mut self, any_busy: bool) {
        if !self.restart_available {
            return; // mock session / Shell A: the "Saved" note is enough
        }
        if !matches!(self.confirm, Confirm::None) {
            return; // e.g. the file-changed modal takes precedence
        }
        self.note = None;
        self.confirm = if any_busy {
            Confirm::RestartBusy
        } else {
            Confirm::Restart
        };
    }

    /// Group the cursor currently sits on.
    pub fn group_index(&self) -> usize {
        self.group
    }

    /// Hit-test a cursor position against the stored pane geometry.
    /// `None` when no render has populated the geometry yet or the point is
    /// in neither rect (the divider column and borders included).
    pub fn pane_at(&self, col: u16, row: u16) -> Option<ConfigPane> {
        let geo = self.geometry?;
        if rect_contains(geo.groups, col, row) {
            Some(ConfigPane::Groups)
        } else if rect_contains(geo.keys_inner, col, row) {
            Some(ConfigPane::Rows)
        } else {
            None
        }
    }

    /// Mouse click on the right pane: map `y` (inside the stored
    /// `keys_inner` rect) to row `row_offset + (y - keys_inner.y)`. Ignored
    /// (no state change) when the geometry is absent, `y` is above the pane,
    /// the mapped index is past the current group's last row (an empty
    /// group has none), or — otherwise — the row is selected and the stored
    /// `row_offset` snapshot is refreshed via `clamp_offset`
    /// so the seam stays consistent between renders.
    pub fn select_row_at(&mut self, y: u16) {
        let Some(geo) = self.geometry else {
            return;
        };
        if y < geo.keys_inner.y {
            return;
        }
        let idx = self.row_offset + (y - geo.keys_inner.y) as usize;
        if idx >= self.row_count() {
            return;
        }
        self.row = idx;
        self.clamp_offset();
        self.refresh_geometry_offset();
    }

    /// Mouse click on the left pane: map `y` (inside the stored `groups`
    /// rect) to group index `y - groups.y` (no scroll offset there). An
    /// out-of-range index is ignored ENTIRELY — `switch_group`
    /// is delta-based with `rem_euclid` wrapping, so an unchecked delta would
    /// land on an unrelated group. In range, the click routes through
    /// `switch_group` (its row/row_offset reset is intended) and refreshes
    /// the stored snapshot if `clamp_offset` ran.
    pub fn select_group_at(&mut self, y: u16) {
        let Some(geo) = self.geometry else {
            return;
        };
        if y < geo.groups.y {
            return;
        }
        let idx = (y - geo.groups.y) as usize;
        if idx >= Group::ALL.len() {
            return;
        }
        self.switch_group(idx as isize - self.group as isize);
        self.refresh_geometry_offset();
    }

    /// Route one mouse event (the ONE entry point both shells share —
    /// Shell B's `mouse` arm and Shell A's `run` loop both
    /// delegate here). Pure state mutation, like [`ConfigState::handle_key`].
    ///
    /// Routing:
    /// - a screen-owned modal (the value-edit input or any
    ///   [`Confirm`] variant) swallows EVERY mouse event, mirroring how
    ///   modal surfaces consume the wheel/clicks everywhere else;
    /// - WHEEL is routed by the pane under the cursor: over the rows pane
    ///   the SELECTION moves by [`WHEEL_STEP`] through `row_up`/`row_down`
    ///   (moving only `row_offset` would let the highlight scroll out of
    ///   the visible window), over the groups pane the group selection
    ///   moves one step per notch through the same guarded path as
    ///   `select_group_at` (range-checked — no `rem_euclid` wraparound;
    ///   `switch_group`'s row/row_offset reset is intended, it mirrors
    ///   ←/→); outside both panes: no-op. The stored geometry snapshot is
    ///   refreshed after every move so hit-tests stay consistent between
    ///   renders;
    /// - LEFT-BUTTON-UP clicks only SELECT (`select_row_at` /
    ///   `select_group_at`) — no toggle-on-click, no editing, no secret
    ///   reveal (keyboard parity);
    /// - other buttons and press/drag/motion states are ignored (no
    ///   drag-selection in the config screen).
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.edit.is_some() || self.confirm != Confirm::None {
            return; // a modal owns the screen
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = mouse.kind == MouseEventKind::ScrollDown;
                match self.pane_at(mouse.column, mouse.row) {
                    Some(ConfigPane::Rows) => {
                        if down {
                            self.row_down(usize::from(WHEEL_STEP));
                        } else {
                            self.row_up(usize::from(WHEEL_STEP));
                        }
                        self.refresh_geometry_offset();
                    }
                    Some(ConfigPane::Groups) => {
                        let target = self.group as isize + if down { 1 } else { -1 };
                        if target >= 0 && (target as usize) < Group::ALL.len() {
                            self.switch_group(if down { 1 } else { -1 });
                            self.refresh_geometry_offset();
                        }
                    }
                    None => {}
                }
            }
            MouseEventKind::Up(MouseButton::Left) => match self.pane_at(mouse.column, mouse.row) {
                Some(ConfigPane::Rows) => self.select_row_at(mouse.row),
                Some(ConfigPane::Groups) => self.select_group_at(mouse.row),
                None => {}
            },
            _ => {}
        }
    }

    /// Keep the stored `row_offset` snapshot in sync after a mutation that
    /// ran [`ConfigState::clamp_offset`] (no-op while geometry is `None`).
    fn refresh_geometry_offset(&mut self) {
        if let Some(geo) = &mut self.geometry {
            geo.row_offset = self.row_offset;
        }
    }
}

/// Canonical truthy literal test for Bool rows (mirrors the backend's
/// pydantic-style truthy set the engine validates).
fn is_bool_true(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
}

/// Mask a secret for the LIST view: `••••…last4` when long enough, a fixed
/// bullet run otherwise. (`— (not set)` / `— (empty)` are handled by the
/// caller — they are state, not masking.)
pub fn mask_secret(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() >= 8 {
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("\u{2022}\u{2022}\u{2022}\u{2022}\u{2026}{tail}")
    } else {
        "\u{2022}\u{2022}\u{2022}\u{2022}".to_owned()
    }
}

/// True when `CHIBI_ENV` equals `dev` (case-insensitive). This mirrors the
/// backend's exact check (`chibi/config/app.py`: `os.getenv("CHIBI_ENV",
/// "no-dev").lower() != "dev"` skips the settings file) — any other value
/// leaves the file live, so no warning is shown.
pub fn is_dev_mode() -> bool {
    std::env::var("CHIBI_ENV")
        .map(|v| v.eq_ignore_ascii_case("dev"))
        .unwrap_or(false)
}

/// Render the screen (full-frame overlay for Shell B, whole terminal for
/// Shell A). Purely visual — all key handling lives in
/// [`ConfigState::handle_key`].
pub fn render(f: &mut Frame, state: &mut ConfigState, theme: &Theme, area: Rect) {
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(Style::new().bg(theme.bg)), area);

    let dev_mode = is_dev_mode();
    let mut title = " Backend configuration ".to_owned();
    if state.dirty {
        title.push_str("\u{00b7} \u{25cf} unsaved ");
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.blue))
        .style(Style::new().bg(theme.bg))
        .title(Span::styled(
            title,
            Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Optional dev banner row (CHIBI_ENV=dev makes the file inert backend-
    // side: saving would silently do nothing until the env var is gone).
    let banner_h: u16 = if dev_mode { 1 } else { 0 };
    let rows = Layout::vertical([
        Constraint::Length(banner_h),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    if dev_mode && rows[0].height > 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " \u{26a0} CHIBI_ENV=dev — the backend ignores this file in dev mode",
                Style::new().fg(theme.yellow),
            ))),
            rows[0],
        );
    }

    let main = Layout::horizontal([Constraint::Length(26), Constraint::Min(0)]).split(rows[1]);

    // ---- left pane: groups ----
    let mut group_lines: Vec<Line> = Vec::new();
    for (i, group) in Group::ALL.iter().enumerate() {
        let selected = i == state.group;
        let count = group.keys().count();
        let emphasis = if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        };
        let line = Line::from(vec![
            Span::styled(
                if selected { "\u{25b8} " } else { "  " },
                Style::new().fg(theme.yellow),
            ),
            Span::styled(
                group.title().to_owned(),
                Style::new()
                    .fg(if selected { theme.fg } else { theme.dim })
                    .add_modifier(emphasis),
            ),
            Span::styled(format!(" ({count})"), Style::new().fg(theme.dim)),
        ])
        .style(if selected {
            Style::new().bg(theme.selection)
        } else {
            Style::new()
        });
        group_lines.push(line);
    }
    f.render_widget(Paragraph::new(group_lines), main[0]);

    // ---- right pane: key rows (windowed, left border = divider) ----
    let keys_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::new().fg(theme.selection))
        .style(Style::new().bg(theme.bg));
    let keys_inner = keys_block.inner(main[1]);
    f.render_widget(keys_block, main[1]);

    let defs = state.rows();
    state.rows_visible = keys_inner.height.max(1) as usize;
    state.clamp_offset();
    if !defs.is_empty() {
        state.row = state.row.min(defs.len() - 1);
    }
    // Mouse seam: snapshot the normalized geometry AFTER the row-window
    // normalization above so hit-tests agree with what is on screen.
    state.geometry = Some(ConfigGeometry {
        groups: main[0],
        keys_inner,
        row_offset: state.row_offset,
        visible: state.rows_visible,
    });
    let key_w = defs
        .iter()
        .map(|def| def.key.width())
        .max()
        .unwrap_or(8)
        .min(28);
    let visible = state.rows_visible;
    let first = state.row_offset.min(defs.len());
    let last = (first + visible).min(defs.len());
    let mut row_lines: Vec<Line> = Vec::new();
    for (i, def) in defs[first..last].iter().enumerate() {
        let i = first + i;
        let selected = i == state.row;
        let (commented, absent) = match state.doc.get(def.key) {
            ValueState::Active { .. } => (false, false),
            ValueState::Commented { .. } => (true, false),
            ValueState::Absent => (false, true),
        };
        let value_text = state.display_value(def);
        let placeholder = value_text.starts_with('\u{2014}');
        let value_style = match def.kind {
            Kind::Bool
                if !placeholder && state.current_value(def).is_some_and(|v| is_bool_true(&v)) =>
            {
                Style::new().fg(theme.green)
            }
            _ if placeholder => Style::new().fg(theme.dim),
            Kind::Secret => Style::new().fg(theme.purple),
            _ => Style::new().fg(theme.fg),
        };
        let mut spans = vec![
            Span::styled(
                if selected { "\u{25b8} " } else { "  " },
                Style::new().fg(theme.yellow),
            ),
            Span::styled(
                format!("{:<key_w$}", def.key),
                Style::new().fg(if placeholder { theme.dim } else { theme.cyan }),
            ),
            Span::raw("  "),
            Span::styled(value_text, value_style),
        ];
        if commented {
            spans.push(Span::styled(" [commented]", Style::new().fg(theme.yellow)));
        }
        if absent {
            spans.push(Span::styled(" [default]", Style::new().fg(theme.dim)));
        }
        if state.errors.contains_key(def.key) {
            spans.push(Span::styled(" \u{2717}", Style::new().fg(theme.red)));
        }
        let line = Line::from(spans).style(if selected {
            Style::new().bg(theme.selection)
        } else {
            Style::new()
        });
        row_lines.push(line);
    }
    if keys_inner.height > 0 {
        f.render_widget(Paragraph::new(row_lines), keys_inner);
    }

    // ---- footer: description (or the edit line), then hints ----
    render_footer(f, state, theme, rows[2], rows[3]);

    // ---- confirm modals (rendered last, topmost) ----
    if state.confirm != Confirm::None {
        render_confirm(f, state, theme, area);
    }
}

/// The two footer rows: while editing, the first row becomes the single-line
/// value editor (masked until `v`); otherwise it shows the selected key's
/// DESCRIPTION only — never a value. The second row carries the key hints and
/// the transient note / error.
fn render_footer(f: &mut Frame, state: &mut ConfigState, theme: &Theme, desc: Rect, hints: Rect) {
    let width = desc.width.max(hints.width) as usize;
    if let (true, Some(edit)) = (desc.height > 0, &state.edit) {
        // ---- edit line ----
        let Some(def) = state.selected_def() else {
            return;
        };
        let (shown, suffix) = if def.secret && !edit.reveal {
            (
                "\u{2022}".repeat(edit.buf.chars().count()),
                "  (v to reveal)",
            )
        } else {
            (edit.buf.clone(), "")
        };
        let mut spans = vec![
            Span::styled("\u{276f} ", Style::new().fg(theme.cyan)),
            Span::styled(
                format!("{} = ", def.key),
                Style::new().fg(theme.cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(shown, Style::new().fg(theme.fg)),
            Span::styled("\u{258f}", Style::new().fg(theme.fg)),
            Span::styled(suffix.to_owned(), Style::new().fg(theme.dim)),
        ];
        if let Some(err) = &edit.error {
            spans.push(Span::styled(
                format!("  \u{2717} {err}"),
                Style::new().fg(theme.red),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), desc);
        if hints.height > 0 {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Enter stage \u{00b7} Esc cancel \u{00b7} v reveal",
                    Style::new().fg(theme.yellow),
                ))),
                hints,
            );
        }
        return;
    }
    if desc.height > 0 {
        // ---- description row: key + description (+ inline row error) ----
        let line = match state.selected_def() {
            Some(def) => {
                if let Some(err) = state.errors.get(def.key) {
                    Line::from(Span::styled(
                        truncate_str(&format!("{}: {err}", def.key), width),
                        Style::new().fg(theme.red),
                    ))
                } else {
                    Line::from(Span::styled(
                        format!("{} \u{2014} {}", def.key, def.description),
                        Style::new().fg(theme.dim),
                    ))
                }
            }
            None => Line::from(Span::styled(
                "no keys in this group",
                Style::new().fg(theme.dim),
            )),
        };
        f.render_widget(Paragraph::new(line), desc);
    }
    if hints.height > 0 {
        // ---- hints + transient note / error ----
        let hints_text =
            "\u{2191}\u{2193}/jk rows \u{00b7} \u{2190}\u{2192} groups \u{00b7} Enter edit \u{00b7} s save \u{00b7} Esc/F2 close";
        let mut spans = vec![Span::styled(
            hints_text.to_owned(),
            Style::new().fg(theme.yellow),
        )];
        let note = state.save_error.clone().or_else(|| state.note.clone());
        if let Some(note) = note {
            let style = if state.save_error.is_some() {
                Style::new().fg(theme.red)
            } else {
                Style::new().fg(theme.green)
            };
            let used = hints_text.width();
            let text = truncate_str(&note, width.saturating_sub(used + 2).max(8));
            spans.push(Span::raw("  \u{00b7}  "));
            spans.push(Span::styled(text, style));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), hints);
    }
}

/// Truncate to `max` display columns with a trailing ellipsis.
fn truncate_str(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > max.saturating_sub(1) {
            out.push('\u{2026}');
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// The centered decision modal (save/discard/stay, restart variants,
/// file-changed). Two-option rows ride the shared quit-confirmation grammar;
/// the third options (`d` / `r` / `o`) ride dedicated keys printed in hints.
fn render_confirm(f: &mut Frame, state: &ConfigState, theme: &Theme, area: Rect) {
    let (title, message, hint, color) = match state.confirm {
        Confirm::Discard => (
            " Unsaved changes ",
            "Save changes before closing?",
            "s/Enter save \u{00b7} d discard \u{00b7} Esc/n stay",
            theme.yellow,
        ),
        Confirm::Restart => (
            " Configuration saved ",
            "Restart the backend now?",
            "y/Enter restart \u{00b7} Esc/n stay",
            theme.green,
        ),
        Confirm::RestartBusy => (
            " Configuration saved ",
            "Restart now? In-flight requests will be lost.",
            "y/Enter restart now \u{00b7} d when idle \u{00b7} Esc/n stay",
            theme.yellow,
        ),
        Confirm::FileChanged => (
            " File changed on disk ",
            "The settings file changed while the editor was open.",
            "r reload (drops edits) \u{00b7} o overwrite \u{00b7} Esc cancel",
            theme.orange,
        ),
        Confirm::None => return,
    };
    let max_w = area.width.saturating_sub(4).max(20);
    let width = (message.width().max(hint.width()) as u16 + 4)
        .max(30)
        .clamp(20, max_w);
    let height = 4.min(area.height.saturating_sub(2)).max(3);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title,
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height > 0 {
        let mut lines = vec![Line::from(Span::styled(message, Style::new().fg(theme.fg)))];
        if inner.height > 1 {
            lines.push(Line::from(Span::styled(
                hint,
                Style::new().fg(theme.yellow),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }
}

/// Shell A: the pre-loop configuration screen (structural clone of
/// `setup_screen::run`). No `App`, no event channel, no backend process —
/// sync file IO only; the save path is the engine's atomic pipeline. A
/// missing file is generated from the vendored template first (this shell IS
/// the onboarding step after a backend install).
///
/// `q`/`Ctrl+C` ask before quitting (the shared quit-confirmation popup);
/// Esc closes via the component (`Flow::Cancelled`, or the save/discard/
/// stay confirm when dirty). A successful save-and-close returns
/// [`Flow::Saved`]; the caller respawns the backend either way — NO restart
/// is attempted here.
pub async fn run(terminal: &mut Tui, reader: &mut EventStream, theme: &Theme) -> io::Result<Flow> {
    let mut state = match resolve_settings_path() {
        Some(path) => {
            if stat_settings(&path).ok().flatten().is_none() {
                let _ = write_generated_settings(&path);
            }
            ConfigState::load_at(path, false)
        }
        None => ConfigState::load(false),
    };
    let mut quit_confirm = false;
    loop {
        terminal.draw(|f| render(f, &mut state, theme, f.area()))?;
        let maybe_event = reader.next().await;
        match maybe_event {
            Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if quit_confirm {
                    match quit_decision(key) {
                        QuitDecision::Confirm => return Ok(Flow::Cancelled),
                        QuitDecision::Dismiss => quit_confirm = false,
                        QuitDecision::Swallow => {}
                    }
                    continue;
                }
                if (ctrl && matches!(key.code, KeyCode::Char('c')))
                    || matches!(key.code, KeyCode::Char('q'))
                {
                    quit_confirm = true;
                    continue;
                }
                match state.handle_key(key) {
                    ConfigAction::Save { close, overwrite } => {
                        let outcome = state.perform_save(overwrite);
                        if outcome == SaveOutcome::Saved && close {
                            return Ok(Flow::Saved);
                        }
                    }
                    ConfigAction::Closed => return Ok(Flow::Cancelled),
                    // Restart actions are unreachable in Shell A (no
                    // restart offer is ever staged with restart_available=false).
                    _ => {}
                }
            }
            Some(Ok(CtEvent::Mouse(mouse))) => {
                // Shell A's local quit-confirmation: same parity as Shell B's
                // app-level gate — clicks are swallowed while the popup is
                // up, the WHEEL stays live (scrolling behind the popup is
                // harmless, the documented Panels convention). Everything
                // else delegates to the shared lib entry point.
                let click = matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left));
                if quit_confirm && click {
                    continue;
                }
                state.handle_mouse(mouse);
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(io::Error::other(e)),
            None => return Ok(Flow::Cancelled), // stream ended: nowhere to go
        }
    }
}

#[cfg(test)]
mod tests;
