//! Application state: chats, selection, input buffer, request lifecycle.

use std::collections::VecDeque;

use crate::backend::BackendEvent;
use crate::markdown;
use crate::model::{ChatLifecycle, Message};
use crate::popup::ErrorPopup;
use crate::theme::Theme;
use tui_textarea::TextArea;

/// Liveness of the backend link as shown by the status-bar indicator.
///
/// This tracks the *transport* only. Per-request progress (queued/running)
/// lives in each chat's [`ChatLifecycle`] and the spinner line — the two
/// never duplicate each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    /// Backend process is up and the handshake succeeded.
    Connected,
    /// (Re)connect attempt in progress.
    Connecting,
    /// No usable backend; recoverable via `R` from the error popup.
    Disconnected,
}

/// One search hit inside a message's rendered text (feat_search_thread).
///
/// `message_index` addresses [`App::chats`] active chat's `messages`;
/// `line_index` is the 0-based index of the RENDERED markdown line (as
/// produced by [`crate::markdown::render`], i.e. the same line the chat pane
/// shows) containing the hit. `line_text` is the plain text of that rendered
/// line (style markup already stripped — the snippet source), `col` the
/// 0-based char offset where the case-insensitive match starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    /// Index into the ACTIVE chat's `messages` vec.
    pub message_index: usize,
    /// Index of the rendered markdown line within that message.
    pub line_index: usize,
    /// Plain text of the rendered line (markup stripped).
    pub line_text: String,
    /// Char offset of the case-insensitive hit inside `line_text`.
    pub col: usize,
}

/// State of the open in-thread search popup (feat_search_thread).
///
/// Lives in [`Mode::Searching`] — deliberately OUT of the chat mutation
/// paths: `query` is the popup's own single-line buffer, `matches` the
/// cached live-computed hit list, `selected` the current selection index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchState {
    /// Current search query (popup's own buffer, never the message draft).
    pub query: String,
    /// Index into `matches` of the currently selected hit.
    pub selected: usize,
    /// Live-computed case-insensitive matches for `query`.
    pub matches: Vec<SearchMatch>,
}

/// One search hit across ALL threads (feat_search_all_threads).
///
/// Extends [`SearchMatch`] with the owning chat: `chat_index` addresses
/// [`App::chats`] and `chat_title` is the thread-title snapshot shown as
/// the popup label. `message_index` stays relative to THAT chat's
/// `messages` vec — the wrapped-row jump math consumes the trio
/// `(chat_index, message_index, line_index, col)` unchanged from the
/// in-thread machinery, only scoped to a different chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalSearchMatch {
    /// Index into [`App::chats`] of the owning thread.
    pub chat_index: usize,
    /// Thread title at match-computation time (popup label).
    pub chat_title: String,
    /// Index into `chats[chat_index].messages` vec.
    pub message_index: usize,
    /// Index of the rendered markdown line within that message.
    pub line_index: usize,
    /// Plain text of the rendered line (markup stripped).
    pub line_text: String,
    /// Char offset of the case-insensitive hit inside `line_text`.
    pub col: usize,
}

/// State of the open ALL-threads search popup (feat_search_all_threads).
///
/// Lives in [`Mode::SearchingAll`] — the same shape as [`SearchState`], but
/// `matches` spans every chat, so each entry carries its own
/// `chat_index`/`chat_title`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalSearchState {
    /// Current search query (popup's own buffer, never the message draft).
    pub query: String,
    /// Index into `matches` of the currently selected hit.
    pub selected: usize,
    /// Live-computed case-insensitive matches across all chats.
    pub matches: Vec<GlobalSearchMatch>,
}

/// State of the open diagnostics-log viewer modal (feat_stderr_log_modal).
///
/// Read position management is deliberately simple, snapshot-copy based:
/// the modal holds a COPY of the ring buffer taken at the last refresh, so
/// the capture task can keep appending (and evicting) freely underneath.
/// `scroll` counts rows scrolled UP from the bottom — `0` is live-tail mode,
/// where the snapshot is refreshed every frame so new lines stream in; any
/// offset > 0 detaches the view: the snapshot freezes and lines arriving
/// after `snapshot_total` are only counted (the `+K new lines` footer), never
/// rendered until the user pages back to the bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogViewerState {
    /// Snapshot copy of the buffered lines (oldest → newest) at the last
    /// refresh (open, live-tail frame, or return-to-bottom).
    pub lines: Vec<String>,
    /// Rows scrolled up from the bottom (0 = live-tail at the bottom).
    pub scroll: u16,
    /// [`crate::diag::total_appended()`] at snapshot time — the baseline the
    /// `+K new lines` footer hint is computed against.
    pub snapshot_total: u64,
}

/// Input mode of the whole app (feature: inline thread rename / search).
///
/// Deliberately tiny and explicit so tests can drive transitions
/// deterministically: `Normal` is everyday chatting; `Renaming` captures all
/// printable input into its own single-line buffer until `Enter` (save) or
/// `Esc` (cancel). The rename draft lives here — NOT in the prompt
/// [`App::input`] textarea — so cancelling can never lose a half-typed
/// message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Everyday input/chatting state.
    Normal,
    /// The ACTIVE chat's title is being edited inline. `buf` holds the raw
    /// untrimmed draft; commit applies trimming and rejects empty results.
    Renaming { buf: String },
    /// The Ctrl+D delete-confirmation popup is open for the ACTIVE chat
    /// (feat_thread_delete). Enter/`y` confirm, Esc/`n` cancel; every other
    /// key is swallowed by the modal branch in `main.rs` so nothing leaks
    /// into the textarea or triggers a global binding.
    ConfirmDelete,
    /// The Ctrl+F in-thread search popup is open (feat_search_thread):
    /// `state` holds the query buffer, cached match list and selection.
    /// Modal-ish isolation mirrors ConfirmDelete — keystrokes go to the
    /// popup only (see `main.rs`), the chat view is read-only, and Enter
    /// jumps the chat scroll to the selected match via
    /// [`App::pending_search_jump`] before closing.
    Searching { state: SearchState },
    /// The Ctrl+Shift+F ALL-threads search popup is open
    /// (feat_search_all_threads): `state` holds the query buffer, cached
    /// match list (spanning every chat, each entry labeled with its thread
    /// title) and selection. Same modal-ish isolation as [`Mode::Searching`]
    /// — one popup at a time — but Enter ACTIVATES the match's thread
    /// (same selection mechanics as Ctrl+↑/↓ switching) and records a
    /// pending wrapped-row jump via [`App::pending_global_search_jump`].
    SearchingAll { state: GlobalSearchState },
    /// The ^G diagnostics-log viewer modal is open (feat_stderr_log_modal):
    /// a monospace view of the unified backend-stderr + `[tui]` lifecycle
    /// ring buffer. Modal isolation like the search popups — PgUp/PgDn (and
    /// ↑/↓) scroll, Esc closes, every other key is swallowed. Opening
    /// resets the unseen-lines counter; live-tail at the bottom, frozen
    /// snapshot with a `+K new lines` footer while scrolled up.
    LogViewer { state: LogViewerState },
}

impl Mode {
    /// True in the everyday chatting state only — no rename session, no
    /// delete-confirm popup, and no search popup open.
    pub fn is_normal(&self) -> bool {
        matches!(self, Mode::Normal)
    }
}
/// feat_focus_panes: which pane owns the keyboard. NOT an [`AppMode`] —
/// the rename/delete/search modals remain [`Mode`]s layered above focus:
/// opening one never changes focus, and closing one always resets it to
/// [`Focus::Chat`] so the editor regains typing immediately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// Everyday chatting state: keys reach the prompt textarea.
    Chat,
    /// The sidebar owns ↑/↓ (thread selection with live active-chat
    /// switching); printable/text-editing keystrokes are swallowed.
    Sidebar,
}

/// Marker: the user pressed `R` in the error popup — the event loop performs
/// the actual async reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectRequest {}

/// A single conversation.
///
/// `id` is the stable thread identifier (UUID v4) persisted across restarts;
/// every request sent for this chat carries a deterministic derivation of it
/// on the wire (see [`crate::live::wire_thread_id`]).
///
/// Per-thread async: each chat independently owns its request lifecycle and a
/// FIFO queue of prompts waiting for the in-flight request to finish. Chats
/// never block each other.
pub struct Chat {
    pub name: String,
    pub id: String,
    pub messages: Vec<Message>,
    /// This chat's own request lifecycle (`Idle` / `Awaiting` / `Running`).
    pub lifecycle: ChatLifecycle,
    /// Prompts submitted while a request was already in flight; sent FIFO as
    /// soon as the current request reaches a terminal event.
    pub queue: VecDeque<String>,
}

impl Chat {
    /// Fresh chat with a generated stable thread id.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            id: crate::history::new_thread_id(),
            messages: Vec::new(),
            lifecycle: ChatLifecycle::Idle,
            queue: VecDeque::new(),
        }
    }

    /// True while this chat has a request in flight.
    pub fn is_busy(&self) -> bool {
        self.lifecycle.is_busy()
    }
}

/// Everything the caller needs to hand one accepted prompt to a backend.
///
/// Generated atomically by [`App::take_input`] so the UI state and the
/// backend agree on the protocol `request_id` (used later for targeted
/// cancel).
#[derive(Clone, Debug)]
pub struct Submitted {
    /// Protocol `request_id` (UUID v4, client-chosen).
    pub request_id: String,
    /// Stable thread id of the active chat (UUID v4).
    pub thread_id: String,
    /// The user prompt text (trimmed, non-empty).
    pub prompt: String,
}

/// Whole UI state.
///
/// There is deliberately NO app-level busy flag: with per-thread async every
/// chat tracks its own lifecycle ([`Chat::lifecycle`] + [`Chat::queue`]) and
/// helpers below delegate to the active chat.
pub struct App {
    pub chats: Vec<Chat>,
    pub active: usize,
    pub scroll: u16,
    /// Visible chat pane height (rows). Set during `ui::render_chat` so
    /// PgUp/PgDn can scroll exactly one page of rows. Defaults to 20
    /// (a conservative page size) until the first render.
    pub chat_visible_rows: u16,
    pub input: TextArea<'static>,
    pub spinner_frame: usize,
    pub should_quit: bool,
    /// Modal error popup (backend failures). When set, it captures input
    /// until dismissed.
    pub error_popup: Option<ErrorPopup>,
    /// Transport liveness for the status-bar indicator.
    pub connection: Connection,
    /// Set by `R` in the error popup; the event loop performs the async
    /// reconnect and clears it.
    pub reconnect_requested: Option<ReconnectRequest>,
    /// One-shot screen-wipe intent set by Ctrl+L (bugfix_ctrl_l_screen_clear)
    /// and consumed by the main loop, which emits the real crossterm
    /// `Clear(All)` outside ratatui's diff-based draw and forces a full
    /// repaint. Visual-only: messages, history and bindings are untouched.
    pub clear_screen_requested: bool,
    /// Cancel target `(request_id, thread_id)` produced by Ctrl+C; the event
    /// loop sends the actual frame. Targets ONLY the active chat's in-flight
    /// request — queued prompts survive a cancel.
    pub pending_cancel: Option<(String, String)>,
    /// Thread id whose persisted history file must be removed
    /// (feat_thread_delete): set by the confirm-popup Enter path; the event
    /// loop performs the actual `history::delete_chat_file_in` and consumes
    /// this. Kept out of the I/O-free [`App`] so state logic stays
    /// unit-testable without a filesystem.
    pub pending_delete: Option<String>,
    /// Transient status toast (feat_thread_delete busy-refusal) shown in the
    /// status line; auto-expires after [`STATUS_MSG_TICKS`] spinner ticks.
    pub status_message: Option<(String, u8)>,
    /// feat_focus_panes: which pane owns the keyboard (Chat by default).
    /// Reset to [`Focus::Chat`] whenever a modal closes — see [`Focus`].
    pub focus: Focus,
    /// Global input mode (feature: inline thread rename / thread delete /
    /// in-thread search).
    pub mode: Mode,
    /// feat_search_thread: pending search jump `(message_index, line_index,
    /// col)` — the char offset inside the message's rendered line. Set by
    /// Enter in the search popup, consumed by `ui::render_chat` (the jump
    /// math needs the SAME wrapped-row totals as rendering, so it happens
    /// there, not in the state layer). `col` lets the jump land on the
    /// WRAPPED row that actually contains the hit inside a long paragraph.
    pub pending_search_jump: Option<(usize, usize, usize)>,
    /// feat_search_all_threads: pending GLOBAL search jump `(chat_index,
    /// message_index, line_index, col)` — `chat_index` is the match's OWNING
    /// thread. Set by Enter in the all-threads search popup AFTER
    /// [`App::jump_to_selected_all`] already activated that thread (the
    /// activation lives in the state layer, the wrapped-row math in
    /// `ui::render_chat` like the in-thread jump). `chat_index` rides along
    /// purely as a defensive marker: render verifies the target is still the
    /// active chat before jumping, so a chat that vanished mid-frame drops
    /// the jump silently instead of scrolling the wrong thread.
    pub pending_global_search_jump: Option<(usize, usize, usize, usize)>,
    /// feat_stderr_log_modal: visible height (rows) of the log-viewer
    /// modal's content area, set during `ui::render_log_viewer` so PgUp/PgDn
    /// scroll exactly one page of modal rows. Defaults to 20 until first
    /// render (same seam as [`App::chat_visible_rows`]).
    pub log_visible_rows: u16,
    /// feat_stderr_log_modal: the consumer-side "seen" watermark — the
    /// [`crate::diag::DiagLog::total`] value at the moment the log stream
    /// was last fully viewed (viewer opened / re-tailed / closed at the
    /// bottom). The `log*` status marker fires while
    /// `diag::total_appended() > log_seen_total` and the viewer is closed.
    /// Tracking unseen state on the consumer side of a monotonic producer
    /// total is race-free by construction — no counter resets to coordinate.
    pub log_seen_total: u64,
    /// feat_status_line: visibility of the one-row status strip (workspace
    /// cwd · active chat's model), toggled with ^O. Default HIDDEN. Pure
    /// VIEW state like [`Focus`] — deliberately NOT a [`Mode`]: it never
    /// captures keys and survives every modal open/close untouched.
    pub status_strip_visible: bool,
    /// feat_status_line: workspace root backing the strip's cwd segment
    /// (shown as its basename). Wired once at startup from the CLI
    /// `--workspace` value (which itself defaults to the process cwd) for
    /// BOTH backends, mock included. The renderer reads it reactively every
    /// frame, so if the root ever changes at runtime the next frame shows
    /// the new basename — today it is CLI-only and never changes.
    /// `None` renders as the `—` placeholder.
    pub workspace_root: Option<String>,
}

const SPINNER: [&str; 10] = [
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];

/// feat_input_grow: hard cap on how many terminal rows the ACTIVE editor
/// block (message draft OR rename draft) may occupy. Beyond this the
/// textarea view auto-scrolls inside the capped window so the caret always
/// stays on screen.
pub const MAX_INPUT_LINES: usize = 20;

/// feat_thread_delete: how many 100 ms spinner ticks a transient status
/// toast stays visible (~2.5 s).
pub const STATUS_MSG_TICKS: u8 = 25;
/// fix_ack_silent_absorb: the protocol-level acknowledgement marker the
/// backend uses for silent turns ("THE ACK RULE" in the chibi backend,
/// `chibi/constants.py`). An agent answer whose ENTIRE content is one or
/// more of these markers (plus optional surrounding whitespace) is a
/// protocol ack, not a user-facing answer — the TUI absorbs it invisibly:
/// no assistant bubble, no error, the pending spinner just resolves to
/// Idle (and the per-thread queue drains normally). Content that merely
/// CONTAINS the marker alongside real text is shown as-is, raw.
pub const ACK_MARKER: &str = "<chibi>ACK</chibi>";

/// Heuristic: does this error message indicate a broken transport (backend
/// process died, pipe broke, cancel impossible) rather than a per-request
/// refusal? Transport failures escalate to the error popup.
pub fn is_transport_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("broken")
        || m.contains("child died")
        || m.contains("connection lost")
        || m.contains("closed stdout")
        || m.contains("spawn failed")
        || m.contains("failed to spawn")
        || m.contains("handshake failed")
        || m.contains("unsupported protocol version")
        || m.contains("pipeline actor stopped")
        || m.contains("stdin already closed")
}

impl App {
    pub fn new(chats: Vec<Chat>) -> Self {
        let mut input = TextArea::default();
        input.set_placeholder_text("Type a message…  (\u{23ce} send)");
        Self {
            chats,
            active: 0,
            scroll: 0,
            chat_visible_rows: 20,
            input,
            spinner_frame: 0,
            should_quit: false,
            error_popup: None,
            connection: Connection::Connecting,
            reconnect_requested: None,
            clear_screen_requested: false,
            pending_cancel: None,
            pending_delete: None,
            status_message: None,
            focus: Focus::Chat,
            mode: Mode::Normal,
            pending_search_jump: None,
            pending_global_search_jump: None,
            log_visible_rows: 20,
            log_seen_total: 0,
            status_strip_visible: false,
            workspace_root: None,
        }
    }

    /// Header title of the active chat.
    pub fn chat_title(&self) -> String {
        self.chats
            .get(self.active)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    }

    /// Stable thread id of the active chat.
    pub fn active_thread_id(&self) -> Option<&str> {
        self.chats.get(self.active).map(|c| c.id.as_str())
    }

    pub fn select_next(&mut self) {
        if self.active + 1 < self.chats.len() {
            self.active += 1;
        }
        self.scroll = 0;
    }

    pub fn select_prev(&mut self) {
        self.active = self.active.saturating_sub(1);
        self.scroll = 0;
    }

    /// feat_focus_panes: toggle which pane owns the keyboard — Ctrl+T flips
    /// [`Focus::Chat`] ↔ [`Focus::Sidebar`] (twice round-trips). No chat-list
    /// side effects: the selection, scroll and lifecycle are untouched; with
    /// zero or one chats toggling is still meaningful (the highlight/dot
    /// emphasis moves even though navigation has nothing to navigate to).
    /// Modal modes swallow this like every other chord — the popup branches
    /// in `main.rs` return before routing.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Chat => Focus::Sidebar,
            Focus::Sidebar => Focus::Chat,
        };
    }

    // ---- feat_status_line: cwd + model status strip ------------------------

    /// feat_status_line: toggle the one-row status strip (workspace cwd ·
    /// active chat's model) with ^O. Pure VIEW state like [`Focus`] —
    /// deliberately not a [`Mode`]: it never captures keys, survives every
    /// modal open/close untouched, and the renderer just reads the flag
    /// each frame. Default: hidden.
    pub fn toggle_status_strip(&mut self) {
        self.status_strip_visible = !self.status_strip_visible;
    }

    /// feat_status_line: the strip's cwd segment — the BASENAME of the
    /// workspace root ([`App::workspace_root`], wired from the CLI at
    /// startup; the TUI knows the root from the request frames / CLI and
    /// today it never changes at runtime, but the value is read reactively
    /// here so a future runtime change shows up on the next frame). A
    /// pathless root (`/`, `.`) or a non-UTF-8 tail degrades to the raw
    /// string; `None` (only before startup wiring) yields `None` and the
    /// renderer shows the `—` placeholder.
    pub fn status_cwd(&self) -> Option<&str> {
        let root = self.workspace_root.as_deref()?;
        let path = std::path::Path::new(root);
        Some(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(root),
        )
    }

    /// feat_status_line: the strip's model segment — the LAST KNOWN model
    /// label of the ACTIVE chat, reusing the feat_agent_model_label
    /// per-message metadata: the most recent assistant message carrying a
    /// non-empty label. Deriving it per frame gives the required update
    /// semantics for free: a result resolution stamps the label onto the
    /// new message (visible next frame), an error/fieldless resolution
    /// stamps `None` (the previous label remains "last known"), and
    /// switching chats re-labels from that chat's own message history.
    /// Session-scoped exactly like the header labels — never persisted.
    /// `None` renders as the `—` placeholder.
    pub fn active_model_label(&self) -> Option<&str> {
        self.chats
            .get(self.active)?
            .messages
            .iter()
            .rev()
            .find_map(|m| m.model_label())
    }

    /// Create a chat with a fresh UUID thread_id, select it. feat_focus_panes:
    /// creating a thread is an editor-bound action — focus lands back on
    /// Chat so typing goes straight into the prompt.
    pub fn new_chat(&mut self) {
        let n = self.chats.len() + 1;
        self.chats.push(Chat::new(format!("New chat {n}")));
        self.active = self.chats.len() - 1;
        self.scroll = 0;
        self.focus = Focus::Chat;
    }

    // ---- rename mode (feature: inline thread rename) ----------------------

    /// Enter RENAME MODE for the ACTIVE chat: the draft is pre-filled with
    /// its current name. No-op when there is no chat or a session is already
    /// open (re-pressing Ctrl+R must not clobber an in-progress draft).
    pub fn begin_rename(&mut self) {
        if !self.mode.is_normal() || self.active_thread_id().is_none() {
            return;
        }
        let name = self.chat_title();
        self.mode = Mode::Renaming { buf: name };
    }

    /// Append one character to the open rename draft.
    pub fn rename_push(&mut self, ch: char) {
        if let Mode::Renaming { buf } = &mut self.mode {
            buf.push(ch);
        }
    }

    /// Backspace: drop the last character of the open rename draft.
    pub fn rename_backspace(&mut self) {
        if let Mode::Renaming { buf } = &mut self.mode {
            buf.pop();
        }
    }

    /// Current rename draft, when a rename session is open.
    pub fn rename_buf(&self) -> Option<&str> {
        match &self.mode {
            Mode::Renaming { buf } => Some(buf.as_str()),
            // The delete-confirm popup, the search popups and the log
            // viewer have no draft of their own (search queries live in
            // their own state).
            Mode::Normal
            | Mode::ConfirmDelete
            | Mode::Searching { .. }
            | Mode::SearchingAll { .. }
            | Mode::LogViewer { .. } => None,
        }
    }

    /// feat_input_grow: terminal rows the ACTIVE editor block needs right
    /// now — visible buffer lines clamped to `1..=MAX_INPUT_LINES`.
    ///
    /// Normal mode counts the multiline message draft (`Shift+Enter`);
    /// rename mode counts its own draft (modified Enters / paste insert
    /// `\n` into the title too). Recomputed per frame by `ui::draw` and fed
    /// to the root vertical layout; a cleared input collapses back to
    /// exactly one row.
    pub fn input_lines_height(&self) -> u16 {
        let buffer_lines = match &self.mode {
            Mode::Renaming { buf } => buf.split('\n').count(),
            // The delete-confirm, search and log-viewer popups overlay the
            // normal editor: their height derives from the message draft,
            // exactly like Normal.
            Mode::Normal
            | Mode::ConfirmDelete
            | Mode::Searching { .. }
            | Mode::SearchingAll { .. }
            | Mode::LogViewer { .. } => self.input.lines().len(),
        };
        buffer_lines.clamp(1, MAX_INPUT_LINES) as u16
    }

    /// Commit the ACTIVE chat's rename: trims whitespace; empty/whitespace-only
    /// names are rejected (the old name survives). Returns whether the name
    /// changed. The caller persists afterwards — [`App`] has no I/O.
    ///
    /// Works regardless of the active chat's lifecycle: a thread title has
    /// nothing to do with request state, so renaming busy chats is allowed.
    /// Always returns to [`Mode::Normal`], even on rejection.
    pub fn commit_rename(&mut self) -> bool {
        let trimmed = match &self.mode {
            Mode::Renaming { buf } => buf.trim().to_owned(),
            // Not renaming (Normal, any popup — delete-confirm, search, log
            // viewer): no-op.
            Mode::Normal
            | Mode::ConfirmDelete
            | Mode::Searching { .. }
            | Mode::SearchingAll { .. }
            | Mode::LogViewer { .. } => return false,
        };
        self.mode = Mode::Normal;
        // feat_focus_panes: a closed modal returns keyboard ownership to
        // the editor pane regardless of where focus was before it opened.
        self.focus = Focus::Chat;
        let Some(chat) = self.chats.get_mut(self.active) else {
            return false;
        };
        if trimmed.is_empty() || chat.name == trimmed {
            return false;
        }
        chat.name = trimmed;
        true
    }

    /// Leave rename mode discarding the draft. The everyday prompt buffer
    /// ([`App::input`]) was never touched by rename mode, so "restores the
    /// normal input state" holds by construction; asserted in tests.
    pub fn cancel_rename(&mut self) -> bool {
        if self.mode.is_normal() {
            return false;
        }
        self.mode = Mode::Normal;
        self.focus = Focus::Chat; // feat_focus_panes: modal closed → editor pane
        true
    }

    // ---- in-thread search (feat_search_thread) ----------------------------

    /// Open the in-thread search popup for the ACTIVE chat with an empty
    /// query. No-op when another modal owns the keyboard (rename session,
    /// delete-confirm popup, an already-open search) or when there is no
    /// active chat. Works while the chat is Busy — search is strictly
    /// read-only (matches are computed from a snapshot of the messages).
    pub fn begin_search(&mut self) {
        if !self.mode.is_normal() || self.active_thread_id().is_none() {
            return;
        }
        self.mode = Mode::Searching {
            state: SearchState {
                query: String::new(),
                selected: 0,
                matches: Vec::new(),
            },
        };
    }

    /// Append one character to the search query and recompute matches live.
    pub fn search_push(&mut self, ch: char) {
        if let Mode::Searching { state } = &mut self.mode {
            state.query.push(ch);
        }
        self.refresh_search_matches();
    }

    /// Backspace: drop the last character of the search query and recompute
    /// matches live.
    pub fn search_backspace(&mut self) {
        if let Mode::Searching { state } = &mut self.mode {
            state.query.pop();
        }
        self.refresh_search_matches();
    }

    /// Recompute the match list + selection clamp from the CURRENT query.
    /// No-op outside search mode (defensive — key routing only calls this
    /// from the popup branch).
    fn refresh_search_matches(&mut self) {
        let query = match &self.mode {
            Mode::Searching { state } => state.query.clone(),
            _ => return,
        };
        let matches = self.compute_search_matches(&query);
        if let Mode::Searching { state } = &mut self.mode {
            state.matches = matches;
            state.selected = state.selected.min(state.matches.len().saturating_sub(1));
        }
    }

    /// Current search query, when the search popup is open.
    pub fn search_query(&self) -> Option<&str> {
        match &self.mode {
            Mode::Searching { state } => Some(state.query.as_str()),
            _ => None,
        }
    }

    /// Live match list of the open search popup (empty when closed).
    pub fn search_matches(&self) -> &[SearchMatch] {
        match &self.mode {
            Mode::Searching { state } => &state.matches,
            _ => &[],
        }
    }

    /// Index of the currently selected match (0 when nothing selected).
    pub fn search_selected(&self) -> usize {
        match &self.mode {
            Mode::Searching { state } => state.selected,
            _ => 0,
        }
    }

    /// Move the selection to the NEXT match (down), clamped at the end.
    pub fn search_select_next(&mut self) {
        if let Mode::Searching { state } = &mut self.mode {
            if !state.matches.is_empty() && state.selected + 1 < state.matches.len() {
                state.selected += 1;
            }
        }
    }

    /// Move the selection to the PREVIOUS match (up), clamped at the start.
    pub fn search_select_prev(&mut self) {
        if let Mode::Searching { state } = &mut self.mode {
            state.selected = state.selected.saturating_sub(1);
        }
    }

    /// Close the search popup WITHOUT jumping. The chat view, messages and
    /// the message draft are untouched (the query lived in the popup only).
    pub fn cancel_search(&mut self) -> bool {
        if !matches!(self.mode, Mode::Searching { .. }) {
            return false;
        }
        self.mode = Mode::Normal;
        self.focus = Focus::Chat; // feat_focus_panes: modal closed → editor pane
        true
    }

    /// Enter: record a pending jump to the selected match and close the
    /// popup. The actual scroll happens in `ui::render_chat` on the next
    /// frame — it needs the SAME wrapped-row totals as rendering, which
    /// only exist there. No-op (popup stays open, view unchanged) when the
    /// query is empty or produced no matches.
    pub fn jump_to_selected(&mut self) -> bool {
        let jump = match &self.mode {
            Mode::Searching { state } => state
                .matches
                .get(state.selected)
                .map(|m| (m.message_index, m.line_index, m.col)),
            _ => None,
        };
        match jump {
            Some(target) => {
                self.pending_search_jump = Some(target);
                self.mode = Mode::Normal;
                // feat_focus_panes: modal closed → editor pane.
                self.focus = Focus::Chat;
                true
            }
            None => false,
        }
    }

    /// Case-insensitive substring matches of `query` over the ACTIVE chat's
    /// rendered messages (style markup stripped, role headers excluded —
    /// they are UI chrome, not message content).
    fn compute_search_matches(&self, query: &str) -> Vec<SearchMatch> {
        match self.chats.get(self.active) {
            Some(chat) => collect_search_matches(chat.messages.iter(), query),
            None => Vec::new(),
        }
    }

    // ---- all-threads search (feat_search_all_threads) --------------------

    /// Open the GLOBAL search popup over ALL chats with an empty query.
    /// No-op when another modal owns the keyboard (rename session,
    /// delete-confirm popup, an already-open in-thread or global search) —
    /// one modal at a time, exactly like the other popups. Works with any
    /// number of chats (zero yields a graceful 0-match state) and while any
    /// chat is Busy: search is strictly read-only (matches are computed
    /// from snapshots of the messages).
    pub fn begin_search_all(&mut self) {
        if !self.mode.is_normal() {
            return;
        }
        self.mode = Mode::SearchingAll {
            state: GlobalSearchState {
                query: String::new(),
                selected: 0,
                matches: Vec::new(),
            },
        };
    }

    /// Append one character to the global-search query and recompute matches
    /// live across ALL chats.
    pub fn search_all_push(&mut self, ch: char) {
        if let Mode::SearchingAll { state } = &mut self.mode {
            state.query.push(ch);
        }
        self.refresh_search_all_matches();
    }

    /// Backspace: drop the last character of the global-search query and
    /// recompute matches live.
    pub fn search_all_backspace(&mut self) {
        if let Mode::SearchingAll { state } = &mut self.mode {
            state.query.pop();
        }
        self.refresh_search_all_matches();
    }

    /// Recompute the all-threads match list + selection clamp from the
    /// CURRENT query. No-op outside global-search mode (defensive — key
    /// routing only calls this from the popup branch). A chat removed
    /// between frames is handled HERE: matches are rebuilt from the live
    /// chat list on every keystroke, so entries of vanished chats simply
    /// disappear (the popup recomputes live per keystroke by design).
    fn refresh_search_all_matches(&mut self) {
        let query = match &self.mode {
            Mode::SearchingAll { state } => state.query.clone(),
            _ => return,
        };
        let matches = self.compute_search_matches_all(&query);
        if let Mode::SearchingAll { state } = &mut self.mode {
            state.matches = matches;
            state.selected = state.selected.min(state.matches.len().saturating_sub(1));
        }
    }

    /// Current global-search query, when the popup is open.
    pub fn search_all_query(&self) -> Option<&str> {
        match &self.mode {
            Mode::SearchingAll { state } => Some(state.query.as_str()),
            _ => None,
        }
    }

    /// Live all-threads match list of the open popup (empty when closed).
    pub fn search_all_matches(&self) -> &[GlobalSearchMatch] {
        match &self.mode {
            Mode::SearchingAll { state } => &state.matches,
            _ => &[],
        }
    }

    /// Index of the currently selected global match (0 when nothing is
    /// selected).
    pub fn search_all_selected(&self) -> usize {
        match &self.mode {
            Mode::SearchingAll { state } => state.selected,
            _ => 0,
        }
    }

    /// Move the selection to the NEXT global match (down), clamped at the
    /// end.
    pub fn search_all_select_next(&mut self) {
        if let Mode::SearchingAll { state } = &mut self.mode {
            if !state.matches.is_empty() && state.selected + 1 < state.matches.len() {
                state.selected += 1;
            }
        }
    }

    /// Move the selection to the PREVIOUS global match (up), clamped at the
    /// start.
    pub fn search_all_select_prev(&mut self) {
        if let Mode::SearchingAll { state } = &mut self.mode {
            state.selected = state.selected.saturating_sub(1);
        }
    }

    /// Close the global search popup WITHOUT jumping or switching threads.
    /// The chats, messages and the message draft are untouched (the query
    /// lived in the popup only).
    pub fn cancel_search_all(&mut self) -> bool {
        if !matches!(self.mode, Mode::SearchingAll { .. }) {
            return false;
        }
        self.mode = Mode::Normal;
        self.focus = Focus::Chat; // feat_focus_panes: modal closed → editor pane
        true
    }

    /// Enter: activate the TARGET THREAD of the selected global match (same
    /// selection mechanics as Ctrl+↑/↓ switching — bounds-safe index set +
    /// chat-scroll reset to follow-bottom) and record a pending
    /// wrapped-row-accurate jump. The actual scroll happens in
    /// `ui::render_chat` on the next frame, where the SAME wrapped-row
    /// totals as rendering exist. The match list is recomputed first so a
    /// chat that vanished mid-popup is handled by the normal recompute path
    /// (its matches drop out silently). No-op — popup stays open, view
    /// unchanged — when the query is empty or produced no matches.
    pub fn jump_to_selected_all(&mut self) -> bool {
        self.refresh_search_all_matches();
        let jump = match &self.mode {
            Mode::SearchingAll { state } => state
                .matches
                .get(state.selected)
                .map(|m| (m.chat_index, m.message_index, m.line_index, m.col)),
            _ => None,
        };
        match jump {
            Some((chat_index, message_index, line_index, col)) if chat_index < self.chats.len() => {
                // Activate the target thread exactly like Ctrl+↑/↓: index
                // set + scroll reset to follow-bottom (the jump math in
                // render_chat then overrides scroll with the match's row).
                self.active = chat_index;
                self.scroll = 0;
                self.pending_global_search_jump =
                    Some((chat_index, message_index, line_index, col));
                self.mode = Mode::Normal;
                // feat_focus_panes: modal closed → editor pane.
                self.focus = Focus::Chat;
                true
            }
            _ => false,
        }
    }

    /// Case-insensitive substring matches of `query` over ALL chats'
    /// rendered messages, ordered by CHAT ORDER then MESSAGE ORDER within
    /// each chat. Reuses [`collect_search_matches`] per chat — the same
    /// matching logic, the same rendered-text scope (markup stripped, role
    /// headers excluded, pending rows skipped); only the scope is additive.
    fn compute_search_matches_all(&self, query: &str) -> Vec<GlobalSearchMatch> {
        let mut out = Vec::new();
        for (chat_index, chat) in self.chats.iter().enumerate() {
            for m in collect_search_matches(chat.messages.iter(), query) {
                out.push(GlobalSearchMatch {
                    chat_index,
                    chat_title: chat.name.clone(),
                    message_index: m.message_index,
                    line_index: m.line_index,
                    line_text: m.line_text,
                    col: m.col,
                });
            }
        }
        out
    }

    // ---- diagnostics log viewer (feat_stderr_log_modal) -------------------

    /// Open the ^G diagnostics-log viewer modal with a snapshot of the ring
    /// buffer, live-tailing at the bottom. Opening resets the unseen-lines
    /// counter — everything buffered up to now becomes "seen"; lines that
    /// arrive afterwards drive the `log*` status marker again. No-op when
    /// another modal owns the keyboard (rename session, delete-confirm
    /// popup, a search popup, an already-open viewer) — one modal at a time,
    /// exactly like the other popups. Works with any connection state and
    /// any number of chats: the viewer reads the process-global diag stream,
    /// never the chat state.
    pub fn begin_log_viewer(&mut self) {
        if !self.mode.is_normal() {
            return;
        }
        let (lines, snapshot_total) = crate::diag::view();
        // Reset-on-open, consumer-side: everything up to this total is seen.
        self.log_seen_total = snapshot_total;
        self.mode = Mode::LogViewer {
            state: LogViewerState {
                lines,
                scroll: 0,
                snapshot_total,
            },
        };
    }

    /// PgUp (or ↑): detach from the bottom / scroll further up. The snapshot
    /// freezes — lines arriving while detached are only counted (the
    /// `+K new lines` footer), never rendered into the frozen view.
    pub fn log_scroll_up(&mut self, amount: u16) {
        if let Mode::LogViewer { state } = &mut self.mode {
            state.scroll = state.scroll.saturating_add(amount);
        }
    }

    /// PgDn (or ↓): back towards the bottom. Reaching `0` re-arms live-tail:
    /// the snapshot refreshes to the current ring content and everything
    /// shown counts as seen.
    pub fn log_scroll_down(&mut self, amount: u16) {
        if let Mode::LogViewer { state } = &mut self.mode {
            state.scroll = state.scroll.saturating_sub(amount);
            if state.scroll == 0 {
                let (lines, snapshot_total) = crate::diag::view();
                state.lines = lines;
                state.snapshot_total = snapshot_total;
                self.log_seen_total = snapshot_total;
            }
        }
    }

    /// Close the log viewer (Esc). Closing while live-tailing (at the
    /// bottom) marks the stream as seen — the user just watched those lines
    /// arrive. Closing while scrolled UP keeps the unseen counter, so the
    /// `log*` status marker keeps flagging the lines missed while detached.
    pub fn close_log_viewer(&mut self) -> bool {
        let at_bottom = match &self.mode {
            Mode::LogViewer { state } => state.scroll == 0,
            _ => return false,
        };
        self.mode = Mode::Normal;
        // feat_focus_panes: modal closed → editor pane.
        self.focus = Focus::Chat;
        if at_bottom {
            // The user just watched the tail arrive: everything is seen.
            self.log_seen_total = crate::diag::total_appended();
        }
        true
    }

    // ---- lifecycle delegation (active chat) ------------------------------

    /// Lifecycle of the active chat (drives the spinner line). Empty chats
    /// list degenerates to Idle.
    pub fn active_lifecycle(&self) -> &ChatLifecycle {
        static IDLE: ChatLifecycle = ChatLifecycle::Idle;
        self.chats
            .get(self.active)
            .map(|c| &c.lifecycle)
            .unwrap_or(&IDLE)
    }

    /// True when the ACTIVE chat has a request in flight. Other chats may be
    /// busy at the same time — that never blocks this one.
    pub fn is_busy(&self) -> bool {
        self.chats
            .get(self.active)
            .is_some_and(|chat| chat.is_busy())
    }

    /// True when ANY chat has a request in flight (spinner tick gating).
    pub fn any_busy(&self) -> bool {
        self.chats.iter().any(Chat::is_busy)
    }

    /// Protocol request id currently tracked by the ACTIVE chat, if any.
    pub fn active_request_id(&self) -> Option<&str> {
        self.chats
            .get(self.active)
            .and_then(|chat| chat.lifecycle.request_id())
    }

    // ---- submission ------------------------------------------------------

    /// Extract the current input buffer as a [`Submitted`] prompt bundle.
    ///
    /// Per-thread async semantics:
    /// * empty/whitespace buffer → `None`;
    /// * idle active chat → the bundle is returned for immediate sending;
    /// * BUSY active chat → the prompt is appended to that chat's FIFO queue
    ///   (visible queued bubble included) and `None` is returned — the caller
    ///   must NOT send anything to the backend.
    ///
    /// The buffer is consumed in both non-empty cases. State mutation for the
    /// immediate-send case happens in [`App::begin_request`].
    pub fn take_input(&mut self) -> Option<Submitted> {
        let text = self.input.lines().join("\n");
        let text = text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        // Reset the TextArea in place, keeping its placeholder.
        self.input = TextArea::default();
        self.input
            .set_placeholder_text("Type a message…  (\u{23ce} send)");

        let chat = self.chats.get_mut(self.active)?;
        if chat.lifecycle.is_busy() {
            // Enqueue into THIS chat's FIFO; other chats are unaffected.
            enqueue_prompt(chat, text);
            return None;
        }
        Some(Submitted {
            request_id: crate::history::new_request_id(),
            thread_id: chat.id.clone(),
            prompt: text,
        })
    }

    /// Register the in-flight request produced by [`App::take_input`]:
    /// appends the outgoing user message plus the pending assistant
    /// placeholder and moves the ACTIVE chat's lifecycle to `Awaiting`.
    ///
    /// For a prompt drained from a chat's FIFO queue use
    /// [`App::dequeue_next_for`] instead — it works on ANY chat (the event
    /// loop needs it for background chats) and swaps the queued marker in
    /// place rather than appending a duplicate user bubble.
    pub fn begin_request(&mut self, submitted: &Submitted) {
        if let Some(chat) = self.chats.get_mut(self.active) {
            chat.messages.push(Message::user(submitted.prompt.clone()));
            chat.messages.push(Message::assistant_pending());
            chat.lifecycle = ChatLifecycle::Awaiting {
                request_id: submitted.request_id.clone(),
            };
        }
        self.scroll = 0; // stick to bottom
    }

    // ---- queued prompts --------------------------------------------------

    /// Number of prompts waiting in the ACTIVE chat's queue.
    pub fn active_queue_len(&self) -> usize {
        self.chats
            .get(self.active)
            .map(|c| c.queue.len())
            .unwrap_or(0)
    }

    /// Pop the next queued prompt from the chat owning `thread_id` (FIFO
    /// head) and start it: the queued marker bubble becomes the live pending
    /// placeholder (the user bubble already exists from enqueue time), that
    /// chat's lifecycle → `Awaiting`. Returns the full [`Submitted`] bundle
    /// so the event loop can send it to the backend. `None` when nothing is
    /// queued for that thread.
    ///
    /// Note: unlike [`App::begin_request`] this works on ANY chat — the event
    /// loop uses it after terminal events, including for background chats.
    pub fn dequeue_next_for(&mut self, thread_id: &str) -> Option<Submitted> {
        let chat = self.chats.iter_mut().find(|c| c.id == thread_id)?;
        let prompt = chat.queue.pop_front()?;
        let submitted = Submitted {
            request_id: crate::history::new_request_id(),
            thread_id: chat.id.clone(),
            prompt,
        };
        // The user bubble was appended at enqueue time; swap the oldest
        // remaining queued marker for the real pending placeholder (display
        // order == FIFO order).
        if let Some(marker) = chat.messages.iter_mut().find(|m| is_queued_marker(m)) {
            *marker = Message::assistant_pending();
        } else {
            chat.messages.push(Message::assistant_pending());
        }
        chat.lifecycle = ChatLifecycle::Awaiting {
            request_id: submitted.request_id.clone(),
        };
        Some(submitted)
    }

    // ---- cancel ----------------------------------------------------------

    /// Target [`Submitted`]-compatible pair `(request_id, thread_id)` for
    /// cancelling the IN-FLIGHT request of the active chat, if any.
    ///
    /// Queued prompts are intentionally untouched: cancelling resolves only
    /// the current pending placeholder; whatever is still in the queue stays
    /// there and is sent afterwards.
    #[allow(clippy::option_option)]
    pub fn cancel_active(&self) -> Option<(String, String)> {
        match (self.active_request_id(), self.active_thread_id()) {
            (Some(request_id), Some(thread_id)) => {
                Some((request_id.to_owned(), thread_id.to_owned()))
            }
            _ => None,
        }
    }

    /// Show the modal error popup. Replaces any previously shown error —
    /// only the latest failure matters for recovery.
    pub fn show_error(&mut self, message: impl Into<String>) {
        self.error_popup = Some(ErrorPopup::new(message));
    }

    /// Dismiss the error popup without reconnecting.
    pub fn dismiss_error(&mut self) {
        self.error_popup = None;
        // feat_focus_panes: a closed modal returns keyboard ownership to
        // the editor pane regardless of where focus was before it appeared
        // (a transport failure can interrupt sidebar navigation).
        self.focus = Focus::Chat;
    }

    /// Clear the whole input buffer and park the cursor at the start
    /// (`Ctrl+L`).
    ///
    /// The TextArea is rebuilt in place so its placeholder survives; undo
    /// history is intentionally reset too — a "clear" that can be undone by
    /// a stray Ctrl+U surprises more than it helps.
    pub fn clear_input(&mut self) {
        self.input = TextArea::default();
        self.input
            .set_placeholder_text("Type a message…  (\u{23ce} send)");
    }

    // ---- screen clear (bugfix_ctrl_l_screen_clear) -------------------------

    /// Testable seam behind `Ctrl+L`: record the one-shot screen-wipe intent
    /// AND reset the chat view state to follow-bottom (`scroll = 0`), so the
    /// post-clear full repaint shows the newest messages.
    ///
    /// Visual-only: messages, history files, popups and other bindings are
    /// untouched. The main loop consumes the flag via
    /// [`App::take_clear_screen_request`] and performs the real terminal
    /// `Clear(All)` + immediate full repaint — unit tests need no terminal.
    pub fn request_clear_screen(&mut self) {
        self.scroll = 0;
        self.clear_screen_requested = true;
    }

    /// Consume the pending screen-wipe intent (one-shot; main loop only).
    pub fn take_clear_screen_request(&mut self) -> bool {
        std::mem::take(&mut self.clear_screen_requested)
    }

    // ---- thread delete (feat_thread_delete) -------------------------------

    /// Open the delete-confirmation popup for the ACTIVE chat — IDLE ONLY.
    ///
    /// Refuses with a transient status toast ([`App::show_status`], the
    /// popup never opens) when the active chat has a request in flight
    /// (`Awaiting`/`Running`) OR prompts waiting in its FIFO queue.
    /// Background chats' work is irrelevant: the guard deliberately
    /// inspects only the chat about to be deleted. No-op outside Normal
    /// mode (a rename session or another popup owns the keyboard) and with
    /// no active chat.
    pub fn begin_delete_confirm(&mut self) {
        if !self.mode.is_normal() {
            return;
        }
        let Some(chat) = self.chats.get(self.active) else {
            return;
        };
        if chat.is_busy() || !chat.queue.is_empty() {
            self.show_status("can't delete — busy");
            return;
        }
        self.mode = Mode::ConfirmDelete;
    }

    /// Leave the confirm popup without deleting. The active chat, its
    /// messages and the message draft are untouched; rename-mode
    /// interaction is unaffected because the draft lives in the prompt
    /// buffer, which was never touched by the popup.
    pub fn cancel_delete(&mut self) -> bool {
        if !matches!(self.mode, Mode::ConfirmDelete) {
            return false;
        }
        self.mode = Mode::Normal;
        self.focus = Focus::Chat; // feat_focus_panes: modal closed → editor pane
        true
    }

    /// Confirm the deletion: removes the ACTIVE chat from state entirely,
    /// records its thread id in [`App::pending_delete`] (the event loop
    /// deletes the persisted history file via `history::delete_chat_file_in`
    /// — idempotent, a missing file is success) and selects the neighbour:
    /// the NEXT chat if one exists, else the PREV, else the clean empty
    /// state (zero chats — placeholder input, empty pane, exactly the
    /// visual of a fresh `App::new(Vec::new())`). The chat view always
    /// returns to follow-bottom (`scroll = 0`).
    ///
    /// Returns the removed thread id (also stored in `pending_delete`).
    /// No-op when no popup is open (defensive — the key routing only calls
    /// this from ConfirmDelete mode).
    pub fn confirm_delete(&mut self) -> Option<String> {
        if !matches!(self.mode, Mode::ConfirmDelete) {
            return None;
        }
        self.mode = Mode::Normal;
        // feat_focus_panes: modal closed → editor pane (even into the
        // clean empty state — focus is pane-level state, not selection).
        self.focus = Focus::Chat;
        let chat = self.chats.get(self.active)?;
        let removed_id = chat.id.clone();
        self.chats.remove(self.active);
        if self.chats.is_empty() {
            // Clean empty state: nothing to select.
            self.active = 0;
        } else if self.active >= self.chats.len() {
            // Removed the LAST chat: the previous one slides into focus.
            self.active = self.chats.len() - 1;
        }
        // Removed a first/middle chat: `active` already points at the chat
        // that shifted into the slot (the old NEXT neighbour).
        self.scroll = 0; // follow-bottom for the newly selected chat
        self.pending_delete = Some(removed_id.clone());
        Some(removed_id)
    }

    // ---- transient status toast -------------------------------------------

    /// Show a transient status message in the status line, replacing any
    /// current toast. Auto-expires after [`STATUS_MSG_TICKS`] spinner ticks
    /// (see [`App::tick_status_message`]).
    pub fn show_status(&mut self, message: impl Into<String>) {
        self.status_message = Some((message.into(), STATUS_MSG_TICKS));
    }

    /// Decrement the toast lifetime; clears it at zero. Called by the event
    /// loop's 100 ms spinner tick so the toast vanishes on its own.
    pub fn tick_status_message(&mut self) {
        if let Some((_, ticks)) = &mut self.status_message {
            *ticks = ticks.saturating_sub(1);
            if *ticks == 0 {
                self.status_message = None;
            }
        }
    }

    /// Local fallback for Ctrl+C when there is no live backend to receive a
    /// cancel frame (mock mode has no cancel protocol; placeholder mode has
    /// no backend at all). Resolves the active chat's pending placeholder
    /// immediately and returns THAT chat to Idle so its spinner can never get
    /// stuck. The chat's queue survives; mock mode simply has no auto-send
    /// pump (same trade-off as before this feature).
    pub fn resolve_cancel_locally(&mut self) {
        if let Some(chat) = self.chats.get_mut(self.active) {
            resolve_live_placeholder(chat, "_Cancelled._".to_owned(), None);
            chat.lifecycle = ChatLifecycle::Idle;
        }
    }

    /// Apply an event from the backend source.
    ///
    /// Request-scoped events carry a `thread_id` (per-thread async) and are
    /// routed to THAT chat, matched against its tracked lifecycle request id:
    /// stray events for unknown threads or finished requests are ignored so
    /// they cannot corrupt state or resurrect spinners. After a terminal
    /// event the chat's FIFO queue is NOT drained here — the event loop owns
    /// spawning the next request via [`App::dequeue_next_for`], because only
    /// the loop can talk to the backend source.
    ///
    /// Events whose `thread_id` is empty are legacy/mock events targeting the
    /// ACTIVE chat (mocks never run in background chats).
    ///
    /// The connection-level [`BackendEvent::Disconnected`] is applied
    /// unconditionally — it carries no request.
    pub fn apply_backend_event(&mut self, event: BackendEvent) {
        if let BackendEvent::Disconnected = event {
            // The event source itself reported the link down.
            self.connection = Connection::Disconnected;
            return;
        }

        // Route to the owning chat by thread id (empty → active chat).
        let route_thread_id: Option<String> = match &event {
            BackendEvent::Queued { thread_id, .. } => Some(thread_id.clone()),
            BackendEvent::Running { thread_id, .. } => Some(thread_id.clone()),
            BackendEvent::Result { thread_id, .. } => Some(thread_id.clone()),
            BackendEvent::Error { thread_id, .. } => thread_id.clone(),
            // Internal pump signal: handled by the event loop, never here.
            BackendEvent::QueueDrain { .. } => return,
            BackendEvent::Disconnected => None,
        };
        let target = match route_thread_id {
            Some(tid) if !tid.is_empty() => tid,
            _ => match self.active_thread_id() {
                Some(tid) => tid.to_owned(),
                None => return,
            },
        };

        let Some(chat_index) = self.chats.iter().position(|c| c.id == target) else {
            return; // unknown thread: not ours
        };
        // The chat must currently track a request; events for idle chats are
        // stale/stray and ignored.
        let Some(tracked_request_id) = self.chats[chat_index]
            .lifecycle
            .request_id()
            .map(str::to_owned)
        else {
            return;
        };

        match event {
            BackendEvent::Queued { .. } => {
                self.chats[chat_index].lifecycle = ChatLifecycle::Awaiting {
                    request_id: tracked_request_id.clone(),
                };
            }
            BackendEvent::Running { .. } => {
                self.chats[chat_index].lifecycle = ChatLifecycle::Running {
                    request_id: tracked_request_id.clone(),
                };
            }
            BackendEvent::Result {
                markdown,
                request_id,
                model,
                ..
            } => {
                if event_matches_request(request_id, &tracked_request_id) {
                    let chat = &mut self.chats[chat_index];
                    if is_invisible_result(&markdown) {
                        // fix_ack_silent_absorb: an empty or pure-ACK answer is
                        // a protocol-level acknowledgement, not a user-facing
                        // reply — absorb it invisibly. The pending placeholder
                        // is dropped (no empty bubble), the lifecycle resolves
                        // to Idle so the active-chat spinner stops cleanly and
                        // re-arms on the next prompt, and NO error/toast fires.
                        // The per-thread queue drain is unaffected: the live
                        // glue emits QueueDrain after every terminal event
                        // regardless of content, so queued prompts still send.
                        drop_live_pending_placeholder(chat);
                    } else {
                        resolve_live_placeholder(chat, markdown, model);
                    }
                    chat.lifecycle = ChatLifecycle::Idle;
                    self.scroll = 0;
                }
            }
            BackendEvent::Error {
                message,
                request_id,
                ..
            } => {
                if event_matches_request(request_id, &tracked_request_id) {
                    let chat = &mut self.chats[chat_index];
                    resolve_live_placeholder(chat, format!("**Error:** {message}"), None);
                    chat.lifecycle = ChatLifecycle::Idle;

                    // Transport-level failures (broken pipe, lost backend,
                    // failed cancel of a dead link) escalate to the modal
                    // popup; the connection indicator flips to disconnected.
                    // Per-request errors (bad prompt, refused request) stay
                    // inline.
                    if is_transport_failure(&message) {
                        self.connection = Connection::Disconnected;
                        self.show_error(message);
                    }
                    self.scroll = 0;
                }
            }
            // Handled by the early returns above; arms kept for exhaustiveness.
            BackendEvent::QueueDrain { .. } | BackendEvent::Disconnected => {}
        }
    }

    pub fn tick_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
    }

    pub fn spinner_char(&self) -> &'static str {
        SPINNER[self.spinner_frame]
    }

    /// Scroll up (towards older content) by lines; `0` means "stick to bottom".
    ///
    /// The offset is clamped to a generous ceiling (`u16::MAX / 2`) purely as
    /// an overflow guard: the exact upper bound is content-dependent, so it is
    /// clamped at render time via [`crate::ui::scroll_skip`].
    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount).min(u16::MAX / 2);
    }

    /// Scroll down (towards newer content); reaching `0` re-enables
    /// follow-bottom mode.
    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    /// True when the user is at the bottom (auto-follow new messages).
    pub fn at_bottom(&self) -> bool {
        self.scroll == 0
    }
}

/// Append one prompt to a busy chat's visible queue: user bubble plus a small
/// pending assistant marker showing the position in the FIFO. The marker row
/// is swapped for the real pending answer when the prompt leaves the queue
/// (see [`App::dequeue_next_for`]).
fn enqueue_prompt(chat: &mut Chat, prompt: String) {
    chat.messages.push(Message::user(prompt.clone()));
    chat.queue.push_back(prompt);
    chat.messages
        .push(Message::assistant_queued(chat.queue.len()));
}

/// Marker predicate for the visible "⏳ queued (#n)" placeholder rows.
fn is_queued_marker(message: &Message) -> bool {
    message.pending && message.markdown.starts_with("\u{23f3} queued")
}

/// Collect case-insensitive substring matches of `query` over the RENDERED
/// text of an iterator of messages (feat_search_thread).
///
/// Matching runs on the same [`crate::markdown::render`] output the chat
/// pane paints — style markup already stripped, so `**bold**` matches
/// `bold` (never the asterisks) and role header lines are excluded by
/// construction (they are UI chrome drawn by `ui::render_chat`, not message
/// content). Pending/queued placeholder rows carry no displayable text and
/// are skipped. Each hit reports `(message_index, line_index)` so the jump
/// math in `ui.rs` can map it onto the SAME wrapped-row totals as
/// rendering.
///
/// The iterator parameter is the task-6 hook: an all-threads search chains
/// every chat's messages through this same function additively — only the
/// scope argument changes, never the matching logic.
pub fn collect_search_matches<'a>(
    messages: impl Iterator<Item = &'a Message>,
    query: &str,
) -> Vec<SearchMatch> {
    let qchars: Vec<char> = query.chars().collect();
    if qchars.is_empty() {
        return Vec::new();
    }
    let theme = Theme::tokyo_night();
    let mut out = Vec::new();
    for (message_index, msg) in messages.enumerate() {
        if msg.pending {
            continue; // no displayable text to match
        }
        let rendered = markdown::render(&msg.markdown, &theme);
        for (line_index, line) in rendered.iter().enumerate() {
            let plain: String = line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            let chars: Vec<char> = plain.chars().collect();
            if qchars.len() > chars.len() {
                continue;
            }
            // Char-window case-insensitive search: works on CHARS so `col`
            // is a valid char offset for the snippet renderer, and never
            // panics on multi-byte glyphs (byte slicing on a lowercased
            // copy would be unsafe for non-ASCII).
            let mut from = 0;
            while from + qchars.len() <= chars.len() {
                let hit = chars[from..from + qchars.len()]
                    .iter()
                    .zip(&qchars)
                    .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()));
                if hit {
                    out.push(SearchMatch {
                        message_index,
                        line_index,
                        line_text: plain.clone(),
                        col: from,
                    });
                    from += qchars.len();
                } else {
                    from += 1;
                }
            }
        }
    }
    out
}

/// Resolve the LIVE pending placeholder of a chat: the newest pending row
/// that is not a queued marker (those belong to prompts still waiting in the
/// FIFO queue and are owned by [`App::dequeue_next_for`]). Terminal outcomes
/// (`result`, `error`, cancel) must never touch queued markers.
/// Resolve the live pending placeholder of `chat` into a finished message.
///
/// `model` (feat_agent_model_label) is stamped ONLY onto this one message at
/// result-resolution time: replies are labelled per-message, so future
/// per-message model switching can never mislabel an earlier answer.
fn resolve_live_placeholder(chat: &mut Chat, markdown: String, model: Option<String>) {
    if let Some(row) = chat
        .messages
        .iter_mut()
        .rev()
        .find(|m| m.pending && !is_queued_marker(m))
    {
        row.pending = false;
        row.markdown = markdown;
        row.model = model;
    }
}
/// fix_ack_silent_absorb: is this answer content invisible-by-contract?
///
/// True when the content is empty/whitespace-only, or consists ONLY of the
/// [`ACK_MARKER`] — exact, repeated, or embedded in whitespace. Content that
/// contains the marker PLUS any other text is a real (if odd) answer and is
/// shown raw: marker cleanup is the backend's job, not the TUI's.
pub fn is_invisible_result(markdown: &str) -> bool {
    let mut rest = markdown.trim();
    loop {
        if rest.is_empty() {
            return true;
        }
        match rest.strip_prefix(ACK_MARKER) {
            Some(after) => rest = after.trim_start(),
            None => return false,
        }
    }
}

/// Remove the live pending placeholder row WITHOUT leaving an answer behind
/// (fix_ack_silent_absorb). Targets the same row [`resolve_live_placeholder`]
/// would — the last pending, non-queued placeholder — but drops it so an
/// absorbed (blank / pure-ACK) result leaves no bubble at all. A no-op when
/// no live placeholder exists (stray result).
fn drop_live_pending_placeholder(chat: &mut Chat) {
    if let Some(row) = chat
        .messages
        .iter_mut()
        .rposition(|m| m.pending && !is_queued_marker(m))
    {
        chat.messages.remove(row);
    }
}

/// Does a numeric [`BackendEvent`] id refer to the tracked protocol request?
/// The live glue task derives event ids from the protocol UUID
/// (see [`crate::live::submitted_event_id`]).
fn event_matches_request(event_id: u64, tracked_request_id: &str) -> bool {
    event_id == crate::live::wire_thread_id(tracked_request_id) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    /// Regression test for the review finding: PgUp must move one page up
    /// from the bottom, not to the top.
    #[test]
    fn pgup_moves_one_page_up_from_bottom() {
        let mut app = App::new(Vec::new());
        app.scroll_up(20);
        assert_eq!(app.scroll, 20, "first PgUp detaches by one page");
        app.scroll_up(20);
        assert_eq!(app.scroll, 40, "subsequent PgUps move further up");
        app.scroll_down(20);
        assert_eq!(app.scroll, 20, "PgDn moves back down");
        app.scroll_down(20);
        assert_eq!(app.scroll, 0, "returning to 0 re-enables follow-bottom");
        assert!(app.at_bottom());
    }

    #[test]
    fn scroll_up_clamps_at_ceiling() {
        let mut app = App::new(Vec::new());
        app.scroll_up(u16::MAX);
        assert_eq!(app.scroll, u16::MAX / 2);
    }

    // ---- bugfix_ctrl_l_screen_clear -----------------------------------------

    #[test]
    fn request_clear_screen_resets_scroll_to_follow_bottom() {
        let mut app = App::new(Vec::new());
        app.scroll_up(37);
        assert!(!app.at_bottom());
        assert!(!app.clear_screen_requested, "precondition: no wipe pending");

        app.request_clear_screen();

        assert_eq!(
            app.scroll, 0,
            "^L snaps the chat view back to follow-bottom"
        );
        assert!(app.at_bottom());
        assert!(
            app.clear_screen_requested,
            "wipe intent recorded for the event loop"
        );
    }

    #[test]
    fn take_clear_screen_request_is_one_shot() {
        let mut app = App::new(Vec::new());
        app.request_clear_screen();
        assert!(app.take_clear_screen_request());
        assert!(
            !app.take_clear_screen_request(),
            "a second consume must not re-wipe the screen"
        );
    }

    // ---- helpers ---------------------------------------------------------

    fn app_with_chats(n: usize) -> App {
        let chats = (0..n)
            .map(|i| Chat::new(format!("chat-{i}")))
            .collect::<Vec<_>>();
        App::new(chats)
    }

    fn type_in(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.input.input(tui_textarea::Input {
                key: tui_textarea::Key::Char(ch),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
    }

    /// Simulate the Ctrl+R hotkey exactly as `handle_key` receives it.
    fn press_ctrl_r(app: &mut App) {
        app.begin_rename();
    }

    fn submit_text(app: &mut App, text: &str) -> Submitted {
        type_in(app, text);
        let submitted = app.take_input().expect("prompt taken");
        app.begin_request(&submitted);
        submitted
    }

    /// Numeric event id the live glue task would stamp for this request id.
    fn event_id_of(request_id: &str) -> u64 {
        crate::live::wire_thread_id(request_id) as u64
    }

    /// Deliver a terminal Result event to `index`'s tracked request id.
    fn finish_chat(app: &mut App, index: usize) {
        finish_chat_with_model(app, index, None);
    }

    /// [`finish_chat`] with a model label (feat_agent_model_label).
    fn finish_chat_with_model(app: &mut App, index: usize, model: Option<&str>) {
        let request_id = app.chats[index].lifecycle.request_id().unwrap().to_owned();
        let thread_id = app.chats[index].id.clone();
        app.apply_backend_event(BackendEvent::Result {
            request_id: event_id_of(&request_id),
            markdown: "**done**".into(),
            thread_id,
            model: model.map(str::to_owned),
        });
    }

    // ---- chat_state: creation / switching --------------------------------

    #[test]
    fn new_chat_appends_selects_and_gets_fresh_uuid() {
        let mut app = app_with_chats(2);
        let existing_ids: Vec<String> = app.chats.iter().map(|c| c.id.clone()).collect();
        app.new_chat();
        assert_eq!(app.chats.len(), 3);
        assert_eq!(app.active, 2, "the fresh chat must be selected");
        assert_eq!(app.chat_title(), "New chat 3");
        let fresh = &app.chats[2];
        assert!(!fresh.id.is_empty());
        assert!(!existing_ids.contains(&fresh.id), "thread ids are unique");
        assert!(fresh.messages.is_empty());
        assert_eq!(fresh.lifecycle, ChatLifecycle::Idle);
        assert!(fresh.queue.is_empty());
    }

    #[test]
    fn switching_chats_keeps_history_and_selection() {
        let mut app = app_with_chats(3);
        app.select_next();
        assert_eq!(app.active, 1);
        assert_eq!(app.chats[0].name, "chat-0", "history untouched");
        app.select_prev();
        assert_eq!(app.active, 0);
        // saturating at the top edge
        app.select_prev();
        assert_eq!(app.active, 0);
    }

    // ---- feat_focus_panes: Chat ↔ Sidebar focus state ----------------------

    /// THE round-trip criterion: Ctrl+T's state flip is a pure pane switch —
    /// toggling twice lands back on Chat; nothing about the chat list
    /// (selection, scroll, count) moves along.
    #[test]
    fn toggle_focus_round_trips_chat_and_sidebar() {
        let mut app = app_with_chats(3);
        app.scroll = 42;
        assert_eq!(app.focus, Focus::Chat, "default focus");

        app.toggle_focus();
        assert_eq!(app.focus, Focus::Sidebar);

        app.toggle_focus();
        assert_eq!(app.focus, Focus::Chat, "toggle twice round-trips");
        assert_eq!(app.active, 0, "selection untouched by focus flips");
        assert_eq!(app.scroll, 42, "scroll untouched by focus flips");
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.chats.len(), 3);
    }

    /// Focus is pane-level state, independent of the list contents: zero and
    /// single-chat lists toggle too (no navigation semantics involved — no
    /// toast, popup or quit ever fires).
    #[test]
    fn toggle_focus_is_independent_of_list_size() {
        for n in [0usize, 1usize] {
            let mut app = App::new((0..n).map(|i| Chat::new(format!("chat-{i}"))).collect());
            app.toggle_focus();
            assert_eq!(app.focus, Focus::Sidebar, "n={n}");
            app.toggle_focus();
            assert_eq!(app.focus, Focus::Chat, "n={n}");
            assert!(!app.should_quit);
            assert!(app.status_message.is_none());
            assert!(app.error_popup.is_none());
        }
    }

    /// Arrows keep CLAMPING after the Ctrl+T rewrite — focus toggling never
    /// leaks into select_next/select_prev (regression pinned when the old
    /// wrap-cycling method was removed).
    #[test]
    fn toggle_focus_does_not_change_arrow_clamping() {
        let mut app = app_with_chats(3);
        app.active = 2;
        app.select_next();
        assert_eq!(app.active, 2, "select_next clamps at the last thread");
        app.select_prev();
        app.select_prev();
        assert_eq!(app.active, 0, "select_prev clamps at the first thread");
        app.select_prev();
        assert_eq!(app.active, 0, "select_prev stays clamped at the top");

        // And the toggle itself still works from anywhere on the list.
        app.toggle_focus();
        assert_eq!(app.focus, Focus::Sidebar);
        app.active = 2;
        app.select_next();
        assert_eq!(app.active, 2, "clamping holds while Sidebar focused");
    }

    /// Modal closers reset focus to Chat: opening a modal above a Sidebar-
    /// focused UI and closing it hands the keyboard back to the editor,
    /// whichever close path was taken.
    #[test]
    fn closing_rename_resets_focus_to_chat_on_commit_and_cancel() {
        // Commit path.
        let mut app = app_with_chats(1);
        app.focus = Focus::Sidebar;
        press_ctrl_r(&mut app);
        assert!(matches!(app.mode, Mode::Renaming { .. }));
        app.rename_push('x');
        assert!(app.commit_rename());
        assert_eq!(app.focus, Focus::Chat);
        assert!(app.mode.is_normal());

        // Cancel path.
        let mut app = app_with_chats(1);
        app.focus = Focus::Sidebar;
        press_ctrl_r(&mut app);
        assert!(matches!(app.mode, Mode::Renaming { .. }));
        assert!(app.cancel_rename());
        assert_eq!(app.focus, Focus::Chat);
    }

    #[test]
    fn closing_search_popups_resets_focus_to_chat() {
        // In-thread search: cancel and jump paths.
        let mut app = app_with_chats(1);
        submit_text(&mut app, "hello world");
        app.focus = Focus::Sidebar;
        press_ctrl_r_cancel_search_all_paths_helper(&mut app);
        assert_eq!(app.focus, Focus::Chat);

        // Global search: cancel path.
        let mut app = app_with_chats(1);
        app.focus = Focus::Sidebar;
        app.begin_search_all();
        assert!(matches!(app.mode, Mode::SearchingAll { .. }));
        assert!(app.cancel_search_all());
        assert_eq!(app.focus, Focus::Chat);

        // Global search: jump path (activate thread + close).
        let mut app = app_with_chats(2);
        submit_text(&mut app, "target needle");
        finish_chat(&mut app, 0);
        app.focus = Focus::Sidebar;
        app.begin_search_all();
        app.search_all_push('n');
        app.search_all_push('e');
        assert!(
            !app.search_all_matches().is_empty(),
            "precondition: a match exists"
        );
        assert!(app.jump_to_selected_all());
        assert_eq!(app.focus, Focus::Chat);
    }

    /// Helper shared by `closing_search_popups_resets_focus_to_chat`: walks
    /// the in-thread search cancel + jump paths from a Sidebar-focused start.
    fn press_ctrl_r_cancel_search_all_paths_helper(app: &mut App) {
        app.begin_search();
        assert!(matches!(app.mode, Mode::Searching { .. }));
        for ch in "hello".chars() {
            app.search_push(ch);
        }
        assert!(!app.search_matches().is_empty(), "precondition: matches");
        // Jump path…
        assert!(app.jump_to_selected());
        assert_eq!(app.focus, Focus::Chat);
        // …then reopen for the cancel path.
        app.focus = Focus::Sidebar;
        app.begin_search();
        assert!(matches!(app.mode, Mode::Searching { .. }));
        assert!(app.cancel_search());
        assert_eq!(app.focus, Focus::Chat);
    }

    #[test]
    fn closing_delete_confirm_resets_focus_to_chat() {
        // Cancel path.
        let mut app = app_with_chats(2);
        app.focus = Focus::Sidebar;
        app.begin_delete_confirm();
        assert_eq!(app.mode, Mode::ConfirmDelete);
        assert!(app.cancel_delete());
        assert_eq!(app.focus, Focus::Chat);

        // Confirm path (delete leaves ≥1 chat behind).
        let mut app = app_with_chats(2);
        app.focus = Focus::Sidebar;
        app.begin_delete_confirm();
        assert_eq!(app.mode, Mode::ConfirmDelete);
        assert!(app.confirm_delete().is_some());
        assert_eq!(app.focus, Focus::Chat);

        // Confirm path into the clean EMPTY state (deleted last chat) —
        // focus still returns to the editor even with no chats left.
        let mut app = app_with_chats(1);
        app.focus = Focus::Sidebar;
        app.begin_delete_confirm();
        assert!(app.confirm_delete().is_some());
        assert!(app.chats.is_empty());
        assert_eq!(app.focus, Focus::Chat);
    }

    #[test]
    fn dismissing_error_popup_resets_focus_to_chat() {
        let mut app = app_with_chats(1);
        app.focus = Focus::Sidebar;
        app.show_error("boom");
        assert!(app.error_popup.is_some());
        app.dismiss_error();
        assert!(app.error_popup.is_none());
        assert_eq!(app.focus, Focus::Chat);
    }

    /// ^N new-chat is editor-bound (feat_focus_panes): creating a thread
    /// always lands focus back on Chat, whether the chord came from either
    /// pane.
    #[test]
    fn new_chat_lands_focus_on_chat() {
        let mut app = app_with_chats(2);
        app.focus = Focus::Sidebar;
        app.new_chat();
        assert_eq!(app.focus, Focus::Chat);
        assert_eq!(app.active, 2, "new chat is selected");
    }

    // ---- lifecycle (per chat) --------------------------------------------

    #[test]
    fn lifecycle_idle_awaiting_running_result_idle() {
        let mut app = App::new(vec![Chat::new("lifecycle")]);
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);

        let submitted = submit_text(&mut app, "explain lifetimes");
        assert_eq!(submitted.prompt, "explain lifetimes");
        assert!(!submitted.thread_id.is_empty());
        assert!(!submitted.request_id.is_empty());
        assert_eq!(
            app.active_lifecycle(),
            &ChatLifecycle::Awaiting {
                request_id: submitted.request_id.clone()
            }
        );
        assert_eq!(app.chats[0].messages.len(), 2, "user + pending assistant");

        let req_event_id = event_id_of(&submitted.request_id);
        let thread_id = submitted.thread_id.clone();
        app.apply_backend_event(BackendEvent::Running {
            request_id: req_event_id,
            thread_id,
        });
        assert!(matches!(
            app.active_lifecycle(),
            ChatLifecycle::Running { .. }
        ));

        finish_chat(&mut app, 0);
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 2);
        assert!(!msgs[1].pending);
        assert_eq!(msgs[1].markdown, "**done**");
    }

    #[test]
    fn lifecycle_error_returns_to_idle_with_error_message() {
        let mut app = App::new(vec![Chat::new("errors")]);
        let submitted = submit_text(&mut app, "boom");
        app.apply_backend_event(BackendEvent::Error {
            request_id: event_id_of(&submitted.request_id),
            message: "backend exploded".into(),
            thread_id: Some(app.chats[0].id.clone()),
        });
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        assert_eq!(
            app.chats[0].messages[1].markdown,
            "**Error:** backend exploded"
        );
    }

    #[test]
    fn empty_input_is_never_submitted_and_state_stays_idle() {
        let mut app = app_with_chats(1);
        app.input.input(tui_textarea::Input::default());
        assert!(app.take_input().is_none(), "whitespace-only is rejected");
        assert!(!app.is_busy());
        assert!(app.active_request_id().is_none());
        assert!(app.chats[0].messages.is_empty());
    }

    // ---- per-thread async: same chat enqueues, other chats stay free -----

    #[test]
    fn same_chat_second_submit_enqueues_fifo_bubble() {
        let mut app = app_with_chats(1);
        let first = submit_text(&mut app, "first");

        // Second Enter mid-flight enqueues instead of rejecting…
        type_in(&mut app, "second");
        assert!(app.take_input().is_none(), "busy chat returns None");
        // …the buffer was consumed and the queue holds the prompt…
        assert!(app.input.lines().iter().all(|l| l.is_empty()));
        assert_eq!(app.active_queue_len(), 1);
        // …with visible bubbles: user message + queued marker.
        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 4, "user+pending, then user+queued marker");
        assert_eq!(msgs[2].markdown, "second");
        assert_eq!(msgs[2].role, Role::User);
        assert!(msgs[3].pending, "queued marker is a pending row");
        assert_eq!(msgs[3].markdown, "\u{23f3} queued (#1)");
        // Dequeue swaps the marker for the real pending placeholder.
        let thread = app.chats[0].id.clone();
        finish_chat(&mut app, 0);
        let _next = app.dequeue_next_for(&thread).expect("drain");
        // NOTE: no begin_request here — dequeue_next_for already started the
        // request (lifecycle + pending row); the loop only sends the bundle.
        assert_eq!(app.chats[0].messages[3].markdown, "");
        assert!(app.chats[0].messages[3].pending);
        // In-flight request moved to the drained prompt's id.
        assert_ne!(app.active_request_id(), Some(first.request_id.as_str()));
    }

    /// Terminal events must resolve ONLY the live pending placeholder —
    /// never the "⏳ queued" markers belonging to prompts still waiting in
    /// the FIFO queue (those are swapped by [`App::dequeue_next_for`]).
    #[test]
    fn resolve_live_placeholder_skips_queued_markers() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "first");
        type_in(&mut app, "second");
        assert!(app.take_input().is_none(), "busy chat enqueues");

        // Force a queued-marker row BEHIND the live pending placeholder to
        // prove resolution targets the placeholder, not the newest pending
        // row (the regression this helper was written for).
        let chat = &mut app.chats[0];
        chat.messages.push(Message::user("third"));
        chat.queue.push_back("third".into());
        chat.messages.push(Message::assistant_queued(2));
        assert_eq!(chat.messages.len(), 6);
        // [user, pending(live), user, marker#1, user, marker#2]
        assert!(chat.messages[3].markdown.starts_with("\u{23f3} queued"));

        // Deliver the terminal Result for the live request.
        finish_chat(&mut app, 0);

        let msgs = &app.chats[0].messages;
        assert!(!msgs[1].pending, "live placeholder resolved");
        assert_eq!(msgs[1].markdown, "**done**");
        assert!(
            msgs[3].pending && msgs[3].markdown == "\u{23f3} queued (#1)",
            "queued marker #1 must survive a terminal event"
        );
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        assert!(
            msgs[5].pending && msgs[5].markdown == "\u{23f3} queued (#2)",
            "queued marker #2 must survive a terminal event"
        );
    }

    // ---- feat_agent_model_label: per-message labelling --------------------

    /// THE per-message contract: two consecutive replies produced by
    /// DIFFERENT models each carry their own label — the second resolution
    /// must never overwrite the first message's metadata (future
    /// per-message model switching must not mislabel earlier answers).
    #[test]
    fn consecutive_results_label_their_own_messages() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "first prompt");
        finish_chat_with_model(&mut app, 0, Some("glm-5.2"));

        // Second round-trip in the same chat, different model this time.
        submit_text(&mut app, "second prompt");
        finish_chat_with_model(&mut app, 0, Some("kimi-k2.7"));

        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 4);
        assert_eq!(
            msgs[1].model.as_deref(),
            Some("glm-5.2"),
            "first answer keeps its own label"
        );
        assert_eq!(
            msgs[3].model.as_deref(),
            Some("kimi-k2.7"),
            "second answer carries the new label"
        );
    }

    /// Fallback: a fieldless result (model = None) resolves to a message
    /// with NO metadata — rendered later as the plain `● Chibi` header.
    #[test]
    fn fieldless_result_resolves_to_plain_unlabelled_message() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "old backend");
        finish_chat_with_model(&mut app, 0, None);

        let msgs = &app.chats[0].messages;
        assert!(!msgs[1].pending);
        assert_eq!(msgs[1].markdown, "**done**");
        assert_eq!(msgs[1].model_label(), None);
    }

    // ---- feat_status_line: strip state + segments --------------------------

    /// Default hidden; ^O flips visibility round-trip.
    #[test]
    fn status_strip_starts_hidden_and_toggles_round_trip() {
        let mut app = app_with_chats(1);
        assert!(
            !app.status_strip_visible,
            "strip must start hidden (task contract)"
        );
        app.toggle_status_strip();
        assert!(app.status_strip_visible);
        app.toggle_status_strip();
        assert!(!app.status_strip_visible);
    }

    /// View state like Focus: opening and closing every modal family must
    /// leave the visibility flag untouched.
    #[test]
    fn status_strip_visibility_survives_modal_open_close() {
        let mut app = app_with_chats(1);
        app.toggle_status_strip();

        app.begin_search();
        assert!(matches!(app.mode, Mode::Searching { .. }));
        assert!(
            app.status_strip_visible,
            "opening the search popup must not hide the strip"
        );
        assert!(app.cancel_search());
        assert!(app.mode.is_normal());
        assert!(
            app.status_strip_visible,
            "closing a modal must not reset it"
        );

        app.begin_delete_confirm();
        assert!(matches!(app.mode, Mode::ConfirmDelete));
        assert!(app.status_strip_visible);
        assert!(app.cancel_delete());
        assert!(app.status_strip_visible);

        app.begin_log_viewer();
        assert!(matches!(app.mode, Mode::LogViewer { .. }));
        assert!(app.status_strip_visible);
        assert!(app.close_log_viewer());
        assert!(app.status_strip_visible, "log viewer close must keep it");
    }

    /// The cwd segment is the workspace root's BASENAME (long paths must not
    /// leak into the strip); pathless roots degrade to the raw string.
    #[test]
    fn status_cwd_is_the_workspace_basename() {
        let mut app = app_with_chats(1);
        assert_eq!(app.status_cwd(), None, "unwired root yields None");

        app.workspace_root = Some("/Users/sergio/Develop/personal/chibi-tui".into());
        assert_eq!(app.status_cwd(), Some("chibi-tui"));

        app.workspace_root = Some("/Users/sergio/Develop/".into());
        assert_eq!(
            app.status_cwd(),
            Some("Develop"),
            "trailing slash tolerated"
        );

        app.workspace_root = Some("/".into());
        assert_eq!(app.status_cwd(), Some("/"), "pathless root degrades to raw");

        app.workspace_root = Some(".".into());
        assert_eq!(app.status_cwd(), Some("."));
    }

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
        finish_chat_with_model(&mut app, 0, Some("glm-5.2"));

        app.select_next();
        submit_text(&mut app, "chat1");
        finish_chat_with_model(&mut app, 1, Some("kimi-k2.7"));

        assert_eq!(app.active_model_label(), Some("kimi-k2.7"));
        app.select_prev();
        assert_eq!(
            app.active_model_label(),
            Some("glm-5.2"),
            "switching back must re-label from chat 0's own last model"
        );
    }

    // ---- fix_ack_silent_absorb: invisible ACK / blank answers --------------

    /// [`finish_chat_with_model`] with arbitrary answer content.
    fn finish_chat_with_content(app: &mut App, index: usize, markdown: &str) {
        let request_id = app.chats[index].lifecycle.request_id().unwrap().to_owned();
        let thread_id = app.chats[index].id.clone();
        app.apply_backend_event(BackendEvent::Result {
            request_id: event_id_of(&request_id),
            markdown: markdown.to_owned(),
            thread_id,
            model: None,
        });
    }

    /// Predicate contract: whitespace-only and pure-marker contents are
    /// invisible; anything that also contains other text — even alongside
    /// the marker — is a real answer.
    #[test]
    fn invisible_result_predicate_matches_blank_and_pure_ack_only() {
        for blank in ["", "   ", " \n\t "] {
            assert!(is_invisible_result(blank), "blank {blank:?} must absorb");
        }
        let doubled = format!("{ACK_MARKER}{ACK_MARKER}");
        let spaced = format!(" {ACK_MARKER}  {ACK_MARKER} ");
        for ack in [
            ACK_MARKER,
            "  <chibi>ACK</chibi>  ",
            "\n<chibi>ACK</chibi>\n",
            doubled.as_str(),
            spaced.as_str(),
        ] {
            assert!(is_invisible_result(ack), "pure ack {ack:?} must absorb");
        }
        for mixed in [
            "real answer",
            &format!("{ACK_MARKER} partial text"),
            &format!("text {ACK_MARKER}"),
            &format!("{ACK_MARKER}ish"), // marker must match whole segments only
        ] {
            assert!(!is_invisible_result(mixed), "mixed {mixed:?} must show");
        }
    }

    /// A blank result leaves NO assistant bubble: the pending placeholder is
    /// dropped, the lifecycle resolves to Idle (spinner stops cleanly and
    /// re-arms on the next prompt), and no error popup/toast fires.
    #[test]
    fn blank_result_is_absorbed_without_bubble() {
        for content in ["", "   \n\t "] {
            let mut app = app_with_chats(1);
            submit_text(&mut app, "question");
            assert_eq!(app.chats[0].messages.len(), 2, "user + pending");

            finish_chat_with_content(&mut app, 0, content);

            let msgs = &app.chats[0].messages;
            assert_eq!(msgs.len(), 1, "placeholder dropped, no bubble");
            assert_eq!(msgs[0].role, Role::User);
            assert!(msgs.iter().all(|m| !m.pending), "no stuck spinner row");
            assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
            assert!(app.error_popup.is_none(), "blank result is not an error");
        }
    }

    /// A pure-ACK answer (exact marker, whitespace-wrapped, repeated) behaves
    /// exactly like a blank one: absorbed invisibly, clean Idle, no error.
    #[test]
    fn pure_ack_result_is_absorbed_without_bubble() {
        let wrapped = format!("  {ACK_MARKER}\n");
        let doubled = format!("{ACK_MARKER}{ACK_MARKER}");
        let spaced = format!(" {ACK_MARKER}  {ACK_MARKER} ");
        for content in [
            ACK_MARKER,
            wrapped.as_str(),
            doubled.as_str(),
            spaced.as_str(),
        ] {
            let mut app = app_with_chats(1);
            submit_text(&mut app, "question");
            finish_chat_with_content(&mut app, 0, content);

            let msgs = &app.chats[0].messages;
            assert_eq!(msgs.len(), 1, "pure ACK {content:?} leaves no bubble");
            assert!(msgs.iter().all(|m| !m.pending));
            assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
            assert!(app.error_popup.is_none());
        }
    }

    /// Content that CONTAINS the marker but also real text is a real answer:
    /// shown as-is, raw — the TUI does not clean up partial markers (that is
    /// the backend's job).
    #[test]
    fn mixed_marker_content_is_shown_raw() {
        for content in [
            format!("before {ACK_MARKER} after"),
            format!("{ACK_MARKER} partial text"),
            format!("{ACK_MARKER}ish"),
        ] {
            let mut app = app_with_chats(1);
            submit_text(&mut app, "question");
            finish_chat_with_content(&mut app, 0, &content);

            let msgs = &app.chats[0].messages;
            assert_eq!(msgs.len(), 2, "mixed content renders a bubble");
            assert!(!msgs[1].pending);
            assert_eq!(msgs[1].markdown, content, "shown raw, unmodified");
            assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        }
    }

    /// Queue interplay: absorbing an ACK result must still hand the FIFO to
    /// the drain step — the next queued prompt sends (mirrors the event-loop
    /// glue: terminal Result → QueueDrain → dequeue_next_for).
    #[test]
    fn absorbed_result_still_drains_queued_prompt() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "one");
        type_in(&mut app, "two");
        assert!(app.take_input().is_none(), "busy chat enqueues");

        finish_chat_with_content(&mut app, 0, ACK_MARKER);

        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        assert_eq!(app.active_queue_len(), 1);

        // The drain step owned by the event loop after QueueDrain.
        let thread = app.chats[0].id.clone();
        let next = app
            .dequeue_next_for(&thread)
            .expect("queued prompt must send after ACK absorb");
        assert_eq!(next.prompt, "two");
        assert!(matches!(
            app.chats[0].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));

        // Shape after the swap: [user(one), user(two), pending(live)] — the
        // absorbed round added no bubble and the queued marker became the
        // live placeholder.
        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 3);
        assert!(msgs[2].pending && !is_queued_marker(&msgs[2]));
    }

    /// A background chat's absorbed result stays invisible there too and
    /// must not disturb the ACTIVE chat's state or view.
    #[test]
    fn absorbed_result_in_background_chat_is_invisible() {
        let mut app = app_with_chats(2);
        app.select_next(); // active = chat 1 (the "background" one)
        submit_text(&mut app, "background question");
        app.select_prev(); // foreground chat 0 stays empty

        finish_chat_with_content(&mut app, 1, ACK_MARKER);

        assert_eq!(app.chats[1].messages.len(), 1, "no bubble in bg chat");
        assert!(app.chats[1].messages.iter().all(|m| !m.pending));
        assert_eq!(app.chats[1].lifecycle, ChatLifecycle::Idle);
        assert!(app.chats[0].messages.is_empty(), "foreground untouched");
        assert!(app.error_popup.is_none());
    }

    #[test]
    fn fifo_queue_drains_in_order_on_terminal_events() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "one");
        for text in ["two", "three", "four"] {
            type_in(&mut app, text);
            assert!(app.take_input().is_none());
        }
        assert_eq!(app.active_queue_len(), 3);

        // Terminal event #1 → next queued prompt becomes the new request.
        finish_chat(&mut app, 0);
        let thread = app.chats[0].id.clone();
        let next = app.dequeue_next_for(&thread).expect("queued #1");
        assert_eq!(next.prompt, "two", "strict FIFO order");
        assert!(matches!(
            app.chats[0].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));
        assert_eq!(app.active_queue_len(), 2);

        // Terminal event #2 → next queued prompt.
        finish_chat(&mut app, 0);
        let next = app.dequeue_next_for(&thread).expect("queued #2");
        assert_eq!(next.prompt, "three");
        assert_eq!(app.active_queue_len(), 1);

        // Terminal event #3 → last queued prompt.
        finish_chat(&mut app, 0);
        let next = app.dequeue_next_for(&thread).expect("queued #3");
        assert_eq!(next.prompt, "four");
        assert_eq!(app.active_queue_len(), 0);

        // Terminal event #4 → idle, nothing left to dequeue.
        finish_chat(&mut app, 0);
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        assert!(app.dequeue_next_for(&thread).is_none());

        // Queued markers were swapped in place for pending rows: display
        // order stays user → answer pairs in strict FIFO sequence.
        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 8, "no duplicate bubbles from the drain path");
        assert_eq!(msgs[0].markdown, "one");
        assert_eq!(msgs[1].markdown, "**done**");
        assert!(!msgs[1].pending);
        assert_eq!(msgs[2].markdown, "two");
        assert_eq!(msgs[3].markdown, "**done**");
        assert!(!msgs[3].pending);
        assert_eq!(msgs[4].markdown, "three");
        assert_eq!(msgs[5].markdown, "**done**");
        assert!(!msgs[5].pending);
        assert_eq!(msgs[6].markdown, "four");
        assert_eq!(msgs[7].markdown, "**done**");
        assert!(!msgs[7].pending);
    }

    #[test]
    fn other_chat_submit_works_while_first_chat_is_running() {
        let mut app = app_with_chats(2);
        let first = submit_text(&mut app, "long running in A");

        // Switch to chat B and submit while A runs — never blocked.
        app.select_next();
        let second = submit_text(&mut app, "parallel in B");
        assert_ne!(first.thread_id, second.thread_id);
        assert!(matches!(
            app.chats[0].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));
        assert!(matches!(
            app.chats[1].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));
        assert!(app.chats[0].queue.is_empty(), "A's queue untouched");
        assert!(app.chats[1].queue.is_empty(), "B started immediately");

        // B's answer lands first and only touches B.
        let second_req = second.request_id.clone();
        app.apply_backend_event(BackendEvent::Result {
            request_id: event_id_of(&second_req),
            markdown: "B done".into(),
            thread_id: second.thread_id.clone(),
            model: None,
        });
        assert_eq!(app.chats[0].messages.len(), 2, "A still pending");
        assert_eq!(app.chats[1].messages[1].markdown, "B done");
        assert_eq!(app.chats[1].lifecycle, ChatLifecycle::Idle);
        assert!(
            matches!(app.chats[0].lifecycle, ChatLifecycle::Awaiting { .. }),
            "A keeps running across B's completion"
        );
    }

    #[test]
    fn background_chat_drains_its_own_queue_when_not_selected() {
        let mut app = app_with_chats(2);
        submit_text(&mut app, "A1");
        type_in(&mut app, "A2");
        assert!(app.take_input().is_none());

        // Leave for chat B; A finishes in the background and its queue moves.
        app.select_next();
        finish_chat(&mut app, 0);
        let thread_a = app.chats[0].id.clone();
        let next = app
            .dequeue_next_for(&thread_a)
            .expect("A's queued prompt survives being backgrounded");
        assert_eq!(next.prompt, "A2");
        assert_eq!(next.thread_id, app.chats[0].id);
    }

    #[test]
    fn spinner_reflects_active_chat_only() {
        let mut app = app_with_chats(2);
        submit_text(&mut app, "background work"); // chat 0 goes busy

        // Switch to idle chat 1: spinner must hide even though chat 0 runs.
        app.select_next();
        assert!(!app.is_busy(), "active chat idle ⇒ not busy");
        assert!(app.any_busy(), "some background chat still runs");

        // Back to chat 0: busy again.
        app.select_prev();
        assert!(app.is_busy());
    }

    // ---- cancel semantics -------------------------------------------------

    #[test]
    fn cancel_active_reports_inflight_request_only() {
        let mut app = app_with_chats(1);
        assert!(app.cancel_active().is_none(), "nothing in flight yet");

        let submitted = submit_text(&mut app, "to cancel");
        let (request_id, thread_id) = app.cancel_active().expect("cancel target present");
        assert_eq!(request_id, submitted.request_id);
        assert_eq!(thread_id, app.chats[0].id);
        // UI stays busy: only the backend's cancelled-error resolves it.
        assert!(matches!(
            app.chats[0].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));
    }

    #[test]
    fn cancel_keeps_queue_and_drains_it_afterwards() {
        let mut app = app_with_chats(1);
        let inflight = submit_text(&mut app, "in flight");
        type_in(&mut app, "queued behind cancel");
        assert!(app.take_input().is_none());
        assert_eq!(app.active_queue_len(), 1);

        // Cancel targets ONLY the in-flight request id.
        let (cancel_id, _) = app.cancel_active().expect("cancel target");
        assert_eq!(cancel_id, inflight.request_id);

        // Simulate the cancelled-error terminal event for that request.
        app.apply_backend_event(BackendEvent::Error {
            request_id: event_id_of(&inflight.request_id),
            message: "Cancelled".into(),
            thread_id: Some(app.chats[0].id.clone()),
        });
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        // Queue survived the cancel…
        assert_eq!(app.active_queue_len(), 1);
        // …and drains normally.
        let thread = app.chats[0].id.clone();
        let next = app.dequeue_next_for(&thread).expect("survivor");
        assert_eq!(next.prompt, "queued behind cancel");
        assert_eq!(
            app.chats[0].messages[1].markdown, "**Error:** Cancelled",
            "cancelled placeholder resolved by the backend error frame"
        );
    }

    #[test]
    fn cancel_targets_active_chat_not_background_chats() {
        let mut app = app_with_chats(2);
        let bg = submit_text(&mut app, "background"); // chat 0 busy

        app.select_next();
        assert!(
            app.cancel_active().is_none(),
            "active chat B is idle: Ctrl+C means quit, never touches A"
        );
        // A's request remains intact and cancellable from its own view.
        app.select_prev();
        let (request_id, thread_id) = app.cancel_active().expect("A's cancel target");
        assert_eq!(request_id, bg.request_id);
        assert_eq!(thread_id, app.chats[0].id);
    }

    #[test]
    fn stray_events_after_completion_are_ignored() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "one shot");
        app.apply_backend_event(BackendEvent::Result {
            request_id: event_id_of(&submitted.request_id),
            markdown: "answer".into(),
            thread_id: submitted.thread_id.clone(),
            model: None,
        });
        // Late duplicate / stale event for an already-finished request:
        app.apply_backend_event(BackendEvent::Error {
            request_id: event_id_of(&submitted.request_id),
            message: "late failure".into(),
            thread_id: Some(submitted.thread_id.clone()),
        });
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        let msgs = &app.chats[0].messages;
        assert_eq!(msgs[1].markdown, "answer", "result not overwritten");
    }

    #[test]
    fn events_for_unknown_threads_are_ignored() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "mine");
        app.apply_backend_event(BackendEvent::Result {
            request_id: 12345,
            markdown: "not mine".into(),
            thread_id: "00000000-0000-0000-0000-00000000dead".into(),
            model: None,
        });
        assert!(matches!(
            app.chats[0].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));
        assert_eq!(app.chats[0].messages.len(), 2);
    }

    #[test]
    fn thread_ids_are_unique_across_many_new_chats() {
        let mut app = app_with_chats(0);
        let mut ids = std::collections::HashSet::new();
        for _ in 0..50 {
            app.new_chat();
            ids.insert(app.chats.last().unwrap().id.clone());
        }
        assert_eq!(ids.len(), 50, "every new chat gets a unique thread id");
    }

    // ---- ux_polish: error popup ------------------------------------------

    #[test]
    fn error_popup_shown_replaced_and_dismissed() {
        let mut app = app_with_chats(1);
        assert!(app.error_popup.is_none());

        app.show_error("spawn failed");
        assert_eq!(app.error_popup.as_ref().unwrap().message, "spawn failed");

        // A newer failure replaces the stale one.
        app.show_error("broken pipe");
        assert_eq!(app.error_popup.as_ref().unwrap().message, "broken pipe");

        app.dismiss_error();
        assert!(app.error_popup.is_none());
        // Dismissing twice is a no-op.
        app.dismiss_error();
        assert!(app.error_popup.is_none());
    }

    // ---- ux_polish: clear input (Ctrl+L) ---------------------------------

    #[test]
    fn clear_input_empties_buffer_and_restores_placeholder() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "to be cleared");
        app.clear_input();
        assert!(
            app.input.lines().iter().all(|l| l.is_empty()),
            "buffer must be empty after Ctrl+L"
        );
        assert_eq!(
            app.input.placeholder_text(),
            "Type a message…  (\u{23ce} send)"
        );
    }

    #[test]
    fn clear_input_on_empty_buffer_is_a_no_op() {
        let mut app = app_with_chats(1);
        app.clear_input();
        assert!(app.input.lines().iter().all(|l| l.is_empty()));
    }

    #[test]
    fn is_busy_tracks_active_chat_lifecycle() {
        let mut app = app_with_chats(1);
        assert!(!app.is_busy());
        submit_text(&mut app, "work");
        assert!(app.is_busy());
        finish_chat(&mut app, 0);
        assert!(!app.is_busy());
    }

    #[test]
    fn any_busy_spans_all_chats() {
        let mut app = app_with_chats(2);
        assert!(!app.any_busy());
        submit_text(&mut app, "work");
        assert!(app.any_busy());
        finish_chat(&mut app, 0);
        assert!(!app.any_busy());
    }

    // ---- ux_polish: local cancel fallback ---------------------------------

    #[test]
    fn resolve_cancel_locally_resolves_pending_and_returns_to_idle() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "long running");
        assert!(app.chats[0].messages[1].pending);
        assert!(app.is_busy());

        app.resolve_cancel_locally();

        assert!(!app.chats[0].messages[1].pending);
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        assert!(app.cancel_active().is_none());
    }

    // ---- ux_polish: transport-failure escalation --------------------------

    #[test]
    fn transport_failures_open_error_popup_and_flip_connection() {
        let mut app = App::new(vec![Chat::new("t")]);
        app.connection = Connection::Connected;
        let submitted = submit_text(&mut app, "go");

        app.apply_backend_event(BackendEvent::Error {
            request_id: event_id_of(&submitted.request_id),
            message: "backend connection lost".into(),
            thread_id: Some(app.chats[0].id.clone()),
        });
        assert_eq!(app.connection, Connection::Disconnected);
        let popup = app.error_popup.as_ref().expect("popup shown");
        assert_eq!(popup.message, "backend connection lost");
        // The chat row still records the failure inline.
        assert!(app.chats[0].messages[1]
            .markdown
            .contains("connection lost"));
    }

    #[test]
    fn per_request_errors_stay_inline_without_popup() {
        let mut app = App::new(vec![Chat::new("t")]);
        app.connection = Connection::Connected;
        let submitted = submit_text(&mut app, "go");

        app.apply_backend_event(BackendEvent::Error {
            request_id: event_id_of(&submitted.request_id),
            message: "Request failed (InvalidArgument): prompt too long".into(),
            thread_id: Some(app.chats[0].id.clone()),
        });
        assert!(
            app.error_popup.is_none(),
            "per-request errors must not open the popup"
        );
        assert_eq!(app.connection, Connection::Connected);
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    }

    #[test]
    fn disconnected_event_flips_indicator_only() {
        let mut app = app_with_chats(1);
        app.connection = Connection::Connected;
        app.apply_backend_event(BackendEvent::Disconnected);
        assert_eq!(app.connection, Connection::Disconnected);
        assert!(app.error_popup.is_none(), "no popup without a request");
        assert!(!app.is_busy());
    }

    #[test]
    fn is_transport_failure_covers_known_phrases() {
        for msg in [
            "I/O error talking to backend: Broken pipe (os error 32)",
            "backend connection lost",
            "handshake failed: unexpected frame",
            "failed to spawn `chibi`: program not found",
            "cancel failed: backend stdin already closed",
        ] {
            assert!(is_transport_failure(msg), "{msg} must be transport");
        }
        for msg in ["Request failed (InvalidArgument): bad prompt", "Cancelled"] {
            assert!(!is_transport_failure(msg), "{msg} is per-request");
        }
    }

    // ---- feat_rename_thread ------------------------------------------------

    #[test]
    fn ctrl_r_enters_rename_mode_prefilled_with_current_name() {
        let mut app = app_with_chats(2);
        assert_eq!(app.mode, Mode::Normal);

        press_ctrl_r(&mut app);
        assert_eq!(
            app.mode,
            Mode::Renaming {
                buf: "chat-0".to_owned()
            },
            "draft must be pre-filled with the active chat's name"
        );
    }

    #[test]
    fn rename_typing_and_backspace_edit_the_draft_only() {
        let mut app = app_with_chats(1);
        press_ctrl_r(&mut app);

        for ch in " v2".chars() {
            app.rename_push(ch);
        }
        assert_eq!(
            app.mode,
            Mode::Renaming {
                buf: "chat-0 v2".to_owned()
            }
        );
        // Prompt input untouched by rename editing.
        assert!(app.input.lines().iter().all(|l| l.is_empty()));

        app.rename_backspace();
        app.rename_backspace();
        assert_eq!(app.rename_buf(), Some("chat-0 "));
        // Chat name unchanged while still drafting.
        assert_eq!(app.chats[0].name, "chat-0");
    }

    /// Enter commits the trimmed draft; the event loop persists afterwards.
    #[test]
    fn commit_rename_saves_trimmed_name() {
        let mut app = app_with_chats(1);
        press_ctrl_r(&mut app);
        // Replace the pre-filled name with the new draft, then save.
        for _ in 0..app.rename_buf().unwrap_or_default().chars().count() {
            app.rename_backspace();
        }
        for ch in "  Deep Dive   ".chars() {
            app.rename_push(ch);
        }

        assert!(app.commit_rename());
        assert_eq!(app.chats[0].name, "Deep Dive", "name trimmed on save");
        assert_eq!(app.chat_title(), "Deep Dive");
        assert_eq!(app.mode, Mode::Normal, "commit closes the session");
    }

    #[test]
    fn cancel_rename_discards_draft_and_keeps_old_name() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "half typed prompt");

        press_ctrl_r(&mut app);
        for ch in "junk draft".chars() {
            app.rename_push(ch);
        }

        assert!(app.cancel_rename());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.chats[0].name, "chat-0", "old name survives Esc");
        // The prompt buffer content survived the whole rename round-trip
        // (rename mode owns its own draft).
        assert_eq!(app.input.lines().join(""), "half typed prompt");
        // Cancelling again without a session is a harmless no-op.
        assert!(!app.cancel_rename());
    }

    #[test]
    fn empty_or_whitespace_names_are_rejected_silently() {
        for draft in ["", "   ", "\t\n "] {
            let mut app = app_with_chats(1);
            press_ctrl_r(&mut app);
            for ch in draft.chars() {
                app.rename_push(ch);
            }
            assert!(!app.commit_rename(), "draft {draft:?} rejected");
            assert_eq!(
                app.chats[0].name, "chat-0",
                "draft {draft:?}: old name kept"
            );
            assert_eq!(app.mode, Mode::Normal, "rejection still leaves rename mode");
        }
    }

    // ---- feat_rename_thread: busy-chat rename ------------------------------

    /// Renaming must not care about request lifecycle: a busy thread's title
    /// is independent of its in-flight work.
    #[test]
    fn renaming_a_busy_chat_works_and_keeps_lifecycle() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "long running question");
        assert!(app.is_busy());
        assert!(matches!(
            app.chats[0].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));

        press_ctrl_r(&mut app);
        // Replace the pre-filled name entirely, then type the new one.
        for _ in 0..app.rename_buf().unwrap_or_default().chars().count() {
            app.rename_backspace();
        }
        for ch in "renamed while busy".chars() {
            app.rename_push(ch);
        }
        assert!(app.commit_rename());

        assert_eq!(app.chats[0].name, "renamed while busy");
        assert_eq!(
            app.chats[0].lifecycle.request_id(),
            Some(submitted.request_id.as_str()),
            "in-flight request untouched by the rename"
        );
        assert_eq!(
            app.active_lifecycle(),
            &ChatLifecycle::Awaiting {
                request_id: submitted.request_id.clone()
            }
        );
    }

    /// Ctrl+R with an already-open session is a no-op (never clobbers the
    /// in-progress draft).
    #[test]
    fn ctrl_r_while_already_renaming_keeps_the_draft() {
        let mut app = app_with_chats(1);
        press_ctrl_r(&mut app);
        for ch in "draft".chars() {
            app.rename_push(ch);
        }

        press_ctrl_r(&mut app);
        assert_eq!(
            app.mode,
            Mode::Renaming {
                buf: "chat-0draft".to_owned()
            },
            "re-press must not reset the draft"
        );
    }

    #[test]
    fn mode_helpers_report_state() {
        let mut app = app_with_chats(1);
        assert!(app.mode.is_normal());
        assert!(app.rename_buf().is_none());

        press_ctrl_r(&mut app);
        assert!(!app.mode.is_normal());
        assert_eq!(app.rename_buf(), Some("chat-0"));

        app.cancel_rename();
        assert!(app.mode.is_normal());
    }

    #[test]
    fn begin_rename_without_chats_is_a_no_op() {
        let mut app = App::new(Vec::new());
        press_ctrl_r(&mut app);
        assert_eq!(app.mode, Mode::Normal, "no chat ⇒ no rename session");
    }

    // ---- feat_input_grow: editor block height helper ---------------------

    #[test]
    fn input_lines_height_collapses_to_one_when_empty_or_cleared() {
        let mut app = app_with_chats(1);
        assert_eq!(app.input_lines_height(), 1, "empty input ⇒ single row");

        app.input.insert_str("one\ntwo\nthree");
        assert_eq!(app.input_lines_height(), 3);

        app.clear_input();
        assert_eq!(
            app.input_lines_height(),
            1,
            "cleared input must collapse back to exactly one row"
        );
    }

    #[test]
    fn input_lines_height_caps_at_max() {
        let mut app = app_with_chats(1);
        for _ in 0..19 {
            app.input.insert_str("\n");
        }
        assert_eq!(
            app.input.lines().len(),
            20,
            "boundary: exactly MAX lines stays uncapped"
        );
        assert_eq!(app.input_lines_height(), crate::app::MAX_INPUT_LINES as u16);

        for _ in 0..40 {
            app.input.insert_str("\n");
        }
        assert_eq!(
            app.input_lines_height(),
            20,
            ">MAX buffer lines clamp to cap"
        );
    }

    #[test]
    fn input_lines_height_counts_rename_draft_lines_too() {
        let mut app = app_with_chats(1);
        press_ctrl_r(&mut app);
        assert_eq!(app.input_lines_height(), 1);

        // Renaming inserts `\n`s into its own draft via modified Enters /
        // paste (routing covered by main.rs tests); here the helper must
        // count THAT active buffer, not the hidden message draft.
        app.mode = Mode::Renaming {
            buf: "ab\ncd\nef".to_owned(),
        };
        assert_eq!(app.input_lines_height(), 3);
    }

    // ---- feat_thread_delete: confirm popup state logic --------------------

    #[test]
    fn ctrl_d_opens_confirm_popup_on_idle_chat() {
        let mut app = app_with_chats(2);
        assert_eq!(app.mode, Mode::Normal);
        app.begin_delete_confirm();
        assert_eq!(app.mode, Mode::ConfirmDelete);
        assert!(
            app.status_message.is_none(),
            "no refusal toast on a deletable chat"
        );
    }

    #[test]
    fn ctrl_d_refuses_busy_chat_with_status_toast() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "long running");
        assert!(app.is_busy());

        app.begin_delete_confirm();
        assert_eq!(app.mode, Mode::Normal, "popup must never open while busy");
        let (msg, _) = app.status_message.as_ref().expect("toast shown");
        assert!(msg.contains("busy"), "toast text: {msg:?}");
        assert_eq!(app.chats.len(), 1, "chat untouched");
    }

    #[test]
    fn ctrl_d_refuses_chat_with_queued_prompts() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "first");
        type_in(&mut app, "second");
        assert!(app.take_input().is_none(), "busy chat enqueues");
        assert_eq!(app.active_queue_len(), 1);

        app.begin_delete_confirm();
        assert_eq!(app.mode, Mode::Normal, "queued prompts block deletion");
        assert!(app.status_message.is_some());
    }

    /// Deleting an idle chat while ANOTHER thread runs in the background is
    /// allowed: the guard inspects only the active chat about to be deleted,
    /// and the background request's lifecycle stays untouched.
    #[test]
    fn busy_background_chat_does_not_block_deleting_idle_active_chat() {
        let mut app = app_with_chats(2);
        submit_text(&mut app, "background work"); // chat 0 busy
        app.select_next(); // chat 1 (idle) becomes active
        assert!(!app.is_busy());

        app.begin_delete_confirm();
        assert_eq!(app.mode, Mode::ConfirmDelete, "idle chat is deletable");
        assert!(app.status_message.is_none());
        assert!(
            matches!(app.chats[0].lifecycle, ChatLifecycle::Awaiting { .. }),
            "background request untouched"
        );
    }

    #[test]
    fn begin_delete_confirm_noop_outside_normal_mode_and_without_chats() {
        let mut app = App::new(Vec::new());
        app.begin_delete_confirm();
        assert_eq!(app.mode, Mode::Normal, "no active chat ⇒ no popup");

        let mut app = app_with_chats(1);
        press_ctrl_r(&mut app); // rename session open
        app.begin_delete_confirm();
        assert!(
            matches!(app.mode, Mode::Renaming { .. }),
            "rename session owns the keyboard — no popup"
        );
    }

    #[test]
    fn confirm_delete_removes_chat_and_selects_next_neighbour() {
        let mut app = app_with_chats(3);
        let removed_id = app.chats[0].id.clone();
        app.scroll_up(30); // detach from bottom before deleting
        assert!(!app.at_bottom());

        app.begin_delete_confirm();
        let removed = app.confirm_delete().expect("removed thread id");

        assert_eq!(removed, removed_id);
        assert_eq!(app.chats.len(), 2);
        assert_eq!(app.active, 0, "next chat slides into the slot");
        assert_eq!(app.chats[0].name, "chat-1");
        assert_eq!(app.mode, Mode::Normal, "popup closed by confirm");
        assert_eq!(
            app.pending_delete.as_deref(),
            Some(removed_id.as_str()),
            "file removal handed to the event loop"
        );
        assert_eq!(app.scroll, 0, "follow-bottom after delete");
        assert!(app.at_bottom());
    }

    #[test]
    fn confirm_delete_middle_chat_selects_next() {
        let mut app = app_with_chats(3);
        app.active = 1;
        app.begin_delete_confirm();
        app.confirm_delete();
        assert_eq!(app.chats.len(), 2);
        assert_eq!(app.active, 1, "former chat-2 is the next neighbour");
        assert_eq!(app.chats[1].name, "chat-2");
    }

    #[test]
    fn confirm_delete_last_chat_selects_prev_neighbour() {
        let mut app = app_with_chats(3);
        app.active = 2;
        app.begin_delete_confirm();
        app.confirm_delete();
        assert_eq!(app.chats.len(), 2);
        assert_eq!(app.active, 1, "previous chat selected after removing last");
        assert_eq!(app.chats[1].name, "chat-1");
        assert_eq!(app.scroll, 0);
    }

    /// Deleting the only chat reaches the clean empty state: zero chats,
    /// active index parked at 0, follow-bottom, no stuck lifecycle.
    #[test]
    fn confirm_delete_last_chat_reaches_clean_empty_state() {
        let mut app = app_with_chats(1);
        app.begin_delete_confirm();
        let removed = app.confirm_delete();

        assert!(removed.is_some());
        assert!(app.chats.is_empty());
        assert_eq!(app.active, 0);
        assert_eq!(app.scroll, 0);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.at_bottom());
        assert!(!app.is_busy());
        assert!(app.active_request_id().is_none());
        assert!(app.pending_delete.is_some());
    }

    #[test]
    fn cancel_delete_keeps_chat_and_draft_untouched() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "precious draft");
        app.begin_delete_confirm();

        assert!(app.cancel_delete());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.chats.len(), 1);
        assert_eq!(app.chats[0].name, "chat-0");
        assert_eq!(app.input.lines().join(""), "precious draft");
        assert!(app.pending_delete.is_none());
        assert!(!app.cancel_delete(), "cancelling again is a no-op");
    }

    #[test]
    fn confirm_delete_without_popup_is_noop() {
        let mut app = app_with_chats(1);
        assert!(app.confirm_delete().is_none());
        assert_eq!(app.chats.len(), 1);
        assert!(app.pending_delete.is_none());
    }

    #[test]
    fn status_toast_expires_after_ticks() {
        let mut app = app_with_chats(1);
        app.show_status("can't delete — busy");
        assert!(app.status_message.is_some());
        for _ in 0..STATUS_MSG_TICKS {
            app.tick_status_message();
        }
        assert!(app.status_message.is_none(), "toast auto-cleared");
        // Further ticks are harmless no-ops.
        app.tick_status_message();
        assert!(app.status_message.is_none());
    }

    // ---- feat_search_thread: popup state logic ----------------------------

    #[test]
    fn ctrl_f_opens_search_popup_with_empty_query() {
        let mut app = app_with_chats(2);
        assert_eq!(app.mode, Mode::Normal);
        app.begin_search();
        assert!(matches!(app.mode, Mode::Searching { .. }));
        assert_eq!(app.search_query(), Some(""));
        assert!(app.search_matches().is_empty());
        assert_eq!(app.search_selected(), 0);
        // Messages untouched by merely opening the popup.
        assert!(app.chats[0].messages.is_empty());
    }

    #[test]
    fn begin_search_noop_without_chats_or_in_other_modes() {
        // No active chat.
        let mut app = App::new(Vec::new());
        app.begin_search();
        assert_eq!(app.mode, Mode::Normal);

        // Rename session open.
        let mut app = app_with_chats(1);
        press_ctrl_r(&mut app);
        app.begin_search();
        assert!(matches!(app.mode, Mode::Renaming { .. }));

        // Delete-confirm popup open.
        let mut app = app_with_chats(1);
        app.begin_delete_confirm();
        app.begin_search();
        assert_eq!(app.mode, Mode::ConfirmDelete);

        // Already searching — re-press must not reset the query.
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::user("needle here"));
        app.begin_search();
        app.search_push('n');
        app.begin_search();
        assert_eq!(app.search_query(), Some("n"), "re-press must not clobber");
    }

    #[test]
    fn search_typing_recomputes_matches_live_case_insensitive() {
        let mut app = app_with_chats(1);
        app.chats[0]
            .messages
            .push(Message::assistant("The Quick Brown Fox"));
        app.chats[0].messages.push(Message::user("quick check"));
        app.begin_search();

        app.search_push('q');
        assert_eq!(app.search_query(), Some("q"));
        assert_eq!(
            app.search_matches().len(),
            2,
            "case-insensitive: q in Quick/quick"
        );

        app.search_push('u');
        assert_eq!(app.search_matches().len(), 2);

        app.search_backspace();
        app.search_backspace();
        assert_eq!(app.search_query(), Some(""));
        assert!(
            app.search_matches().is_empty(),
            "empty query ⇒ no match list (graceful empty state)"
        );
    }

    #[test]
    fn search_matches_ignore_role_headers_and_markup_noise() {
        let mut app = app_with_chats(1);
        // `**bold**` renders to `bold` — the asterisks are style markup and
        // must never be matchable; the role header line ("● Chibi") is UI
        // chrome drawn by render_chat, not part of message text.
        app.chats[0]
            .messages
            .push(Message::assistant("use **bold** sparingly"));
        app.begin_search();

        for ch in "**".chars() {
            app.search_push(ch);
        }
        assert!(
            app.search_matches().is_empty(),
            "style markup must not match"
        );

        for _ in 0..2 {
            app.search_backspace();
        }
        for ch in "bold".chars() {
            app.search_push(ch);
        }
        assert_eq!(
            app.search_matches().len(),
            1,
            "rendered text (markup stripped) must match"
        );
        let m = &app.search_matches()[0];
        assert_eq!(m.message_index, 0);
        assert_eq!(m.line_index, 0);
        assert!(
            m.line_text.contains("bold") && !m.line_text.contains('*'),
            "snippet source must be markup-free: {:?}",
            m.line_text
        );
    }

    #[test]
    fn search_selection_navigation_clamps() {
        let mut app = app_with_chats(1);
        for i in 0..5 {
            app.chats[0]
                .messages
                .push(Message::user(format!("hit {i} needle")));
        }
        app.begin_search();
        for ch in "needle".chars() {
            app.search_push(ch);
        }
        assert_eq!(app.search_matches().len(), 5);
        assert_eq!(app.search_selected(), 0);

        for _ in 0..10 {
            app.search_select_next();
        }
        assert_eq!(app.search_selected(), 4, "next clamps at the last match");

        for _ in 0..10 {
            app.search_select_prev();
        }
        assert_eq!(app.search_selected(), 0, "prev clamps at the first");
    }

    #[test]
    fn search_enter_jumps_to_selected_and_closes_popup() {
        let mut app = app_with_chats(1);
        app.chats[0]
            .messages
            .push(Message::assistant("line one needle"));
        app.chats[0]
            .messages
            .push(Message::user("line two needle again"));
        app.begin_search();
        for ch in "needle".chars() {
            app.search_push(ch);
        }
        assert_eq!(app.search_matches().len(), 2);

        app.search_select_next();
        assert_eq!(app.search_selected(), 1);

        assert!(app.jump_to_selected());
        assert_eq!(app.mode, Mode::Normal, "Enter closes the popup");
        assert_eq!(
            app.pending_search_jump,
            Some((1, 0, 9)),
            "pending jump targets message 1, rendered line 0, char col 9"
        );
        assert!(app.pending_search_jump.is_some());
    }

    #[test]
    fn search_enter_without_matches_is_noop() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::user("nothing to see"));
        app.begin_search();
        // Empty query.
        assert!(!app.jump_to_selected());
        assert!(
            matches!(app.mode, Mode::Searching { .. }),
            "popup stays open on an empty-query Enter"
        );
        assert!(app.pending_search_jump.is_none());
        // Non-matching query.
        app.search_push('z');
        assert!(!app.jump_to_selected());
        assert!(app.pending_search_jump.is_none());
        // Esc still closes cleanly afterwards.
        assert!(app.cancel_search());
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn search_esc_cancels_keeps_view_and_messages() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::user("needle"));
        app.scroll_up(15); // detach from bottom
        let scroll_before = app.scroll;
        let msgs_before: Vec<String> = app.chats[0]
            .messages
            .iter()
            .map(|m| m.markdown.clone())
            .collect();

        app.begin_search();
        app.search_push('n');
        assert!(app.cancel_search());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.scroll, scroll_before, "view unchanged by Esc");
        assert!(
            app.pending_search_jump.is_none(),
            "Esc must never request a jump"
        );
        let msgs_after: Vec<String> = app.chats[0]
            .messages
            .iter()
            .map(|m| m.markdown.clone())
            .collect();
        assert_eq!(msgs_before, msgs_after, "messages untouched");
        assert!(!app.cancel_search(), "cancelling again is a no-op");
    }

    /// The search cycle — open, type, navigate, jump, close — must leave
    /// every message byte-for-byte identical (search is strictly read-only).
    #[test]
    fn search_cycle_is_read_only_on_messages() {
        let mut app = app_with_chats(1);
        app.chats[0]
            .messages
            .push(Message::user("first question with needle"));
        app.chats[0]
            .messages
            .push(Message::assistant("answer with needle too"));
        let before = serde_json::to_string(&app.chats[0].messages).unwrap();

        app.begin_search();
        for ch in "needle".chars() {
            app.search_push(ch);
        }
        app.search_select_next();
        app.jump_to_selected();
        // Reopen and cancel (second cycle, close path).
        app.begin_search();
        app.search_push('n');
        app.cancel_search();

        let after = serde_json::to_string(&app.chats[0].messages).unwrap();
        assert_eq!(before, after, "message bytes must be unchanged");
    }

    /// Search works while the ACTIVE chat is busy (read-only by design) —
    /// the popup opens, matches are found, and the lifecycle stays untouched.
    #[test]
    fn search_opens_and_finds_while_chat_is_busy() {
        let mut app = app_with_chats(1);
        app.chats[0]
            .messages
            .push(Message::user("busy needle question"));
        let submitted = submit_text(&mut app, "in flight");
        assert!(app.is_busy());

        app.begin_search();
        assert!(matches!(app.mode, Mode::Searching { .. }));
        for ch in "needle".chars() {
            app.search_push(ch);
        }
        assert_eq!(app.search_matches().len(), 1);
        assert!(app.jump_to_selected());
        assert_eq!(
            app.chats[0].lifecycle.request_id(),
            Some(submitted.request_id.as_str()),
            "lifecycle untouched by search"
        );
    }

    /// `collect_search_matches` takes an ITERATOR over messages — the task-6
    /// hook: an all-threads search chains every chat's messages through the
    /// same function additively.
    #[test]
    fn collect_search_matches_chains_any_iterator_scope() {
        let chat_a = {
            let mut c = Chat::new("a");
            c.messages.push(Message::user("needle in chat a"));
            c
        };
        let chat_b = {
            let mut c = Chat::new("b");
            c.messages.push(Message::assistant("unrelated"));
            c.messages.push(Message::user("needle in chat b too"));
            c
        };
        let hits = collect_search_matches(
            chat_a.messages.iter().chain(chat_b.messages.iter()),
            "needle",
        );
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].message_index, hits[1].message_index), (0, 2));
    }

    /// Byte-slicing on a lowercased copy would panic on multi-byte glyphs
    /// whose lowercase form expands (`İ` → `i̇`); the char-window matcher
    /// must be byte-agnostic and still find ASCII hits around wide chars.
    #[test]
    fn collect_search_matches_is_unicode_safe_and_finds_ascii() {
        let mut chat = Chat::new("wide");
        chat.messages
            .push(Message::assistant("İstanbul needle İzmir"));
        let hits = collect_search_matches(chat.messages.iter(), "needle");
        assert_eq!(hits.len(), 1, "ASCII hit next to multi-byte glyphs found");
        assert_eq!(hits[0].col, 9, "char offset into the original line");
        // A hit on the wide-glyph word itself is found without panicking.
        let hits = collect_search_matches(chat.messages.iter(), "zmir");
        assert_eq!(hits.len(), 1, "trailing ASCII of a wide-prefixed word");
    }

    /// Multiple occurrences on one rendered line yield one match each.
    #[test]
    fn collect_search_matches_reports_every_occurrence() {
        let mut chat = Chat::new("multi");
        chat.messages
            .push(Message::assistant("x needle y needle z"));
        let hits = collect_search_matches(chat.messages.iter(), "needle");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].col, 2);
        assert_eq!(hits[1].col, 11);
        assert_eq!(hits[0].line_text, "x needle y needle z");
    }

    #[test]
    fn input_lines_height_counts_draft_while_searching() {
        let mut app = app_with_chats(1);
        app.input.insert_str("one\ntwo");
        assert_eq!(app.input_lines_height(), 2);
        app.begin_search();
        assert_eq!(
            app.input_lines_height(),
            2,
            "search overlays the editor; draft height unchanged"
        );
        app.search_push('n');
        assert_eq!(app.input_lines_height(), 2, "query lives in the popup");
    }

    /// per_thread_async tolerance: a terminal event addressed to a REMOVED
    /// chat's thread id must be dropped silently — no panic, no corruption
    /// of surviving chats (their in-flight requests keep routing normally).
    #[test]
    fn terminal_event_for_removed_thread_is_dropped_silently() {
        let mut app = app_with_chats(2);
        let bg = submit_text(&mut app, "background"); // chat 0 busy
        app.select_next(); // chat 1 (idle) active
        app.begin_delete_confirm();
        let removed_id = app.confirm_delete().expect("chat 1 removed");
        assert_ne!(removed_id, bg.thread_id, "busy chat still present");
        assert_eq!(app.chats.len(), 1);

        // Stale event for the REMOVED thread id: silently dropped.
        app.apply_backend_event(BackendEvent::Result {
            request_id: 987,
            markdown: "ghost".into(),
            thread_id: removed_id.clone(),
            model: None,
        });
        assert_eq!(app.chats.len(), 1);
        assert!(
            matches!(app.chats[0].lifecycle, ChatLifecycle::Awaiting { .. }),
            "surviving chat's request untouched by the ghost event"
        );

        // A late event for the still-present busy chat still routes normally.
        let bg_req = bg.request_id.clone();
        app.apply_backend_event(BackendEvent::Result {
            request_id: event_id_of(&bg_req),
            markdown: "**done**".into(),
            thread_id: bg.thread_id.clone(),
            model: None,
        });
        assert_eq!(app.chats[0].messages[1].markdown, "**done**");
        assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
    }

    // ---- feat_search_all_threads: popup state logic ----------------------

    #[test]
    fn ctrl_shift_f_opens_global_search_popup_with_empty_query() {
        let mut app = app_with_chats(2);
        assert_eq!(app.mode, Mode::Normal);
        app.begin_search_all();
        assert!(matches!(app.mode, Mode::SearchingAll { .. }));
        assert_eq!(app.search_all_query(), Some(""));
        assert!(app.search_all_matches().is_empty());
        assert_eq!(app.search_all_selected(), 0);
        // Messages untouched by merely opening the popup.
        assert!(app.chats[0].messages.is_empty());
        assert!(app.chats[1].messages.is_empty());
        assert!(app.pending_global_search_jump.is_none());
    }

    /// One modal at a time: global search never opens over a rename session,
    /// the delete-confirm popup, or an already-open search (in-thread or
    /// global — re-press must not clobber the query).
    #[test]
    fn begin_search_all_noop_with_open_modal() {
        // Rename session open.
        let mut app = app_with_chats(1);
        press_ctrl_r(&mut app);
        app.begin_search_all();
        assert!(matches!(app.mode, Mode::Renaming { .. }));

        // Delete-confirm popup open.
        let mut app = app_with_chats(1);
        app.begin_delete_confirm();
        app.begin_search_all();
        assert_eq!(app.mode, Mode::ConfirmDelete);

        // In-thread search open.
        let mut app = app_with_chats(1);
        app.begin_search();
        app.begin_search_all();
        assert!(matches!(app.mode, Mode::Searching { .. }));

        // Already searching globally — re-press must not reset the query.
        let mut app = app_with_chats(1);
        app.begin_search_all();
        app.search_all_push('n');
        app.begin_search_all();
        assert_eq!(
            app.search_all_query(),
            Some("n"),
            "re-press must not clobber"
        );
    }

    /// The all-threads match list is ordered by CHAT ORDER then MESSAGE
    /// ORDER within each chat, labeled with each thread's title, and
    /// recomputed live (case-insensitive) on every keystroke. Empty query →
    /// graceful empty list.
    #[test]
    fn global_search_typing_recomputes_across_all_chats_in_order() {
        let mut app = app_with_chats(3);
        app.chats[0]
            .messages
            .push(Message::assistant("needle in chat zero"));
        app.chats[1].messages.push(Message::user("unrelated"));
        app.chats[1]
            .messages
            .push(Message::user("needle first in chat one"));
        app.chats[1]
            .messages
            .push(Message::user("needle second in chat one"));
        app.chats[2]
            .messages
            .push(Message::assistant("needle in chat two"));
        app.begin_search_all();

        for ch in "needle".chars() {
            app.search_all_push(ch);
        }
        assert_eq!(app.search_all_query(), Some("needle"));
        let matches = app.search_all_matches();
        assert_eq!(matches.len(), 4);
        // Order = chat order, then message order within each chat.
        assert_eq!(
            matches.iter().map(|m| m.chat_index).collect::<Vec<_>>(),
            vec![0, 1, 1, 2]
        );
        assert_eq!(matches[0].chat_title, "chat-0");
        assert_eq!(matches[1].chat_title, "chat-1");
        assert_eq!((matches[1].message_index, matches[2].message_index), (1, 2));
        assert_eq!(matches[3].chat_index, 2, "single match in chat 2");

        // Empty query → graceful empty state.
        for _ in 0..6 {
            app.search_all_backspace();
        }
        assert_eq!(app.search_all_query(), Some(""));
        assert!(app.search_all_matches().is_empty());
    }

    #[test]
    fn global_search_navigation_clamps() {
        let mut app = app_with_chats(2);
        for i in 0..3 {
            app.chats[0]
                .messages
                .push(Message::user(format!("hit {i} needle")));
        }
        for i in 0..2 {
            app.chats[1]
                .messages
                .push(Message::user(format!("hit {i} needle")));
        }
        app.begin_search_all();
        for ch in "needle".chars() {
            app.search_all_push(ch);
        }
        assert_eq!(app.search_all_matches().len(), 5);
        assert_eq!(app.search_all_selected(), 0);

        for _ in 0..10 {
            app.search_all_select_next();
        }
        assert_eq!(
            app.search_all_selected(),
            4,
            "next clamps at the last match"
        );

        for _ in 0..10 {
            app.search_all_select_prev();
        }
        assert_eq!(app.search_all_selected(), 0, "prev clamps at the first");
    }

    /// THE activation criterion (state layer): Enter activates the TARGET
    /// thread (even a NON-active one) with Ctrl+↑/↓ semantics — index set +
    /// scroll reset — and records the pending jump carrying that chat.
    #[test]
    fn global_search_enter_activates_target_thread_and_records_jump() {
        let mut app = app_with_chats(3);
        app.chats[0]
            .messages
            .push(Message::user("needle in chat zero"));
        app.chats[2]
            .messages
            .push(Message::assistant("needle deep in chat two"));
        app.active = 1; // searching from chat 1; hits live in chats 0 and 2
        app.begin_search_all();
        for ch in "needle".chars() {
            app.search_all_push(ch);
        }
        assert_eq!(app.search_all_matches()[0].chat_index, 0);
        // Select the SECOND match — the NON-active thread.
        app.search_all_select_next();
        assert_eq!(
            app.search_all_matches()[app.search_all_selected()].chat_index,
            2
        );

        assert!(app.jump_to_selected_all());
        assert_eq!(app.mode, Mode::Normal, "Enter closes the popup");
        assert_eq!(app.active, 2, "target thread activated");
        assert_eq!(
            app.pending_global_search_jump,
            Some((2, 0, 0, 0)),
            "pending jump carries chat 2, message 0, rendered line 0, col 0"
        );
        assert!(
            app.pending_search_jump.is_none(),
            "in-thread jump state untouched"
        );
        assert_eq!(app.scroll, 0, "thread switch resets scroll (follow-bottom)");
    }

    #[test]
    fn global_search_enter_without_matches_is_noop() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::user("nothing to see"));
        app.begin_search_all();
        // Empty query.
        assert!(!app.jump_to_selected_all());
        assert!(
            matches!(app.mode, Mode::SearchingAll { .. }),
            "popup stays open on an empty-query Enter"
        );
        assert!(app.pending_global_search_jump.is_none());
        // Non-matching query.
        app.search_all_push('z');
        assert!(!app.jump_to_selected_all());
        assert!(app.pending_global_search_jump.is_none());
        assert_eq!(app.active, 0, "no thread switch without a match");
        // Esc still closes cleanly afterwards.
        assert!(app.cancel_search_all());
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn global_search_esc_cancels_keeps_threads_and_messages() {
        let mut app = app_with_chats(2);
        app.chats[1].messages.push(Message::user("needle"));
        app.scroll_up(15); // detach from bottom
        let scroll_before = app.scroll;
        let active_before = app.active;

        app.begin_search_all();
        app.search_all_push('n');
        assert!(app.cancel_search_all());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.active, active_before, "no thread switch on Esc");
        assert_eq!(app.scroll, scroll_before, "view unchanged by Esc");
        assert!(
            app.pending_global_search_jump.is_none(),
            "Esc must never request a jump"
        );
        assert_eq!(app.chats.len(), 2, "chats untouched");
        assert_eq!(app.chats[1].messages.len(), 1, "messages untouched");
        assert!(!app.cancel_search_all(), "cancelling again is a no-op");
    }

    /// The global search cycle — open, type, navigate, jump, close — must
    /// leave every chat's messages byte-for-byte identical (read-only).
    #[test]
    fn global_search_cycle_is_read_only_on_messages() {
        let mut app = app_with_chats(2);
        app.chats[0].messages.push(Message::user("needle in zero"));
        app.chats[1]
            .messages
            .push(Message::assistant("needle in one"));
        let before: Vec<String> = app
            .chats
            .iter()
            .map(|c| serde_json::to_string(&c.messages).unwrap())
            .collect();

        app.begin_search_all();
        for ch in "needle".chars() {
            app.search_all_push(ch);
        }
        app.search_all_select_next();
        app.jump_to_selected_all();
        // Reopen and cancel (second cycle, close path).
        app.begin_search_all();
        app.search_all_push('n');
        app.cancel_search_all();

        let after: Vec<String> = app
            .chats
            .iter()
            .map(|c| serde_json::to_string(&c.messages).unwrap())
            .collect();
        assert_eq!(before, after, "message bytes must be unchanged");
    }

    /// Global search works while ANY chat is busy (read-only by design) —
    /// and its jump may freely switch to a busy background chat.
    #[test]
    fn global_search_works_while_chats_are_busy() {
        let mut app = app_with_chats(2);
        app.chats[1]
            .messages
            .push(Message::user("needle in background"));
        let submitted = submit_text(&mut app, "in flight"); // chat 0 busy
        assert!(app.is_busy());

        app.begin_search_all();
        for ch in "needle".chars() {
            app.search_all_push(ch);
        }
        assert_eq!(app.search_all_matches().len(), 1);
        assert_eq!(app.search_all_matches()[0].chat_index, 1);
        assert!(app.jump_to_selected_all());
        assert_eq!(app.active, 1, "busy background chat activated");
        assert_eq!(
            app.chats[0].lifecycle.request_id(),
            Some(submitted.request_id.as_str()),
            "lifecycle untouched by search"
        );
    }

    /// Zero chats: the popup opens and shows a graceful empty state; Enter
    /// is a no-op and Esc closes cleanly.
    #[test]
    fn global_search_works_with_zero_chats_gracefully() {
        let mut app = App::new(Vec::new());
        app.begin_search_all();
        assert!(matches!(app.mode, Mode::SearchingAll { .. }));
        app.search_all_push('n');
        assert!(app.search_all_matches().is_empty());
        assert!(!app.jump_to_selected_all());
        assert!(app.cancel_search_all());
        assert_eq!(app.mode, Mode::Normal);
    }

    /// A single-thread dataset behaves exactly like the in-thread search —
    /// no special casing needed, the global path just works.
    #[test]
    fn global_search_single_thread_dataset_works() {
        let mut app = app_with_chats(1);
        app.chats[0]
            .messages
            .push(Message::assistant("sole needle here"));
        app.begin_search_all();
        for ch in "needle".chars() {
            app.search_all_push(ch);
        }
        assert_eq!(app.search_all_matches().len(), 1);
        let m = &app.search_all_matches()[0];
        assert_eq!((m.chat_index, m.chat_title.as_str()), (0, "chat-0"));
        assert!(app.jump_to_selected_all());
        assert_eq!(app.active, 0);
        assert_eq!(app.pending_global_search_jump, Some((0, 0, 0, 5)));
    }

    /// Rendering-wise the popup must stay consistent with its own state:
    /// role headers and markdown markup never match (the per-chat
    /// [`collect_search_matches`] guarantees carry over verbatim).
    #[test]
    fn global_search_matches_ignore_role_headers_and_markup_noise() {
        let mut app = app_with_chats(2);
        app.chats[1]
            .messages
            .push(Message::assistant("use **bold** sparingly"));
        app.begin_search_all();
        for ch in "**".chars() {
            app.search_all_push(ch);
        }
        assert!(app.search_all_matches().is_empty(), "markup must not match");
        for _ in 0..2 {
            app.search_all_backspace();
        }
        for ch in "bold".chars() {
            app.search_all_push(ch);
        }
        assert_eq!(app.search_all_matches().len(), 1);
        let m = &app.search_all_matches()[0];
        assert_eq!((m.chat_index, m.message_index, m.line_index), (1, 0, 0));
        assert!(
            m.line_text.contains("bold") && !m.line_text.contains('*'),
            "snippet source must be markup-free: {:?}",
            m.line_text
        );
    }

    // ---- feat_stderr_log_modal: log viewer state ---------------------------

    /// Unique marker line for global-buffer assertions: other tests append
    /// to the same process-global stream concurrently, so assertions are
    /// contains-based / monotonic, never exact-position.
    fn unique_line(tag: &str) -> String {
        format!(
            "app-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn log_state(app: &App) -> &LogViewerState {
        match &app.mode {
            Mode::LogViewer { state } => state,
            other => panic!("expected LogViewer mode, got {other:?}"),
        }
    }

    #[test]
    fn opening_log_viewer_snapshots_ring_and_lives_tail() {
        let marker = unique_line("snap");
        crate::diag::append(&marker);
        let total_before_open = crate::diag::total_appended();

        // Works with ZERO chats: the viewer reads the global diag stream.
        let mut app = App::new(Vec::new());
        app.begin_log_viewer();

        let state = log_state(&app);
        assert_eq!(state.scroll, 0, "opens live-tailing at the bottom");
        assert!(
            state.lines.contains(&marker),
            "snapshot copy contains the buffered marker"
        );
        // Reset-on-open (consumer-side watermark): everything up to the
        // open-time total is seen. Monotonicity makes `>=` exact — the
        // watermark must have captured a total that already includes the
        // pre-open marker.
        assert!(
            app.log_seen_total >= total_before_open,
            "seen watermark advanced past the pre-open marker: {} < {}",
            app.log_seen_total,
            total_before_open
        );
        assert_eq!(
            app.log_seen_total, state.snapshot_total,
            "watermark and +K baseline coincide at open"
        );
    }

    /// The unseen marker logic end to end at the state level: unseen arrivals
    /// after open/close keep the app "dirty" (marker would show); re-opening
    /// (and closing at the bottom) marks everything seen again.
    #[test]
    fn seen_watermark_resets_on_open_and_close_at_bottom() {
        let mut app = App::new(Vec::new());
        crate::diag::append("pre-existing");
        assert!(
            crate::diag::total_appended() > app.log_seen_total,
            "precondition: fresh app has unseen lines"
        );

        app.begin_log_viewer();
        let seen_at_open = app.log_seen_total;

        // Lines arriving while the viewer is OPEN at the tail are watched
        // live: closing at the bottom marks them seen too.
        crate::diag::append("watched-live");
        assert!(app.close_log_viewer());
        assert!(
            app.log_seen_total > seen_at_open,
            "close-at-bottom advances the watermark past arrivals"
        );

        // Closing while DETACHED does not mark: the missed lines stay unseen.
        let mut app = App::new(Vec::new());
        app.begin_log_viewer();
        let seen_at_open = app.log_seen_total;
        app.log_scroll_up(5);
        crate::diag::append("missed-while-detached");
        assert!(app.close_log_viewer());
        assert_eq!(
            app.log_seen_total, seen_at_open,
            "close while scrolled up must not mark the missed lines seen"
        );
        assert!(
            crate::diag::total_appended() > app.log_seen_total,
            "the missed line still counts as unseen (marker shows)"
        );
    }

    #[test]
    fn opening_log_viewer_noop_while_another_modal_owns_the_keyboard() {
        // Rename session.
        let mut app = app_with_chats(1);
        app.begin_rename();
        app.begin_log_viewer();
        assert!(matches!(app.mode, Mode::Renaming { .. }));

        // Delete-confirm popup.
        let mut app = app_with_chats(1);
        app.begin_delete_confirm();
        app.begin_log_viewer();
        assert_eq!(app.mode, Mode::ConfirmDelete);

        // In-thread search popup.
        let mut app = app_with_chats(1);
        app.begin_search();
        app.begin_log_viewer();
        assert!(matches!(app.mode, Mode::Searching { .. }));

        // Global search popup.
        let mut app = app_with_chats(1);
        app.begin_search_all();
        app.begin_log_viewer();
        assert!(matches!(app.mode, Mode::SearchingAll { .. }));

        // Already-open viewer: re-open must not reset the scroll.
        let mut app = app_with_chats(0);
        app.begin_log_viewer();
        app.log_scroll_up(7);
        app.begin_log_viewer();
        assert_eq!(log_state(&app).scroll, 7, "re-open does not clobber");
    }

    #[test]
    fn log_scrolling_detaches_and_return_to_bottom_re_arms_live_tail() {
        let mut app = app_with_chats(0);
        app.begin_log_viewer();
        let baseline = log_state(&app).snapshot_total;

        // Detach: scroll freezes the snapshot (baseline stays put).
        app.log_scroll_up(30);
        app.log_scroll_up(30);
        assert_eq!(log_state(&app).scroll, 60, "PgUp accumulates");
        app.log_scroll_down(10);
        assert_eq!(log_state(&app).scroll, 50);

        // Lines arriving while detached count against the baseline…
        let detached_marker = unique_line("detached");
        crate::diag::append(&detached_marker);
        assert_eq!(
            log_state(&app).snapshot_total,
            baseline,
            "snapshot frozen while scrolled up"
        );
        let total_now = crate::diag::total_appended();
        assert!(
            total_now >= baseline,
            "monotonic total includes the detached arrival"
        );

        // …and re-arming the tail refreshes the snapshot to CURRENT content.
        app.log_scroll_down(50);
        let state = log_state(&app);
        assert_eq!(state.scroll, 0, "back at the bottom");
        assert!(
            state.lines.contains(&detached_marker),
            "live-tail refresh picked up the detached arrival"
        );
        assert!(
            state.snapshot_total >= baseline,
            "baseline advanced to the current ring content"
        );
    }

    #[test]
    fn closing_log_viewer_resets_mode_and_focus() {
        // At the bottom (live-tail): close marks everything seen.
        let mut app = app_with_chats(2);
        app.focus = Focus::Sidebar;
        app.begin_log_viewer();
        assert!(app.close_log_viewer());
        assert!(app.mode.is_normal());
        assert_eq!(app.focus, Focus::Chat, "modal close returns to editor");
        assert!(!app.close_log_viewer(), "closing again is a no-op");

        // Scrolled up: close still resets the UI…
        let mut app = App::new(Vec::new());
        app.begin_log_viewer();
        let seen_at_open = app.log_seen_total;
        app.log_scroll_up(9);
        crate::diag::append("missed-while-detached");
        assert!(app.close_log_viewer());
        assert!(app.mode.is_normal());
        assert_eq!(app.focus, Focus::Chat);
        // …and the watermark did NOT advance: the missed line stays unseen.
        assert_eq!(
            app.log_seen_total, seen_at_open,
            "close while detached must not mark missed lines seen"
        );
    }

    #[test]
    fn log_viewer_leaves_chats_and_draft_untouched() {
        let mut app = app_with_chats(2);
        type_in(&mut app, "precious draft");
        // Message is Serialize; Chat itself is not — snapshot names, ids,
        // lifecycle states and message bytes (the state the viewer could
        // possibly disturb).
        let chats_before: Vec<String> = app
            .chats
            .iter()
            .map(|c| {
                serde_json::to_string(&c.messages).unwrap()
                    + &format!("|{}|{}|{:?}", c.name, c.id, c.lifecycle)
            })
            .collect();
        app.scroll_up(12);

        app.begin_log_viewer();
        app.log_scroll_up(3);
        app.close_log_viewer();

        let chats_after: Vec<String> = app
            .chats
            .iter()
            .map(|c| {
                serde_json::to_string(&c.messages).unwrap()
                    + &format!("|{}|{}|{:?}", c.name, c.id, c.lifecycle)
            })
            .collect();
        assert_eq!(chats_after, chats_before, "viewer is read-only on chats");
        assert_eq!(app.input.lines().join(""), "precious draft");
        assert_eq!(app.scroll, 12, "chat scroll untouched by the modal");
    }
}
