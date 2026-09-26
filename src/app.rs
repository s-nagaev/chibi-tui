//! Application state: chats, selection, input buffer, request lifecycle.

use std::collections::{HashMap, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};

use crate::backend::BackendEvent;
use crate::diag::LogEntry;
use crate::input::InputArea;
use crate::markdown;
use crate::model::{ChatLifecycle, Message};
use crate::model_picker::{parse_model_listing, parse_selection_confirmation, ModelEntry};
use crate::popup::ErrorPopup;
use crate::protocol::{AgentEventKind, Usage};
use crate::theme::Theme;
use unicode_width::UnicodeWidthChar;

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

/// One search hit inside a message's rendered text.
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

/// State of the open in-thread search popup.
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

/// One search hit across ALL threads.
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

/// State of the open ALL-threads search popup.
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

/// State of the open diagnostics-log viewer modal (its interaction
/// core shared with the renderer).
///
/// Read position management is deliberately simple, snapshot-copy based:
/// the modal holds a COPY of the ring buffer taken at the last refresh, so
/// the capture task can keep appending (and evicting) freely underneath.
/// The cursor walks LOGICAL lines: the newest line is the live tail, so
/// while the cursor rests there the view stays pinned to the tail and every
/// new line streams in; one cursor step up unpins, and the view then stays
/// glued to the cursor instead of jumping to the bottom. `wrap` toggles
/// Paragraph-style reflow of long lines; the cursor still moves one logical
/// line per step, no matter how many rows a wrapped line occupies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogViewerState {
    /// Snapshot copy of the buffered log entries (oldest → newest) at the
    /// last refresh (open, live-tail frame, or return-to-bottom). Each
    /// entry carries the raw text plus the level parsed at ingestion.
    pub lines: Vec<LogEntry>,
    /// Logical line the cursor sits on (0-based; the newest line is the
    /// live tail).
    pub cursor: usize,
    /// `w` toggle: reflow long lines over multiple rows instead of
    /// right-truncating them.
    pub wrap: bool,
    /// [`crate::diag::total_appended()`] at snapshot time — the baseline the
    /// `+K new lines` footer hint is computed against.
    pub snapshot_total: u64,
    /// Row offset of the rendered viewport (render-side bookkeeping, kept
    /// here so a pinned view scrolls minimally instead of re-anchoring on
    /// every frame). Rows, not logical lines: wrap changes the grid.
    pub row_offset: usize,
    /// Buffer of the open `/` search prompt.
    /// `None` while the prompt is closed; while it holds a value the prompt
    /// owns the keyboard completely (Enter commits, Esc cancels).
    pub search_buf: Option<String>,
    /// The committed search: pattern, matching logical lines and which one
    /// the cursor is on. `None` with no search active (an empty pattern
    /// commit switches the search off).
    pub search: Option<LogSearch>,
    /// Transient header feedback of the last `y` copy attempt (`copied` or
    /// `copy: unavailable`), cleared by [`LogViewerState::expire_copy_note`]
    /// after a couple of seconds.
    pub copy_note: Option<String>,
    /// When [`LogViewerState::copy_note`] was set; the expiry anchor.
    pub copy_note_at: Option<std::time::Instant>,
}

/// One committed log-viewer search.
///
/// `matches` holds LOGICAL line indices, so navigation stays correct in both
/// wrap modes: the cursor jumps whole lines, never rows. One entry per line
/// even when the pattern occurs several times inside it: `n`/`N` walk lines,
/// while the renderer highlights every occurrence on every matched line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSearch {
    /// The committed pattern (case-insensitive substring).
    pub pattern: String,
    /// Logical line indices with at least one hit, ascending.
    pub matches: Vec<usize>,
    /// Index into `matches` of the hit the cursor is parked on. `None`
    /// right after commit (before the first `n`/`N`), which is also the
    /// difference between the header showing `matches: 7` and
    /// `matches: 3/7`.
    pub current: Option<usize>,
}

impl LogViewerState {
    /// True when the cursor rests on the newest line of the snapshot, the
    /// live tail. Only then does the viewer keep streaming new lines in.
    pub fn at_tail(&self) -> bool {
        self.cursor + 1 >= self.lines.len()
    }

    /// Rendered row count of one logical line at the given content width:
    /// 1 when wrapping is off, the chunk count otherwise. The wrap math is
    /// shared with the renderer so cursor paging and the visible-window
    /// arithmetic can never disagree.
    pub fn row_count_of(&self, index: usize, width: usize) -> usize {
        if self.wrap {
            wrapped_row_count(&self.lines[index].text, width)
        } else {
            1
        }
    }

    /// Recompute the match list after the snapshot changed (live tail
    /// refresh, return to bottom). The pattern stays, the current-match
    /// ordinal is kept when it still points somewhere, otherwise dropped.
    pub fn reindex_search(&mut self) {
        let Some(pattern) = self.search.as_ref().map(|s| s.pattern.clone()) else {
            return;
        };
        let matches: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, entry)| line_matches(&entry.text, &pattern))
            .map(|(i, _)| i)
            .collect();
        if let Some(search) = self.search.as_mut() {
            search.matches = matches;
            if let Some(cur) = search.current {
                if cur >= search.matches.len() {
                    search.current = None;
                }
            }
        }
    }

    /// Drop the copy feedback note once its brief display window passed
    /// (called from the renderer every frame).
    pub fn expire_copy_note(&mut self) {
        if let Some(at) = self.copy_note_at {
            if at.elapsed() > std::time::Duration::from_secs(2) {
                self.copy_note = None;
                self.copy_note_at = None;
            }
        }
    }
}

/// Case-insensitive substring test used by the log search.
fn line_matches(line: &str, pattern: &str) -> bool {
    line.to_lowercase().contains(&pattern.to_lowercase())
}

/// All case-insensitive occurrences of `pattern` in `line`, as char-offset
/// ranges `(start, end)` (end exclusive, non-overlapping, ascending). Char
/// offsets on purpose: they line up with the wrap chunks the renderer
/// builds, so highlights survive the reflow. An empty pattern never
/// matches.
pub fn find_match_ranges(line: &str, pattern: &str) -> Vec<(usize, usize)> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let hay: Vec<char> = line.to_lowercase().chars().collect();
    let pat: Vec<char> = pattern.to_lowercase().chars().collect();
    if pat.is_empty() || pat.len() > hay.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + pat.len() <= hay.len() {
        if hay[i..i + pat.len()] == pat[..] {
            out.push((i, i + pat.len()));
            i += pat.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Split one logical line into `width`-wide row chunks (character-level,
/// display width via unicode-width, so the row math matches what the
/// renderer shows exactly). `width` 0 is treated as 1: a degenerate
/// viewport must not divide by zero or loop forever.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for ch in line.chars() {
        let ch_w = ch.width().unwrap_or(0);
        if current_w + ch_w > width && !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(ch);
        current_w += ch_w;
    }
    rows.push(current);
    rows
}

/// Row count of a wrapped logical line (see [`wrap_line`]).
pub fn wrapped_row_count(line: &str, width: usize) -> usize {
    wrap_line(line, width).len()
}

/// Phase of the open model-picker popup.
///
/// The popup opens IMMEDIATELY on ^M (modal isolation is active from the
/// first keystroke) while the `/model` listing request travels the normal
/// pipeline; `Loading` is that in-flight placeholder. The first parseable
/// result flips the state to `Ready` — or closes the popup via the
/// degradation path when the listing is unusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelPickerPhase {
    /// Listing request in flight; rows not known yet.
    Loading,
    /// Listing parsed; `entries` is non-empty and navigable.
    Ready,
}

/// State of the open model-picker popup.
///
/// Same modal-family shape as the search popups: the popup owns its own
/// state, the chat draft is untouched, and there is no text input here —
/// the listing is short enough to navigate directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPickerState {
    /// `Loading` until the hidden `/model` result resolves.
    pub phase: ModelPickerPhase,
    /// Parsed listing rows (empty while loading).
    pub entries: Vec<ModelEntry>,
    /// Index into `entries` of the highlighted row.
    pub selected: usize,
}

/// State of the open keybindings help modal.
///
/// The content is static — the renderer reads `ui::HOTKEY_ROWS` directly —
/// so the only per-open state is the scroll offset of the row window, fed
/// back a matching visible-row count the same seam the log viewer and the
/// picker page by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpModalState {
    /// First visible row of `ui::HOTKEY_ROWS`; clamped at both ends by the
    /// scroll methods so the window can never pass an edge.
    pub scroll: usize,
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
    /// The Ctrl+D delete-confirmation popup is open for the ACTIVE chat.
    /// Enter/`y` confirm, Esc/`n` cancel; every other
    /// key is swallowed by the modal branch in `main.rs` so nothing leaks
    /// into the textarea or triggers a global binding.
    ConfirmDelete,
    /// The ^L / ⇧^L stop-reset confirmation popup is open
    ///. `action` selects which destructive
    /// command the confirmation stages: stop the running request (/stop) or
    /// reset the thread (/reset + local dialog clear). Enter/`y` confirm,
    /// Esc/`n` cancel; every other key is swallowed by the modal branch in
    /// `main.rs` — same grammar and isolation as [`Mode::ConfirmDelete`].
    ConfirmStopReset { action: StopResetAction },
    /// The Ctrl+F in-thread search popup is open:
    /// `state` holds the query buffer, cached match list and selection.
    /// Modal-ish isolation mirrors ConfirmDelete — keystrokes go to the
    /// popup only (see `main.rs`), the chat view is read-only, and Enter
    /// jumps the chat scroll to the selected match via
    /// [`App::pending_search_jump`] before closing.
    Searching { state: SearchState },
    /// The Ctrl+Shift+F ALL-threads search popup is open:
    /// `state` holds the query buffer, cached
    /// match list (spanning every chat, each entry labeled with its thread
    /// title) and selection. Same modal-ish isolation as [`Mode::Searching`]
    /// — one popup at a time — but Enter ACTIVATES the match's thread
    /// (same selection mechanics as Ctrl+↑/↓ switching) and records a
    /// pending wrapped-row jump via [`App::pending_global_search_jump`].
    SearchingAll { state: GlobalSearchState },
    /// The ^G diagnostics-log viewer modal is open:
    /// a monospace view of the unified backend-stderr + `[tui]` lifecycle
    /// ring buffer. Modal isolation like the search popups — PgUp/PgDn (and
    /// ↑/↓) scroll, Esc closes, every other key is swallowed. Opening
    /// resets the unseen-lines counter; live-tail at the bottom, frozen
    /// snapshot with a `+K new lines` footer while scrolled up.
    LogViewer { state: LogViewerState },
    /// The ^M model-picker popup is open. Modal
    /// isolation like the search family: ↑/↓ navigate, PgUp/PgDn page the
    /// selection by one viewport of visible rows, Enter selects,
    /// Esc closes, everything else is swallowed. The listing arrives through
    /// the NORMAL request pipeline as a hidden exchange — see
    /// `HiddenPurpose` and [`App::begin_model_picker`].
    ModelPicking { state: ModelPickerState },
    /// The F1 keybindings help modal is open.
    /// Modal isolation like the popup family: ↑/↓ (and PgUp/PgDn) scroll the
    /// static `ui::HOTKEY_ROWS` table, F1 or Esc closes, everything else is
    /// swallowed. The table is a const in `ui.rs` next to the render; a
    /// dispatch-side test pins it against the real key handlers so future
    /// chords cannot silently miss the modal.
    HelpViewing { state: HelpModalState },
}

impl Mode {
    /// True in the everyday chatting state only — no rename session, no
    /// delete-confirm popup, and no search popup open.
    pub fn is_normal(&self) -> bool {
        matches!(self, Mode::Normal)
    }
}

/// What the quit-confirmation popup does with a key press.
///
/// The ONE grammar shared by every quit-confirmation surface: the in-app
/// popup (dispatch branch in `main.rs`), the splash screen and the backend
/// setup screen (both run their own pre-app event loops). Keeping the
/// decision in one pure function pins the grammar against drift between
/// the three loops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuitDecision {
    /// Quit for real (`y`/`Enter`; `Ctrl+C` — the second press IS the
    /// confirmation, the first one only opened the popup).
    Confirm,
    /// Stay in the app, restore the exact prior state (`Esc`/`n`; `q` as
    /// dismiss is deliberate — it can never CONFIRM a quit, and treating
    /// it as "stay" matches the modal family where `q` is unbound rather
    /// than destructive).
    Dismiss,
    /// Swallowed: no leak into any editor, no global binding fires.
    Swallow,
}

/// Classify a key press for the quit-confirmation popup. Plain chars only:
/// ctrl-chords other than `Ctrl+C` are swallowed like any other combo, and
/// modifier rides-along (`Shift+y`) confirms the same as plain `y`.
pub fn quit_decision(key: KeyEvent) -> QuitDecision {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => QuitDecision::Confirm,
        KeyCode::Char('c') if ctrl => QuitDecision::Confirm,
        KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl => QuitDecision::Confirm,
        KeyCode::Esc => QuitDecision::Dismiss,
        KeyCode::Char('n') | KeyCode::Char('N') if !ctrl => QuitDecision::Dismiss,
        KeyCode::Char('q') if !ctrl => QuitDecision::Dismiss,
        _ => QuitDecision::Swallow,
    }
}
/// which pane owns the keyboard. NOT a [`Mode`] —
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
    /// Last-activity timestamp (unix seconds): refreshed on every new user
    /// or assistant message in the thread and persisted with the snapshot.
    /// The sidebar is ordered by it, newest first (ties break by id), so a
    /// thread with fresh activity rises to the top.
    pub updated_at: u64,
    pub messages: Vec<Message>,
    /// This chat's own request lifecycle (`Idle` / `Awaiting` / `Running`).
    pub lifecycle: ChatLifecycle,
    /// Prompts submitted while a request was already in flight; sent FIFO as
    /// soon as the current request reaches a terminal event.
    pub queue: VecDeque<String>,
    /// a visible reply (or an inline error)
    /// landed in this chat while it was NOT the selected one. Cleared the
    /// moment the chat is selected (see `App::select_chat`). Session-only:
    /// never persisted, a restart starts every thread clean.
    pub unread: bool,
    /// Thread's last known turn usage, carried across restarts: the event
    /// loop keeps it fresh on every usage-carrying terminal frame and the
    /// history layer writes it with the thread snapshot, so the next launch
    /// can seed the ctx segment without waiting for a live turn. `None` for
    /// fresh chats and for snapshots recorded before the field existed.
    pub last_usage: Option<Usage>,
    /// Thread's last known model label, persisted with the thread snapshot
    /// (same lifecycle as [`Chat::last_usage`]): visible replies stamping a
    /// model and hidden `/model <n>` switches both refresh it. `None` for
    /// fresh chats and for snapshots recorded before the field existed.
    pub last_model: Option<String>,
    /// This thread's own latest reasoning chain, rendered as the dim block
    /// above the chat's last assistant message. Written ONLY by visible
    /// answers of THIS chat that carry thoughts — the turn's terminal result
    /// and every background continuation APPEND their payload
    /// (`retain_thoughts` via the event handlers; hidden
    /// model-picker exchanges and fieldless command results never touch it) —
    /// and the chain resets when a new visible request starts in THIS chat
    /// only. The renderer reads the ACTIVE chat's field, so a background
    /// reply lands in its own chat and the view can never show another
    /// thread's reasoning. Session-only: never persisted (reasoning is heavy
    /// and the contract keeps the block restart-fresh).
    pub last_thoughts: Option<String>,
    /// live subagent progress for THIS thread, keyed by
    /// the numeric protocol request id → (active, total). Populated from
    /// mid-turn `agent_event` frames via [`Chat::apply_subagent_event`]
    /// regardless of the request lifecycle — background subagents outlive
    /// their turn's result frame, so the last-known count stays renderable
    /// while the chat is idle. Per-request keys keep a late frame from ever
    /// polluting another request's counters; the entry disappears when its
    /// frame reports `active == 0`. Session state only — never persisted.
    pub subagent_counts: HashMap<u64, (u64, u64)>,
}

impl Chat {
    /// Fresh chat with a generated stable thread id.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            id: crate::history::new_thread_id(),
            updated_at: crate::history::now_unix(),
            messages: Vec::new(),
            lifecycle: ChatLifecycle::Idle,
            queue: VecDeque::new(),
            unread: false,
            last_usage: None,
            last_model: None,
            last_thoughts: None,
            subagent_counts: HashMap::new(),
        }
    }

    /// True while this chat has a request in flight.
    pub fn is_busy(&self) -> bool {
        self.lifecycle.is_busy()
    }

    /// Total live subagents currently known for this thread across its
    /// requests, or `None` when the spinner line must not show a counter
    /// (no entries, or every entry reports zero active subagents).
    pub fn running_subagents(&self) -> Option<u64> {
        let running: u64 = self
            .subagent_counts
            .values()
            .map(|(active, _)| active)
            .sum();
        (running > 0).then_some(running)
    }

    /// Fold one mid-turn `agent_event` frame into this thread's subagent
    /// counters. `started` inserts/updates the frame request's entry from
    /// the frame values; `finished` updates it and removes the entry once
    /// the frame reports `active == 0` (all subagents of that request are
    /// done). The frame is non-terminal: the request lifecycle is never
    /// touched here.
    pub fn apply_subagent_event(
        &mut self,
        request_id: u64,
        event: AgentEventKind,
        active: u64,
        total: u64,
    ) {
        match event {
            AgentEventKind::Started => {
                self.subagent_counts.insert(request_id, (active, total));
            }
            AgentEventKind::Finished => {
                if active == 0 {
                    self.subagent_counts.remove(&request_id);
                } else {
                    self.subagent_counts.insert(request_id, (active, total));
                }
            }
        }
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

/// One endpoint of a mouse text selection, in CHAT DISPLAY-ROW space.
///
/// `row` is the index of the wrapped display row within the FULL
/// transcript (the same row list `ui::render_chat` paints —
/// scroll-independent, so scrolling during a drag never moves a stored
/// point) and `col` the 0-based CHAR offset inside that row's visible
/// text (clamped at the row length by the hit-test).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionPoint {
    /// Wrapped display row within the full transcript.
    pub row: usize,
    /// Char offset inside the row's visible text.
    pub col: usize,
}

/// Mouse text selection over the chat transcript: an anchor (press
/// point) plus a head (current drag point). Session-only VIEW state —
/// never persisted, never written to history snapshots, cleared on
/// Esc / a plain click / a thread switch like every other view state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatSelection {
    /// Position of the initial press.
    pub anchor: SelectionPoint,
    /// Position of the latest drag (the selection extends here).
    pub head: SelectionPoint,
    /// True between press and release: the drag is live and further
    /// drag events move the head.
    pub dragging: bool,
}

impl ChatSelection {
    /// Endpoints in ascending (row, col) order — the normalized
    /// selection range every consumer (highlight, text extraction)
    /// works with, regardless of the drag direction.
    pub fn ordered(&self) -> (SelectionPoint, SelectionPoint) {
        if (self.head.row, self.head.col) < (self.anchor.row, self.anchor.col) {
            (self.head, self.anchor)
        } else {
            (self.anchor, self.head)
        }
    }
}

/// Hit-test metadata of ONE wrapped display row: which logical line it
/// belongs to and which char range of that line it shows. Produced by
/// the renderer's own wrap (so hit-testing and painting can never
/// disagree) and cached per frame on [`App::chat_geometry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatRowMeta {
    /// Index of the owning logical line (role header, rendered markdown
    /// line, thoughts line, trailing blank).
    pub logical: usize,
    /// Char offset of the row within its logical line, INCLUSIVE. Break
    /// spaces dropped by the wrap are covered by no row, so ranges may
    /// have gaps — matching the wrap exactly.
    pub start: usize,
    /// Char offset of the row's end within its logical line, EXCLUSIVE.
    pub end: usize,
    /// Plain text of the row (span styles stripped) — the plain-text
    /// extraction source for the clipboard copy.
    pub text: String,
}

/// Render-fed geometry seam for mouse hit-testing (same pattern as
/// [`App::chat_visible_rows`]): the renderer caches the wrapped-row
/// model of the frame it just painted, so the mouse router can map a
/// cursor position onto the exact document position. `chat_id` guards
/// against a one-iteration staleness window (a mouse event arriving
/// after a thread switch but before the next draw): a geometry from a
/// foreign chat is rejected by the consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGeometry {
    /// Stable thread id the geometry was built for.
    pub chat_id: String,
    /// Inner rect of the chat pane the transcript was rendered into.
    pub inner: Rect,
    /// Display rows hidden from the top by the current scroll
    /// (`ui::scroll_skip`), so terminal rows map to document rows.
    pub skip: usize,
    /// Per-display-row metadata (same length as the painted row list).
    pub rows: Vec<ChatRowMeta>,
}

impl ChatGeometry {
    /// Map a terminal position onto a document [`SelectionPoint`].
    /// `None` outside the chat pane's inner rect or with no rows (the
    /// pane is hit-tested by `ui::panel_region` first; this adds the
    /// inner-rect and content guards). Terminal rows inside the pane
    /// but past the last document row clamp to it — dragging below the
    /// transcript end extends to the end, the conventional behavior.
    pub fn position_at(&self, column: u16, row: u16) -> Option<SelectionPoint> {
        if self.rows.is_empty() || !self.inner.contains(Position { x: column, y: row }) {
            return None;
        }
        let visible_row = (row - self.inner.y) as usize;
        let doc_row = (self.skip + visible_row).min(self.rows.len() - 1);
        let meta = &self.rows[doc_row];
        let x = (column - self.inner.x) as usize;
        // Column → char index: count the chars fully LEFT of the pointer
        // (display width via unicode-width, so a wide glyph is taken
        // whole, never half-selected).
        let mut used = 0usize;
        let mut col = 0usize;
        for ch in meta.text.chars() {
            let cw = ch.width().unwrap_or(0);
            if used + cw > x {
                break;
            }
            used += cw;
            col += 1;
        }
        Some(SelectionPoint { row: doc_row, col })
    }
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
    pub input: InputArea,
    pub spinner_frame: usize,
    pub should_quit: bool,
    /// Modal error popup (backend failures). When set, it captures input
    /// until dismissed.
    pub error_popup: Option<ErrorPopup>,
    /// Quit-confirmation popup ("Quit chibi-tui?"). A standalone flag, NOT
    /// a [`Mode`]: opening it from any state — Normal mode, another popup,
    /// the sidebar — must not disturb that state, so dismissing restores it
    /// exactly (focus, mode, draft, in-flight request all untouched). Set
    /// by every manual exit path (`q` in the error popup, idle Ctrl+C) via
    /// [`App::begin_quit_confirm`]; the dispatch branch in `main.rs` owns
    /// the keyboard while it is open.
    pub quit_confirm: bool,
    /// Transport liveness for the status-bar indicator.
    pub connection: Connection,
    /// Set by `R` in the error popup; the event loop performs the async
    /// reconnect and clears it.
    pub reconnect_requested: Option<ReconnectRequest>,
    /// Cancel target `(request_id, thread_id)` produced by Ctrl+C; the event
    /// loop sends the actual frame. Targets ONLY the active chat's in-flight
    /// request — queued prompts survive a cancel.
    pub pending_cancel: Option<(String, String)>,
    /// Thread id whose persisted history file must be removed
    ///: set by the confirm-popup Enter path; the event
    /// loop performs the actual `history::delete_chat_file_in` and consumes
    /// this. Kept out of the I/O-free [`App`] so state logic stays
    /// unit-testable without a filesystem.
    pub pending_delete: Option<String>,
    /// Transient status toast (busy-refusal) shown in the
    /// status line; auto-expires after [`STATUS_MSG_TICKS`] spinner ticks.
    pub status_message: Option<(String, u8)>,
    /// which pane owns the keyboard (Chat by default).
    /// Reset to [`Focus::Chat`] whenever a modal closes — see [`Focus`].
    pub focus: Focus,
    /// Global input mode (feature: inline thread rename / thread delete /
    /// in-thread search).
    pub mode: Mode,
    /// pending search jump `(message_index, line_index,
    /// col)` — the char offset inside the message's rendered line. Set by
    /// Enter in the search popup, consumed by `ui::render_chat` (the jump
    /// math needs the SAME wrapped-row totals as rendering, so it happens
    /// there, not in the state layer). `col` lets the jump land on the
    /// WRAPPED row that actually contains the hit inside a long paragraph.
    pub pending_search_jump: Option<(usize, usize, usize)>,
    /// pending GLOBAL search jump `(chat_index,
    /// message_index, line_index, col)` — `chat_index` is the match's OWNING
    /// thread. Set by Enter in the all-threads search popup AFTER
    /// [`App::jump_to_selected_all`] already activated that thread (the
    /// activation lives in the state layer, the wrapped-row math in
    /// `ui::render_chat` like the in-thread jump). `chat_index` rides along
    /// purely as a defensive marker: render verifies the target is still the
    /// active chat before jumping, so a chat that vanished mid-frame drops
    /// the jump silently instead of scrolling the wrong thread.
    pub pending_global_search_jump: Option<(usize, usize, usize, usize)>,
    /// visible height (rows) of the log-viewer
    /// modal's content area, set during `ui::render_log_viewer` so PgUp/PgDn
    /// scroll exactly one page of modal rows. Defaults to 20 until first
    /// render (same seam as [`App::chat_visible_rows`]).
    pub log_visible_rows: u16,
    /// content width (columns) of the log-viewer
    /// modal's text area, set during `ui::render_log_viewer`. With wrap on,
    /// page steps are measured in reflowed rows, so the cursor math needs
    /// the same width the renderer chunks lines at. Defaults to 100 until
    /// first render.
    pub log_content_width: u16,
    /// Visible list height (rows) of the open model-picker popup, set
    /// during `ui::render_model_picker` so PgUp/PgDn page exactly one
    /// viewport of picker rows. Defaults to 20 until first render (same
    /// seam as [`App::chat_visible_rows`] and [`App::log_visible_rows`]).
    pub picker_visible_rows: u16,
    /// Visible body height (rows) of the open help modal, set during
    /// `ui::render_help_modal` so ↑/↓ and PgUp/PgDn scroll exactly one
    /// row/page of the keybindings table. Defaults to 20 until first render
    /// (same seam as [`App::picker_visible_rows`]).
    pub help_visible_rows: u16,
    /// the consumer-side "seen" watermark — the
    /// [`crate::diag::DiagLog::total`] value at the moment the log stream
    /// was last fully viewed (viewer opened / re-tailed / closed at the
    /// bottom). The `log*` status marker fires while
    /// `diag::total_appended() > log_seen_total` and the viewer is closed.
    /// Tracking unseen state on the consumer side of a monotonic producer
    /// total is race-free by construction — no counter resets to coordinate.
    pub log_seen_total: u64,
    /// visibility of the one-row status strip (workspace
    /// cwd · active chat's model), toggled with ^O. Default VISIBLE. Pure
    /// VIEW state like [`Focus`] — deliberately NOT a [`Mode`]: it never
    /// captures keys and survives every modal open/close untouched.
    pub status_strip_visible: bool,
    /// workspace root backing the strip's cwd segment
    /// (rendered as the last three path components with a leading `/`).
    /// Wired once at startup from the CLI `--workspace` value (which
    /// itself defaults to the process cwd) for BOTH backends, mock
    /// included. The renderer reads it reactively every frame, so if the
    /// root ever changes at runtime the next frame shows the new tail —
    /// today it is CLI-only and never changes. `None` renders as the `—`
    /// placeholder.
    pub workspace_root: Option<String>,
    /// Live effective working directories reported by the backend's
    /// `cwd_update` frames (opt-in via `capabilities.cwd_updates`), keyed by
    /// the WIRE thread id. The status strip reads the ACTIVE thread's entry
    /// (falling back to [`App::workspace_root`] when the thread has no
    /// report yet), so the cwd readout follows the agent state instead of
    /// the launch directory.
    pub thread_cwds: HashMap<i64, String>,
    /// hidden exchange bundle staged by the key
    /// handlers (`^M` open, Enter selection) for the event loop to hand to
    /// the backend source via the SAME `send_submitted` path as any prompt.
    /// The bundle carries its own ids; nothing here touches the transcript.
    pub picker_submission: Option<Submitted>,
    /// purpose registry of in-flight hidden
    /// exchanges, keyed by protocol request id. A terminal event whose
    /// tracked id is found here is suppressed from the transcript and
    /// resolved into the picker/toast state instead of a chat bubble.
    hidden_requests: HashMap<String, HiddenPurpose>,
    /// hidden requests deferred behind the per-thread
    /// busy rules — `(thread_id, prompt)` FIFO. A picker action taken while
    /// the chat is busy must wait for an idle drain (the same rule any
    /// visible prompt obeys), but it must NOT enter the chat's VISIBLE queue
    /// (it would render bubbles for plumbing). Flushed one-per-drain by the
    /// event loop via [`App::take_deferred_hidden_request`].
    hidden_queue: VecDeque<(String, String)>,
    /// session-scoped last-known model labels staged
    /// by a hidden `/model <n>` switch, keyed by thread id. Bridges the gap
    /// until the chat's next visible reply stamps its OWN label (which
    /// retires the override) — the status strip reads
    /// [`App::active_model_label`] and picks the switch up with zero
    /// coupling. Startup restore seeds one entry per thread whose snapshot
    /// recorded a persisted last-known model (see [`Chat::last_model`]), so
    /// the map itself stays session-only and is never written to disk.
    picker_model_labels: HashMap<String, String>,
    /// slash commands the backend advertised at handshake.
    /// Empty for mocks/offline; gates the ^P clone shortcut via detection.
    pub(crate) backend_commands: Vec<String>,
    /// clone request staged by `begin_clone_thread` for
    /// the event loop to send (same seam as `picker_submission`). Carries
    /// the NEW chat's UUID as `thread_id`, so the command arrives on the
    /// thread it creates, exactly like /reset.
    clone_submission: Option<Submitted>,
    /// the not-yet-listed clone chat plus its request
    /// correlation, held between staging and the backend ack. An error
    /// resolution drops it, so a failed clone leaves no orphan thread.
    pending_clone: Option<PendingClone>,
    /// the /stop or /reset control request staged
    /// by the confirm popup for the event loop to send (same seam as
    /// `clone_submission`). It travels OUT-OF-BAND on purpose: both commands
    /// exist precisely to act while a request is still running, so the
    /// busy-chat FIFO queue must never delay them.
    control_submission: Option<Submitted>,
    /// the in-flight control request's
    /// correlation, held between staging and the backend ack. A stop never
    /// touches the chat lifecycle (the killed request resolves itself via
    /// its own cancelled error); a reset clears the dialog on the ack.
    pending_control: Option<PendingControl>,
    /// Latest-turn metadata from the most recent `result` frames.
    ///
    /// Usage is the sticky last-known token count: a terminal frame updates
    /// it only when the frame actually carries usage, and a new request never
    /// clears it, so command results and hidden exchanges between answers
    /// keep the previous readout alive. The ctx segment is empty only while
    /// no usage has arrived this session AND the initially selected chat's
    /// snapshot carried no persisted usage (startup restore seeds this field
    /// from [`Chat::last_usage`]). The reasoning trace is NOT mirrored here:
    /// it lives per chat on [`Chat::last_thoughts`] and the renderer reads
    /// the active chat's own value, so no cross-thread routing exists to
    /// break.
    pub last_turn_usage: Option<Usage>,
    /// session-only visibility of the dim reasoning block
    /// rendered above the latest answer from the active chat's
    /// [`Chat::last_thoughts`]. Default ON; ^S flips it. Pure VIEW state like
    /// [`Focus`] and the status strip — never a [`Mode`], never persisted,
    /// and flipping it never touches the retained thoughts (render-only
    /// switch).
    pub thoughts_visible: bool,
    /// mouse text selection over the chat transcript (anchor + head in
    /// display-row space). Session-only view state: never persisted,
    /// never written to history snapshots, cleared by Esc / a plain
    /// click (press+release without drag) / a thread switch (the
    /// `App::select_chat` seam). `dragging` marks the live press →
    /// release window.
    pub selection: Option<ChatSelection>,
    /// the wrapped-row geometry the renderer painted THIS frame
    /// ([`ChatGeometry`]), the hit-test seam the mouse router maps
    /// cursor positions through. Refreshed on every `render_chat`;
    /// rejected by consumers when its `chat_id` no longer matches the
    /// active thread.
    pub chat_geometry: Option<ChatGeometry>,
}

/// a clone request in flight. The `chat` waits here until the backend
/// acks; the ack lists the clone at the sidebar top, so the source is not
/// tracked.
struct PendingClone {
    chat: Chat,
    request_id: String,
}

/// which destructive command the stop/reset
/// confirmation popup stages. `Stop` maps to the backend's `/stop` command
/// (telegram kill-all + counter flush semantics); `Reset` to `/reset`,
/// which additionally clears the local dialog once the backend acks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopResetAction {
    /// ^L: stop the running request.
    Stop,
    /// ⇧^L: reset the thread and clear the dialog.
    Reset,
}

/// a /stop or /reset control request in flight.
/// Terminal frames matching `(thread_id, request_id)` are consumed by
/// [`App::apply_control_event`] before the normal chat routing would drop
/// them as uncorrelated (the chat keeps tracking the request being killed).
struct PendingControl {
    kind: StopResetAction,
    request_id: String,
    thread_id: String,
}

/// what a hidden (transcript-suppressed) request is
/// FOR — the terminal event's handling depends on it. The wire exchange is
/// identical to a visible prompt; only the resolution differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HiddenPurpose {
    /// Bare `/model`: the listing resolves into the picker (or degrades).
    FetchListing,
    /// `/model <n>`: the confirmation resolves into a status toast.
    SelectModel,
}

const SPINNER: [&str; 10] = [
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];

/// hard cap on how many terminal rows the ACTIVE editor
/// block (message draft OR rename draft) may occupy. Beyond this the
/// textarea view auto-scrolls inside the capped window so the caret always
/// stays on screen.
pub const MAX_INPUT_LINES: usize = 20;

/// how many 100 ms spinner ticks a transient status
/// toast stays visible (~2.5 s).
pub const STATUS_MSG_TICKS: u8 = 25;

/// the bare listing request. Sent verbatim through
/// the normal pipeline; the real backend treats it as a slash command and
/// answers with the textual model listing in `result.content` (no LLM turn).
pub const MODEL_LISTING_PROMPT: &str = "/model";
/// toast shown when a hidden listing request
/// resolves to an unusable listing (unparsable / zero rows).
const MODEL_LIST_UNAVAILABLE: &str = "model list unavailable";
/// the protocol-level acknowledgement marker the
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
        let mut input = InputArea::default();
        input.set_placeholder_text("Type a message…  (\u{23ce} send)");
        // Restore seam: threads carry their last known usage/model across
        // restarts, so the sticky display state is seeded from the snapshot
        // instead of starting blind. The ctx segment is a single app-level
        // readout, so it takes the initially selected chat's usage; the
        // panel model is per thread, so every restored chat with a recorded
        // label stages the same session-scoped override the model picker
        // uses (retired by the chat's next visible labelled reply).
        let picker_model_labels: HashMap<String, String> = chats
            .iter()
            .filter_map(|c| c.last_model.as_ref().map(|m| (c.id.clone(), m.clone())))
            .collect();
        let last_turn_usage = chats.first().and_then(|c| c.last_usage);
        Self {
            chats,
            active: 0,
            scroll: 0,
            chat_visible_rows: 20,
            input,
            spinner_frame: 0,
            should_quit: false,
            error_popup: None,
            quit_confirm: false,
            connection: Connection::Connecting,
            reconnect_requested: None,
            pending_cancel: None,
            pending_delete: None,
            status_message: None,
            focus: Focus::Chat,
            mode: Mode::Normal,
            pending_search_jump: None,
            pending_global_search_jump: None,
            log_visible_rows: 20,
            log_content_width: 100,
            picker_visible_rows: 20,
            help_visible_rows: 20,
            log_seen_total: 0,
            status_strip_visible: true,
            workspace_root: None,
            thread_cwds: HashMap::new(),
            picker_submission: None,
            hidden_requests: HashMap::new(),
            hidden_queue: VecDeque::new(),
            picker_model_labels,
            backend_commands: Vec::new(),
            clone_submission: None,
            pending_clone: None,
            control_submission: None,
            pending_control: None,
            last_turn_usage,
            thoughts_visible: true,
            selection: None,
            chat_geometry: None,
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

    /// the ONE place a chat becomes the selected
    /// one. Bounds-safe (the index is clamped to the current list), resets
    /// the chat view to follow-bottom, clears the unread marker of the
    /// chat being entered, and re-seeds the sticky ctx segment from the
    /// entered chat's last-known usage — the status readout always follows
    /// the thread being entered (neutral when it has none), never the one
    /// left behind. The reasoning block needs no re-seed: the renderer reads
    /// the entered chat's own [`Chat::last_thoughts`], so the view follows
    /// the thread by construction. Every selection mutation (arrows, ^N,
    /// search jump, delete/clone ack, restore activation) goes through here.
    fn select_chat(&mut self, index: usize) {
        self.active = index.min(self.chats.len().saturating_sub(1));
        if let Some(chat) = self.chats.get_mut(self.active) {
            chat.unread = false;
        }
        self.last_turn_usage = self.chats.get(self.active).and_then(|chat| chat.last_usage);
        self.scroll = 0;
        // The mouse selection is per-thread view state like the scroll:
        // entering a thread never shows a selection made in another one.
        self.selection = None;
    }

    pub fn select_next(&mut self) {
        self.select_chat(self.active.saturating_add(1));
    }

    pub fn select_prev(&mut self) {
        self.select_chat(self.active.saturating_sub(1));
    }

    /// Record fresh activity on the chat at `index` and lift the thread to
    /// the top of the sidebar (the list IS the render order — newest first).
    /// A no-op for out-of-range indexes. The selection follows the move:
    /// a chat that was active stays active after landing on top, and the
    /// indexes of the chats it jumped over are fixed up so every other
    /// selection stays put.
    pub fn touch_chat(&mut self, index: usize) {
        if index >= self.chats.len() {
            return;
        }
        self.chats[index].updated_at = crate::history::now_unix();
        if index == 0 {
            return;
        }
        let chat = self.chats.remove(index);
        self.chats.insert(0, chat);
        if self.active == index {
            self.active = 0;
        } else if self.active < index {
            self.active += 1;
        }
    }

    /// Restore seam (remember-last-thread): open the app on a specific
    /// thread id exactly as if the user had picked it — the same selection
    /// semantics as `App::select_chat`, whose sticky ctx-segment seeding
    /// re-seeds the readout from that thread's snapshot (the readout is
    /// seeded from the initially selected chat, so a restored thread that
    /// is not the first must reseed it; the per-thread panel model is
    /// already staged for every restored chat by [`App::new`]). An unknown
    /// id is a silent no-op: a dangling pointer must never disturb the
    /// default startup selection.
    pub fn activate_thread(&mut self, thread_id: &str) {
        let Some(index) = self.chats.iter().position(|c| c.id == thread_id) else {
            return;
        };
        self.select_chat(index);
    }

    /// toggle which pane owns the keyboard — Ctrl+T flips
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

    // cwd + model status strip ------------------------

    /// toggle the one-row status strip (workspace cwd ·
    /// active chat's model) with ^O. Pure VIEW state like [`Focus`] —
    /// deliberately not a [`Mode`]: it never captures keys, survives every
    /// modal open/close untouched, and the renderer just reads the flag
    /// each frame. Default: hidden.
    pub fn toggle_status_strip(&mut self) {
        self.status_strip_visible = !self.status_strip_visible;
    }

    /// toggle the dim reasoning block above the latest
    /// answer with ^S. Render-only session view state (default ON): nothing
    /// is cleared, the flip just changes whether the renderer draws the
    /// active chat's [`Chat::last_thoughts`] block; the retained reasoning
    /// itself is untouched and still lives only in memory.
    pub fn toggle_thoughts(&mut self) {
        self.thoughts_visible = !self.thoughts_visible;
    }

    /// the strip's cwd segment, the LAST THREE components
    /// of the thread's effective working directory: the live value the
    /// backend reported for the ACTIVE thread via `cwd_update` frames
    /// ([`App::thread_cwds`], opt-in `capabilities.cwd_updates`) when one
    /// exists, falling back to the CLI workspace root
    /// ([`App::workspace_root`], wired at startup; the backend never spoke
    /// about that thread yet), joined
    /// with `/` and prefixed with a leading `/`: the path-tail format
    /// `/Develop/personal/chibi-tui` instead of the bare `chibi-tui`, so
    /// sibling workspaces tell themselves apart at a glance. Paths with
    /// fewer components show the same prefix with what they have, nothing
    /// is fabricated; the filesystem root (`/`) and non-absolute roots
    /// (`.`) degrade to the raw string, and so does a tail holding a
    /// non-UTF-8 component. `None` (only before startup wiring) yields
    /// `None` and the renderer shows the `—` placeholder.
    pub fn status_cwd(&self) -> Option<String> {
        let live = self
            .active_thread_id()
            .and_then(|tid| self.thread_cwds.get(&crate::live::wire_thread_id(tid)))
            .cloned();
        let root = live.or_else(|| self.workspace_root.clone())?;
        let root = root.as_str();
        let path = std::path::Path::new(root);
        if !path.is_absolute() {
            return Some(root.to_string());
        }
        let names: Vec<Option<&str>> = path
            .components()
            .skip(1) // the leading `/` of the prefix stands in for RootDir
            .map(|c| c.as_os_str().to_str())
            .collect();
        if names.is_empty() {
            return Some("/".to_string());
        }
        let mut parts: Vec<&str> = Vec::with_capacity(names.len().min(3));
        for name in &names[names.len().saturating_sub(3)..] {
            match name {
                Some(s) => parts.push(s),
                // non-UTF-8 inside the tail: the raw root is the readout
                None => return Some(root.to_string()),
            }
        }
        Some(format!("/{}", parts.join("/")))
    }

    /// the strip's model segment — the LAST KNOWN model
    /// label of the ACTIVE chat, reusing the model-label
    /// per-message metadata: the most recent assistant message carrying a
    /// non-empty label. Deriving it per frame gives the required update
    /// semantics for free: a result resolution stamps the label onto the
    /// new message (visible next frame), an error/fieldless resolution
    /// stamps `None` (the previous label remains "last known"), and
    /// switching chats re-labels from that chat's own message history.
    /// The override map is never written to disk, but startup restore
    /// seeds it from each thread's persisted last-known model (see
    /// `App::picker_model_labels`); per-message labels themselves
    /// persist with the snapshot, so a restored chat re-labels the strip
    /// from its own newest answer once the override retires.
    /// `None` renders as the `—` placeholder.
    ///
    /// a hidden `/model <n>` switch has no transcript
    /// bubble to derive from, so it stages a session-scoped override (see
    /// `App::picker_model_labels`) that this getter prefers; the chat's
    /// next visible labeled reply retires it and message-derived truth
    /// resumes.
    ///
    /// In-chat `● Chibi (model)` annotations are independent of this
    /// getter: each message renders only the label captured when it was
    /// produced, so a mid-chat switch never re-labels past answers.
    pub fn active_model_label(&self) -> Option<&str> {
        let chat = self.chats.get(self.active)?;
        // a hidden `/model <n>` switch updates the
        // last-known model WITHOUT a transcript bubble. The staged override
        // shadows the message-derived label until the next visible reply of
        // the chat stamps its own label (and retires the override).
        if let Some(label) = self.picker_model_labels.get(&chat.id) {
            return Some(label);
        }
        chat.messages.iter().rev().find_map(|m| m.model_label())
    }

    /// Create a chat with a fresh UUID thread_id, select it. Creating a
    /// thread is an editor-bound action — focus lands back on
    /// Chat so typing goes straight into the prompt.
    pub fn new_chat(&mut self) {
        let n = self.chats.len() + 1;
        // A new thread starts on top of the sidebar (newest-first order)
        // and is selected immediately.
        self.chats.insert(0, Chat::new(format!("New chat {n}")));
        self.select_chat(0);
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
            // The delete-confirm popup, the search popups and the log viewer
            // have no draft of their own (search queries live in
            // a seperate state).
            Mode::Normal
            | Mode::ConfirmDelete
            | Mode::ConfirmStopReset { .. }
            | Mode::Searching { .. }
            | Mode::SearchingAll { .. }
            | Mode::LogViewer { .. }
            | Mode::ModelPicking { .. }
            | Mode::HelpViewing { .. } => None,
        }
    }

    /// terminal rows the ACTIVE editor block needs right
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
            | Mode::ConfirmStopReset { .. }
            | Mode::Searching { .. }
            | Mode::SearchingAll { .. }
            | Mode::LogViewer { .. }
            | Mode::ModelPicking { .. }
            | Mode::HelpViewing { .. } => self.input.lines().len(),
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
            // Not renaming (Normal, any popup: delete-confirm, search, log
            // viewer): no-op.
            Mode::Normal
            | Mode::ConfirmDelete
            | Mode::ConfirmStopReset { .. }
            | Mode::Searching { .. }
            | Mode::SearchingAll { .. }
            | Mode::LogViewer { .. }
            | Mode::ModelPicking { .. }
            | Mode::HelpViewing { .. } => return false,
        };
        self.mode = Mode::Normal;
        // a closed modal returns keyboard ownership to
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
        self.focus = Focus::Chat; // modal closed → editor pane
        true
    }

    // ---- in-thread search ----------------------------

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
        self.focus = Focus::Chat; // modal closed → editor pane
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
                // modal closed → editor pane.
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

    // ---- all-threads search --------------------

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
        self.focus = Focus::Chat; // modal closed → editor pane
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
                // Activate the target thread exactly like Ctrl+↑/↓ (via
                // select_chat: index set + scroll reset + marker clear, the
                // jump math in render_chat then overrides the scroll with
                // the match's row).
                self.select_chat(chat_index);
                self.pending_global_search_jump =
                    Some((chat_index, message_index, line_index, col));
                self.mode = Mode::Normal;
                // modal closed → editor pane.
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

    // ---- diagnostics log viewer -------------------

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
                cursor: lines.len().saturating_sub(1),
                wrap: false,
                row_offset: 0,
                lines,
                snapshot_total,
                search_buf: None,
                search: None,
                copy_note: None,
                copy_note_at: None,
            },
        };
    }

    /// Refresh the snapshot to the CURRENT ring content and park the cursor
    /// on the (new) newest line: the live tail re-arms, everything shown
    /// counts as seen. Shared by cursor-down-to-tail, `G` and the render
    /// loop's tail-follow refresh.
    fn log_rearm_tail(&mut self) {
        if let Mode::LogViewer { state } = &mut self.mode {
            let (lines, snapshot_total) = crate::diag::view();
            state.cursor = lines.len().saturating_sub(1);
            state.row_offset = 0;
            state.lines = lines;
            state.snapshot_total = snapshot_total;
            self.log_seen_total = snapshot_total;
            state.reindex_search();
        }
    }

    /// ↑ or k: move the line cursor up (clamped at the top). One step = one
    /// LOGICAL line, even when wrap splits it over several rows. Moving up
    /// unpins the view from the tail: the snapshot freezes and new arrivals
    /// only count in the `+K new lines` footer.
    pub fn log_cursor_up(&mut self, amount: usize) {
        if let Mode::LogViewer { state } = &mut self.mode {
            state.cursor = state.cursor.saturating_sub(amount);
        }
    }

    /// ↓ or j: move the line cursor down (clamped at the bottom). Reaching
    /// the newest line re-arms the live tail (snapshot refresh + seen
    /// watermark), matching the tail-follow rule.
    pub fn log_cursor_down(&mut self, amount: usize) {
        let reached_tail = match &mut self.mode {
            Mode::LogViewer { state } => {
                let last = state.lines.len().saturating_sub(1);
                state.cursor = state.cursor.saturating_add(amount).min(last);
                state.cursor == last && !state.lines.is_empty()
            }
            _ => false,
        };
        if reached_tail {
            self.log_rearm_tail();
        }
    }

    /// PgUp: page up by (roughly) one viewport of rows; the cursor lands on
    /// the page's top line. With wrap on, the page is measured in reflowed
    /// rows at the render width, so a page is a page in both modes.
    pub fn log_page_up(&mut self) {
        if let Mode::LogViewer { state } = &mut self.mode {
            let page = self.log_visible_rows as usize;
            let width = self.log_content_width.max(1) as usize;
            let mut skipped = 0usize;
            let mut target = state.cursor;
            while target > 0 {
                target -= 1;
                skipped += state.row_count_of(target, width);
                if skipped >= page {
                    break;
                }
            }
            state.cursor = target;
        }
    }

    /// PgDn: page down by one viewport of rows; the cursor lands on the
    /// page's bottom line. Reaching the newest line re-arms the live tail.
    pub fn log_page_down(&mut self) {
        let reached_tail = match &mut self.mode {
            Mode::LogViewer { state } => {
                let page = self.log_visible_rows as usize;
                let width = self.log_content_width.max(1) as usize;
                let last = state.lines.len().saturating_sub(1);
                let mut advanced = 0usize;
                let mut target = state.cursor;
                while target < last {
                    advanced += state.row_count_of(target, width);
                    target += 1;
                    if advanced >= page {
                        break;
                    }
                }
                state.cursor = target;
                state.cursor == last && !state.lines.is_empty()
            }
            _ => false,
        };
        if reached_tail {
            self.log_rearm_tail();
        }
    }

    /// g: jump to the oldest line. The view stays pinned to the cursor.
    pub fn log_jump_top(&mut self) {
        if let Mode::LogViewer { state } = &mut self.mode {
            state.cursor = 0;
        }
    }

    /// G: jump to the newest line, back to the live tail. The snapshot
    /// refreshes to current ring content and the stream counts as seen.
    pub fn log_jump_bottom(&mut self) {
        self.log_rearm_tail();
    }

    /// w: toggle logical-line wrapping. The cursor keeps pointing at the
    /// same logical line; only the row grid under it changes.
    pub fn log_toggle_wrap(&mut self) {
        if let Mode::LogViewer { state } = &mut self.mode {
            state.wrap = !state.wrap;
        }
    }

    // ---- log viewer search ------------------

    /// `/`: open the search prompt at the bottom of the viewer with an
    /// empty buffer. Also clears the copy feedback, so the header never
    /// carries two transient notes at once.
    pub fn log_open_search(&mut self) {
        if let Mode::LogViewer { state } = &mut self.mode {
            state.search_buf = Some(String::new());
            state.copy_note = None;
            state.copy_note_at = None;
        }
    }

    /// One printable char into the open search prompt buffer.
    pub fn log_search_push(&mut self, ch: char) {
        if let Mode::LogViewer { state } = &mut self.mode {
            if let Some(buf) = state.search_buf.as_mut() {
                buf.push(ch);
            }
        }
    }

    /// Backspace in the open search prompt buffer (no-op when empty).
    pub fn log_search_pop(&mut self) {
        if let Mode::LogViewer { state } = &mut self.mode {
            if let Some(buf) = state.search_buf.as_mut() {
                buf.pop();
            }
        }
    }

    /// Enter on the open prompt: commit the search. An empty pattern
    /// switches the search off entirely. Otherwise the match list is
    /// computed over the whole snapshot and the cursor jumps to the nearest
    /// match at or after its current line, so the user lands somewhere
    /// visible without losing the reading position.
    pub fn log_commit_search(&mut self) {
        let pattern = match &mut self.mode {
            Mode::LogViewer { state } => state.search_buf.take().unwrap_or_default(),
            _ => return,
        };
        let jump = match &mut self.mode {
            Mode::LogViewer { state } => {
                if pattern.is_empty() {
                    state.search = None;
                    None
                } else {
                    let matches: Vec<usize> = state
                        .lines
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| line_matches(&entry.text, &pattern))
                        .map(|(i, _)| i)
                        .collect();
                    let target = matches.iter().find(|&&m| m >= state.cursor).copied();
                    state.search = Some(LogSearch {
                        pattern,
                        matches,
                        current: None,
                    });
                    target
                }
            }
            _ => None,
        };
        if let Some(line) = jump {
            if let Mode::LogViewer { state } = &mut self.mode {
                state.cursor = line;
            }
        }
    }

    /// Esc on the open prompt: cancel, keep the previous search (if any)
    /// untouched.
    pub fn log_cancel_search(&mut self) {
        if let Mode::LogViewer { state } = &mut self.mode {
            state.search_buf = None;
        }
    }

    /// n: next match after the cursor line, wrapping around at the end.
    /// No-op without a committed search or when nothing matched.
    pub fn log_search_next(&mut self) {
        self.log_search_step(true);
    }

    /// N: previous match before the cursor line, wrapping around at the
    /// start. No-op without a committed search or when nothing matched.
    pub fn log_search_prev(&mut self) {
        self.log_search_step(false);
    }

    /// Shared n/N stepper: picks the neighbouring match line (with
    /// wraparound), records it as the current hit and moves the cursor
    /// there. Navigation never re-arms the live tail on purpose: jumping to
    /// a match is reading, not following the stream.
    fn log_search_step(&mut self, forward: bool) {
        let jump = match &mut self.mode {
            Mode::LogViewer { state } => {
                let Some(search) = state.search.as_mut() else {
                    return;
                };
                if search.matches.is_empty() {
                    return;
                }
                let target = if forward {
                    search
                        .matches
                        .iter()
                        .find(|&&m| m > state.cursor)
                        .copied()
                        .unwrap_or(search.matches[0])
                } else {
                    search
                        .matches
                        .iter()
                        .rev()
                        .find(|&&m| m < state.cursor)
                        .copied()
                        .unwrap_or(*search.matches.last().expect("matches non-empty"))
                };
                search.current = Some(
                    search
                        .matches
                        .iter()
                        .position(|&m| m == target)
                        .expect("target came from matches"),
                );
                Some(target)
            }
            _ => None,
        };
        if let Some(line) = jump {
            if let Mode::LogViewer { state } = &mut self.mode {
                state.cursor = line;
            }
        }
    }

    /// y: copy the full text of the cursor LOGICAL line to the clipboard.
    ///
    /// In wrap mode that is the whole message, not the truncated row. The
    /// transports are [`crate::clipboard::copy_text`]: OSC 52 first, then
    /// the `arboard` system clipboard, then the `CHIBI_TUI_COPY_CMD`
    /// fallback when set. The header gets a brief feedback note either way
    /// (`copied` / `copy: unavailable`).
    pub fn log_copy_selected(&mut self) {
        let note = match &self.mode {
            Mode::LogViewer { state } => {
                let Some(entry) = state.lines.get(state.cursor) else {
                    return;
                };
                match crate::clipboard::copy_text(&entry.text) {
                    crate::clipboard::CopyOutcome::Copied => "copied".to_owned(),
                    crate::clipboard::CopyOutcome::Unavailable => "copy: unavailable".to_owned(),
                }
            }
            _ => return,
        };
        if let Mode::LogViewer { state } = &mut self.mode {
            state.copy_note = Some(note);
            state.copy_note_at = Some(std::time::Instant::now());
        }
    }

    /// Close the log viewer (Esc). Closing while live-tailing (cursor on
    /// the newest line) marks the stream as seen: the user just watched
    /// those lines arrive. Closing while pinned further up keeps the unseen
    /// counter, so the `log*` status marker keeps flagging the lines missed
    /// while the view was frozen.
    pub fn close_log_viewer(&mut self) -> bool {
        let at_bottom = match &self.mode {
            Mode::LogViewer { state } => state.at_tail(),
            _ => return false,
        };
        self.mode = Mode::Normal;
        // modal closed → editor pane.
        self.focus = Focus::Chat;
        if at_bottom {
            // The user just watched the tail arrive: everything is seen.
            self.log_seen_total = crate::diag::total_appended();
        }
        true
    }

    // ---- model picker ----------------------------

    /// Open the model-picker popup for the ACTIVE chat (`^M`).
    ///
    /// The popup opens immediately in [`ModelPickerPhase::Loading`] and the
    /// bare `/model` listing request travels the NORMAL pipeline as a hidden
    /// exchange: it reuses the chat's own request lifecycle (spinner, busy
    /// rules, cancel, queue drain) but appends NO transcript bubbles, and
    /// its result resolves into the picker state instead of a message.
    ///
    /// Per-thread busy rules apply exactly like any prompt: when the chat is
    /// busy the fetch is NOT sent — it is parked in the hidden FIFO and
    /// dispatched by the event loop on the next idle drain
    /// ([`App::take_deferred_hidden_request`]). The picker stays `Loading`
    /// until then. Returns nothing: the bundle (when the chat is idle) is
    /// staged in [`App::picker_submission`] for the event loop, keeping this
    /// method synchronous and testable.
    ///
    /// No-op when another modal owns the keyboard or there is no chat.
    pub fn begin_model_picker(&mut self) {
        if !self.mode.is_normal() || self.active_thread_id().is_none() {
            return;
        }
        let thread_id = self.active_thread_id().unwrap().to_owned();
        self.mode = Mode::ModelPicking {
            state: ModelPickerState {
                phase: ModelPickerPhase::Loading,
                entries: Vec::new(),
                selected: 0,
            },
        };
        let chat_index = self.chats.iter().position(|c| c.id == thread_id).unwrap();
        let chat = &mut self.chats[chat_index];
        if chat.lifecycle.is_busy() {
            // Busy chat: obey the same wait-for-idle rule as a visible
            // prompt, but park it invisibly (plumbing must not queue
            // transcript bubbles).
            self.hidden_queue
                .push_back((thread_id, MODEL_LISTING_PROMPT.to_owned()));
            return;
        }
        let submitted = Submitted {
            request_id: crate::history::new_request_id(),
            thread_id,
            prompt: MODEL_LISTING_PROMPT.to_owned(),
        };
        chat.lifecycle = ChatLifecycle::Awaiting {
            request_id: submitted.request_id.clone(),
        };
        self.hidden_requests
            .insert(submitted.request_id.clone(), HiddenPurpose::FetchListing);
        self.picker_submission = Some(submitted);
    }

    /// Drain the staged hidden bundle staged by the key handlers
    /// (`^M` open or Enter selection). The event loop hands it to the
    /// backend source via the normal `send_submitted` path.
    pub fn take_picker_submission(&mut self) -> Option<Submitted> {
        self.picker_submission.take()
    }

    /// Close the picker without acting (Esc). Pending bare-listing fetches
    /// parked in the hidden FIFO are dropped — nobody is waiting for them
    /// anymore — while an already-confirmed `/model <n>` selection stays
    /// queued: that choice was made deliberately and must survive.
    /// Returns whether a picker was actually closed.
    pub fn close_model_picker(&mut self) -> bool {
        if !matches!(self.mode, Mode::ModelPicking { .. }) {
            return false;
        }
        self.hidden_queue
            .retain(|(_, prompt)| prompt != MODEL_LISTING_PROMPT);
        self.mode = Mode::Normal;
        // modal closed → editor pane.
        self.focus = Focus::Chat;
        true
    }

    /// Navigate the picker selection (clamped at the list edges).
    pub fn model_picker_select_next(&mut self) {
        if let Mode::ModelPicking { state } = &mut self.mode {
            if state.selected + 1 < state.entries.len() {
                state.selected += 1;
            }
        }
    }

    pub fn model_picker_select_prev(&mut self) {
        if let Mode::ModelPicking { state } = &mut self.mode {
            state.selected = state.selected.saturating_sub(1);
        }
    }

    /// PgUp in the picker: jump the selection up one page of the popup's
    /// visible list rows (render-fed [`App::picker_visible_rows`], the same
    /// seam the chat pane and the log viewer page by). Clamped at the top
    /// edge: the arrows never wrap, so paging does not either. The
    /// ratatui selection-aware list keeps the landed-on row in view.
    pub fn model_picker_page_up(&mut self) {
        if let Mode::ModelPicking { state } = &mut self.mode {
            let page = self.picker_visible_rows.max(1) as usize;
            state.selected = state.selected.saturating_sub(page);
        }
    }

    /// PgDn in the picker: jump the selection down one page of the popup's
    /// visible list rows, clamped at the last navigable row. No wraparound
    /// (same edge rule as the arrows); the ratatui selection-aware list
    /// keeps the landed-on row in view after the jump.
    pub fn model_picker_page_down(&mut self) {
        if let Mode::ModelPicking { state } = &mut self.mode {
            let page = self.picker_visible_rows.max(1) as usize;
            let last = state.entries.len().saturating_sub(1);
            state.selected = state.selected.saturating_add(page).min(last);
        }
    }

    /// Parsed rows of the open picker (empty while loading/closed).
    pub fn model_picker_entries(&self) -> &[ModelEntry] {
        match &self.mode {
            Mode::ModelPicking { state } => &state.entries,
            _ => &[],
        }
    }

    /// Open the F1 keybindings help modal. Normal
    /// mode only, like every other popup entry point: the modal branches in
    /// `main.rs` return before this could run elsewhere anyway, so the guard
    /// is the single seam that keeps exactly one popup open at a time.
    /// Pure view state: nothing is fetched, nothing is cleared, the draft
    /// and the connection state are untouched.
    pub fn begin_help_modal(&mut self) {
        if !self.mode.is_normal() {
            return;
        }
        self.mode = Mode::HelpViewing {
            state: HelpModalState { scroll: 0 },
        };
    }

    /// Close the help modal (F1 toggle or Esc). Returns whether a modal was
    /// actually closed, mirroring [`App::close_model_picker`].
    pub fn close_help_modal(&mut self) -> bool {
        if !matches!(self.mode, Mode::HelpViewing { .. }) {
            return false;
        }
        self.mode = Mode::Normal;
        // modal closed → editor pane.
        self.focus = Focus::Chat;
        true
    }

    /// Total rendered line count of the static keybindings table (rows +
    /// group headers — the scroll window pages over header lines too).
    fn help_table_len(&self) -> usize {
        crate::ui::help_modal_total_lines()
    }

    /// ↑ or k in the help modal: scroll the row window up one line, clamped
    /// at the top (no wraparound — same edge rule as the picker arrows).
    pub fn help_scroll_up(&mut self) {
        if let Mode::HelpViewing { state } = &mut self.mode {
            state.scroll = state.scroll.saturating_sub(1);
        }
    }

    /// ↓ or j in the help modal: scroll the row window down one line,
    /// clamped so the window never passes the table's last row. The page
    /// size is the render-fed [`App::help_visible_rows`], so the bottom edge
    /// honors the real viewport; before the first render the default of 20
    /// applies (harmless: nothing is on screen yet to scroll).
    pub fn help_scroll_down(&mut self) {
        let page = self.help_visible_rows.max(1) as usize;
        let max_scroll = self.help_table_len().saturating_sub(page);
        if let Mode::HelpViewing { state } = &mut self.mode {
            state.scroll = (state.scroll + 1).min(max_scroll);
        }
    }

    /// PgUp in the help modal: one full page up, clamped at the top (same
    /// render-fed viewport seam as [`App::model_picker_page_up`]).
    pub fn help_page_up(&mut self) {
        if let Mode::HelpViewing { state } = &mut self.mode {
            let page = self.help_visible_rows.max(1) as usize;
            state.scroll = state.scroll.saturating_sub(page);
        }
    }

    /// PgDn in the help modal: one full page down, clamped so the window
    /// never passes the table's last row (same seam as
    /// [`App::model_picker_page_down`], scroll-offset flavor).
    pub fn help_page_down(&mut self) {
        let page = self.help_visible_rows.max(1) as usize;
        let max_scroll = self.help_table_len().saturating_sub(page);
        if let Mode::HelpViewing { state } = &mut self.mode {
            state.scroll = (state.scroll + page).min(max_scroll);
        }
    }

    /// Index of the highlighted row (0 when nothing is selectable yet).
    pub fn model_picker_selected(&self) -> usize {
        match &self.mode {
            Mode::ModelPicking { state } => state.selected,
            _ => 0,
        }
    }

    /// Enter in the picker: close the popup and stage a hidden
    /// `/model <n>` request for the highlighted row's OWN listing number
    /// (the backend validates against the listing numbering, not the
    /// popup's scroll position). Same busy/queue rules as the fetch: an
    /// idle chat gets the bundle staged for immediate send; a busy chat
    /// parks the selection in the hidden FIFO until an idle drain. The
    /// popup always closes — the confirmation arrives later as a status
    /// toast, never as a bubble.
    ///
    /// No-op while the listing is still loading or the list is empty.
    pub fn confirm_model_picker(&mut self) {
        let Mode::ModelPicking { state } = &self.mode else {
            return;
        };
        if state.phase != ModelPickerPhase::Ready || state.entries.is_empty() {
            return;
        }
        let entry = &state.entries[state.selected];
        let prompt = format!("/model {}", entry.number);
        let Some(thread_id) = self.active_thread_id().map(str::to_owned) else {
            return;
        };
        self.mode = Mode::Normal;
        // modal closed → editor pane.
        self.focus = Focus::Chat;

        let Some(chat) = self.chats.iter_mut().find(|c| c.id == thread_id) else {
            return;
        };
        if chat.lifecycle.is_busy() {
            self.hidden_queue.push_back((thread_id, prompt));
            return;
        }
        let submitted = Submitted {
            request_id: crate::history::new_request_id(),
            thread_id,
            prompt,
        };
        chat.lifecycle = ChatLifecycle::Awaiting {
            request_id: submitted.request_id.clone(),
        };
        self.hidden_requests
            .insert(submitted.request_id.clone(), HiddenPurpose::SelectModel);
        self.picker_submission = Some(submitted);
    }

    /// Event-loop drain hook (per-thread async): after a chat's visible FIFO
    /// was given the chance to send its next prompt, hand out the NEXT parked
    /// hidden request for the SAME thread — but only when the chat ended up
    /// Idle (a just-started visible prompt keeps the hidden request parked
    /// for the next drain). Occupies the chat lifecycle without bubbles.
    pub fn take_deferred_hidden_request(&mut self, thread_id: &str) -> Option<Submitted> {
        let (queue_thread, _prompt) = self.hidden_queue.front()?;
        if queue_thread != thread_id {
            return None;
        }
        let chat = self.chats.iter().find(|c| c.id == thread_id)?;
        if chat.lifecycle.is_busy() {
            return None;
        }
        let (_, prompt) = self.hidden_queue.pop_front()?;
        let submitted = Submitted {
            request_id: crate::history::new_request_id(),
            thread_id: thread_id.to_owned(),
            prompt,
        };
        let chat = self
            .chats
            .iter_mut()
            .find(|c| c.id == thread_id)
            .expect("chat existed a line ago");
        chat.lifecycle = ChatLifecycle::Awaiting {
            request_id: submitted.request_id.clone(),
        };
        let purpose = if submitted.prompt == MODEL_LISTING_PROMPT {
            HiddenPurpose::FetchListing
        } else {
            HiddenPurpose::SelectModel
        };
        self.hidden_requests
            .insert(submitted.request_id.clone(), purpose);
        Some(submitted)
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

    /// Number of live subagents for the ACTIVE chat, or `None` when the
    /// spinner line must not show a counter. Independent of the request
    /// lifecycle: background subagents outlive their turn's result frame,
    /// so the last-known count keeps rendering while the chat is idle, and
    /// per-chat storage makes a thread switch show the newly active
    /// thread's own count (0 → hidden) instead of another thread's.
    pub fn active_chat_subagents(&self) -> Option<u64> {
        self.chats
            .get(self.active)
            .and_then(Chat::running_subagents)
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
        // Reset the editor in place, keeping its placeholder.
        self.input = InputArea::default();
        self.input
            .set_placeholder_text("Type a message…  (\u{23ce} send)");

        let chat = self.chats.get_mut(self.active)?;
        if chat.lifecycle.is_busy() {
            // Enqueue into THIS chat's FIFO; other chats are unaffected.
            enqueue_prompt(chat, text);
            // A queued prompt is still fresh user activity: lift the
            // thread to the sidebar top (selection follows the move).
            self.touch_chat(self.active);
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
        // Fresh user activity lifts the thread to the top of the sidebar;
        // after the lift the chat sits at index 0 and stays selected.
        self.touch_chat(self.active);
        if let Some(chat) = self.chats.get_mut(self.active) {
            // A new VISIBLE request starts in THIS chat: its previous turn's
            // reasoning leaves the view. Other chats' thoughts are untouched.
            chat.last_thoughts = None;
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
        // A dequeued prompt is fresh user activity: lift the thread to the
        // sidebar top before touching its transcript (background chats
        // included; the selection of other threads is preserved).
        if let Some(index) = self.chats.iter().position(|c| c.id == thread_id) {
            self.touch_chat(index);
        }
        let chat = self.chats.iter_mut().find(|c| c.id == thread_id)?;
        let prompt = chat.queue.pop_front()?;
        // A dequeued prompt is a VISIBLE request start for THIS chat only:
        // its previous reasoning leaves the view, other chats keep theirs.
        chat.last_thoughts = None;
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
        // a closed modal returns keyboard ownership to
        // the editor pane regardless of where focus was before it appeared
        // (a transport failure can interrupt sidebar navigation).
        self.focus = Focus::Chat;
    }

    // ---- quit confirmation -------------------------------

    /// Open the quit-confirmation popup ("Quit chibi-tui?").
    ///
    /// Deliberately cancel-safe: it flips one flag and touches NOTHING else
    /// — an in-flight request keeps streaming, the draft, focus and any
    /// other open popup stay exactly as they were. The actual exit still
    /// requires an explicit confirm ([`App::confirm_quit`]); dismissal
    /// ([`App::cancel_quit_confirm`]) restores the prior state by doing
    /// nothing at all.
    pub fn begin_quit_confirm(&mut self) {
        self.quit_confirm = true;
    }

    /// Dismiss the quit-confirmation popup. Returns `true` when it was
    /// open (the dispatch uses this for symmetry with the other cancel
    /// helpers; the state itself needs no restore — opening never changed
    /// anything).
    pub fn cancel_quit_confirm(&mut self) -> bool {
        std::mem::take(&mut self.quit_confirm)
    }

    /// Confirm the quit: close the popup and arm the normal shutdown path
    /// (`should_quit` — the event loop performs the graceful backend
    /// shutdown and terminal restore, the same transition every existing
    /// quit path used to trigger directly).
    pub fn confirm_quit(&mut self) {
        self.quit_confirm = false;
        self.should_quit = true;
    }

    /// Clear the whole input buffer and park the cursor at the start
    /// (`Ctrl+L`).
    ///
    /// The editor is rebuilt in place so its placeholder survives; kill-ring
    /// history is intentionally reset too — a "clear" that can be undone by
    /// a stray Ctrl+U surprises more than it helps.
    pub fn clear_input(&mut self) {
        self.input = InputArea::default();
        self.input
            .set_placeholder_text("Type a message…  (\u{23ce} send)");
    }

    // ---- thread delete -------------------------------

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
        self.focus = Focus::Chat; // modal closed → editor pane
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
        // modal closed → editor pane (even into the
        // clean empty state; focus is pane-level state, not selection).
        self.focus = Focus::Chat;
        let chat = self.chats.get(self.active)?;
        let removed_id = chat.id.clone();
        self.chats.remove(self.active);
        // Removed the LAST chat: select_chat clamps the index so the
        // previous one slides into focus; a first/middle removal keeps the
        // index (the old NEXT neighbour). The selection point also clears
        // the marker of whatever thread the user lands on.
        self.select_chat(self.active);
        self.pending_delete = Some(removed_id.clone());
        Some(removed_id)
    }

    // ---- stop / reset --------------------------

    /// Slash command the backend translates into the telegram /stop core
    /// (cancel the thread's running request + subagent counter kill-flush).
    pub const STOP_COMMAND: &str = "/stop";

    /// Slash command the backend intercepts pre-LLM to reset the thread's
    /// history (telegram /reset core).
    pub const RESET_COMMAND: &str = "/reset";

    /// Whether the connected backend advertises `/stop` at handshake. Mocks
    /// and offline sessions never do, so the ^L confirm stays honest there.
    pub fn supports_stop(&self) -> bool {
        self.backend_commands
            .iter()
            .any(|command| command == Self::STOP_COMMAND)
    }

    /// Whether the connected backend advertises `/reset` at handshake.
    pub fn supports_reset(&self) -> bool {
        self.backend_commands
            .iter()
            .any(|command| command == Self::RESET_COMMAND)
    }

    /// Open the stop-confirmation popup for the ACTIVE chat — BUSY ONLY.
    ///
    /// Stopping is meaningless without a turn in flight, so an idle chat is
    /// a silent no-op (the retired ^L screen-wipe taught that a chord that
    /// "does something visible" while idle invites accidental clears; the
    /// disabled-modal alternative adds state for no information). No-op
    /// outside Normal mode (a rename session or another popup owns the
    /// keyboard). An older backend that never advertised `/stop` gets a
    /// transient toast instead of a dead popup.
    pub fn begin_stop_confirm(&mut self) {
        if !self.mode.is_normal() {
            return;
        }
        if !self.supports_stop() {
            self.show_status("backend does not support /stop");
            return;
        }
        let Some(chat) = self.chats.get(self.active) else {
            return;
        };
        if !chat.is_busy() {
            return;
        }
        self.mode = Mode::ConfirmStopReset {
            action: StopResetAction::Stop,
        };
    }

    /// Open the reset-confirmation popup for the ACTIVE chat. Unlike stop,
    /// resetting an idle thread is meaningful (it drops the stored history
    /// and clears the dialog), so only the mode/no-chat guards apply.
    pub fn begin_reset_confirm(&mut self) {
        if !self.mode.is_normal() {
            return;
        }
        if !self.supports_reset() {
            self.show_status("backend does not support /reset");
            return;
        }
        if self.chats.get(self.active).is_none() {
            return;
        }
        self.mode = Mode::ConfirmStopReset {
            action: StopResetAction::Reset,
        };
    }

    /// Leave the stop/reset popup without acting. The active chat, its
    /// messages and the message draft are untouched.
    pub fn cancel_stop_reset(&mut self) -> bool {
        if !matches!(self.mode, Mode::ConfirmStopReset { .. }) {
            return false;
        }
        self.mode = Mode::Normal;
        self.focus = Focus::Chat;
        true
    }

    /// Confirm the popup: stages the `/stop` or `/reset` control request for
    /// the event loop to send OUT-OF-BAND (never through the busy-chat FIFO
    /// — the whole point is to act while a turn is still running). The chat
    /// lifecycle is deliberately untouched here: the chat keeps tracking the
    /// request the backend is about to kill, and that request resolves
    /// through its own `cancelled` error frame.
    pub fn confirm_stop_reset(&mut self) {
        let Mode::ConfirmStopReset { action } = self.mode else {
            return;
        };
        self.mode = Mode::Normal;
        self.focus = Focus::Chat;
        let Some(chat) = self.chats.get(self.active) else {
            return;
        };
        let prompt = match action {
            StopResetAction::Stop => Self::STOP_COMMAND.to_owned(),
            StopResetAction::Reset => Self::RESET_COMMAND.to_owned(),
        };
        let request_id = crate::history::new_request_id();
        let thread_id = chat.id.clone();
        self.pending_control = Some(PendingControl {
            kind: action,
            request_id: request_id.clone(),
            thread_id: thread_id.clone(),
        });
        self.control_submission = Some(Submitted {
            request_id,
            thread_id,
            prompt,
        });
    }

    /// Hand the staged control request to the event loop. One-shot, same
    /// pattern as [`App::take_clone_submission`].
    pub fn take_control_submission(&mut self) -> Option<Submitted> {
        self.control_submission.take()
    }

    /// Resolve a frame that belongs to the pending /stop or /reset control
    /// request. Returns true when the frame was consumed here, so the normal
    /// chat routing never sees it (the chat still tracks the request being
    /// killed and would drop it as an id mismatch anyway; intercepting keeps
    /// that accident-proof).
    ///
    /// Progress frames are consumed silently. The terminal result toasts the
    /// backend's own feedback text ("Everything stopped." / "Done!");
    /// a confirmed reset additionally clears the dialog view of the thread
    /// it reset (messages, FIFO queue, thoughts, subagent counters —
    /// mirroring the backend's thread-scoped history drop; sticky ctx/model
    /// view state survives per the sticky-display contract). Errors surface
    /// through the modal error popup with the backend's own text; a failed
    /// reset never clears the dialog locally.
    fn apply_control_event(&mut self, event: &BackendEvent) -> bool {
        let Some(pending) = &self.pending_control else {
            return false;
        };
        match event {
            BackendEvent::Queued {
                request_id,
                thread_id,
            }
            | BackendEvent::Running {
                request_id,
                thread_id,
            }
            | BackendEvent::AgentProgress {
                request_id,
                thread_id,
                ..
            } => {
                thread_id == &pending.thread_id
                    && event_matches_request(*request_id, &pending.request_id)
            }
            BackendEvent::Result {
                request_id,
                thread_id,
                markdown,
                ..
            } => {
                if thread_id != &pending.thread_id
                    || !event_matches_request(*request_id, &pending.request_id)
                {
                    return false;
                }
                let pending = self.pending_control.take().expect("pending checked above");
                if pending.kind == StopResetAction::Reset {
                    if let Some(chat) = self.chats.iter_mut().find(|c| c.id == pending.thread_id) {
                        chat.messages.clear();
                        chat.queue.clear();
                        chat.last_thoughts = None;
                        chat.subagent_counts.clear();
                        chat.lifecycle = ChatLifecycle::Idle;
                    }
                    if self.active_thread_id() == Some(pending.thread_id.as_str()) {
                        self.scroll = 0;
                    }
                }
                self.show_status(markdown.trim());
                true
            }
            BackendEvent::Error {
                request_id,
                message,
                thread_id,
            } => {
                let ours = thread_id.as_deref() == Some(pending.thread_id.as_str())
                    && event_matches_request(*request_id, &pending.request_id);
                if !ours {
                    return false;
                }
                self.pending_control = None;
                if is_transport_failure(message) {
                    self.connection = Connection::Disconnected;
                }
                self.show_error(message.clone());
                true
            }
            // Not control-related (or no lifecycle frame at all).
            BackendEvent::QueueDrain { .. } | BackendEvent::Disconnected => false,
            // Unreachable: handled by the early returns in apply_backend_event.
            BackendEvent::BackgroundMessage { .. } | BackendEvent::CwdUpdate { .. } => false,
        }
    }

    // ---- thread clone ----------------------------------

    /// Slash command the backend lists in `capabilities.commands` when it can
    /// clone a thread with its full conversation context. Consumed via
    /// detection, never assumed: ^P stays dead until the handshake lists it.
    pub const CLONE_COMMAND: &str = "/new_thread_with_current_context";

    /// record the backend's advertised slash-command set
    /// from the handshake. Stays empty for mocks and placeholder sessions,
    /// which keeps the clone shortcut disabled there.
    pub fn set_backend_commands(&mut self, commands: Vec<String>) {
        self.backend_commands = commands;
    }

    /// Whether the connected backend can clone threads. Mocks and offline
    /// sessions never advertise the command, so this is false for them.
    pub fn supports_thread_clone(&self) -> bool {
        self.backend_commands
            .iter()
            .any(|c| c == Self::CLONE_COMMAND)
    }

    /// Clone the ACTIVE thread with full context inheritance, riding on the
    /// backend's [`Self::CLONE_COMMAND`]. Guard family mirrors the delete
    /// feature: no-op outside Normal mode, informative popup when the
    /// backend lacks the command, transient toast when the source is busy
    /// or a clone is already in flight.
    ///
    /// The clone chat is minted here with a fresh UUID but NOT listed yet:
    /// it waits in `App::pending_clone` until the backend acks, so a
    /// failed request can never leave an orphan thread in the sidebar. The
    /// staged submission goes out through the event loop (same seam as the
    /// model picker's hidden exchange).
    pub fn begin_clone_thread(&mut self) {
        if !self.mode.is_normal() {
            return;
        }
        if !self.supports_thread_clone() {
            self.show_error("backend does not support thread cloning \u{2014} update chibi");
            return;
        }
        if self.pending_clone.is_some() {
            self.show_status("clone already in progress");
            return;
        }
        let Some(source) = self.chats.get(self.active) else {
            return;
        };
        // Delete precedent: refuse while the source has a turn in flight or
        // prompts queued, a half-answered transcript must not be cloned.
        if source.is_busy() || !source.queue.is_empty() {
            self.show_status("can't clone \u{2014} busy");
            return;
        }
        let source_wire = crate::live::wire_thread_id(source.id.as_str());
        // Fresh UUID, copied display mirror. The messages travel in the
        // clone's own JSON snapshot for the sidebar and restarts, while the
        // backend DB stays the context truth (the command re-keys it).
        let mut clone = Chat::new(format!("{} (copy)", source.name));
        clone.messages = source
            .messages
            .iter()
            .map(|m| m.normalized_for_storage())
            .collect();
        // The request rides ON the new thread (mirrors how /reset arrives on
        // the thread it resets); the args carry the source wire id and the
        // clone title for the backend's name registration.
        let request_id = crate::history::new_request_id();
        let submitted = Submitted {
            request_id: request_id.clone(),
            thread_id: clone.id.clone(),
            prompt: format!("{} {} {}", Self::CLONE_COMMAND, source_wire, clone.name),
        };
        self.pending_clone = Some(PendingClone {
            chat: clone,
            request_id,
        });
        self.clone_submission = Some(submitted);
    }

    /// Hand the staged clone request to the event loop. One-shot, same
    /// pattern as [`App::take_picker_submission`].
    pub fn take_clone_submission(&mut self) -> Option<Submitted> {
        self.clone_submission.take()
    }

    /// Resolve a terminal event that belongs to the pending clone request.
    /// Returns true when the event was consumed here, so the normal chat
    /// routing never sees the not-yet-listed thread id (it would be dropped
    /// as unknown and the clone would hang forever).
    fn apply_clone_event(&mut self, event: &BackendEvent) -> bool {
        let Some(pending) = &self.pending_clone else {
            return false;
        };
        match event {
            // Progress frames of the unlisted clone: consumed silently, the
            // chat has no lifecycle to update yet.
            BackendEvent::Queued {
                request_id,
                thread_id,
            }
            | BackendEvent::Running {
                request_id,
                thread_id,
            }
            | BackendEvent::AgentProgress {
                request_id,
                thread_id,
                ..
            } => {
                thread_id == &pending.chat.id
                    && event_matches_request(*request_id, &pending.request_id)
            }
            BackendEvent::Result {
                request_id,
                thread_id,
                ..
            } => {
                if thread_id != &pending.chat.id
                    || !event_matches_request(*request_id, &pending.request_id)
                {
                    return false;
                }
                // Ack: list the clone at the sidebar top (newest-first
                // order: a clone is brand-new activity) and select it.
                let PendingClone { chat, .. } =
                    self.pending_clone.take().expect("pending checked above");
                self.chats.insert(0, chat);
                self.select_chat(0);
                true
            }
            BackendEvent::Error {
                request_id,
                message,
                thread_id,
            } => {
                let ours = thread_id.as_deref() == Some(pending.chat.id.as_str())
                    && event_matches_request(*request_id, &pending.request_id);
                if !ours {
                    return false;
                }
                // Failure: drop the clone (no orphan in the sidebar) and
                // surface the backend's own error text. Transport-level
                // failures flip the indicator like the hidden exchanges do.
                self.pending_clone = None;
                if is_transport_failure(message) {
                    self.connection = Connection::Disconnected;
                }
                self.show_error(message.clone());
                true
            }
            // Not clone-related (or no lifecycle frame at all).
            BackendEvent::QueueDrain { .. } | BackendEvent::Disconnected => false,
            // Unreachable: handled by the early returns in apply_backend_event.
            BackendEvent::BackgroundMessage { .. } | BackendEvent::CwdUpdate { .. } => false,
        }
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

    /// flag a chat that just grew a VISIBLE
    /// reply while the user was looking at another thread. Only content the
    /// user has not seen counts: a blank/pure-ACK absorb and a hidden
    /// plumbing exchange add nothing readable, so they never light the
    /// marker. The flag on the selected chat is meaningless by construction
    /// (select_chat clears it, replies to the selected chat are on screen).
    fn mark_unread_if_background(&mut self, chat_index: usize) {
        if chat_index != self.active {
            if let Some(chat) = self.chats.get_mut(chat_index) {
                chat.unread = true;
            }
        }
    }

    /// Apply an event from the backend source.
    ///
    /// Request-scoped events carry a `thread_id` (per-thread async) and are
    /// routed to THAT chat, matched against its tracked lifecycle request id:
    /// stray events for unknown threads or finished requests are ignored so
    /// they cannot corrupt state or resurrect spinners. Mid-turn subagent
    /// progress is the exception: it is non-terminal and folds into the
    /// owning chat's counters even while the chat is idle (background
    /// subagents outlive their turn's result frame), never resurrecting a
    /// spinner — the lifecycle is untouched. After a terminal
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

        // a terminal event of a pending clone resolves
        // the clone itself and never reaches the chat routing below, which
        // would drop it as an unknown thread id and hang the clone forever.
        if self.apply_clone_event(&event) {
            return;
        }

        // a frame of the pending /stop or /reset
        // control request resolves the control itself and never reaches the
        // chat routing below — the chat keeps tracking the request the
        // control just killed, and the control's own terminal frame must not
        // disturb that lifecycle.
        if self.apply_control_event(&event) {
            return;
        }

        // Out-of-band continuation answer: session-scoped, keyed by the wire
        // thread id, never by a request. It must not pass through the
        // request-lifecycle routing below (the parent request is long over,
        // so the owning chat is usually Idle and would silently drop it).
        if let BackendEvent::BackgroundMessage {
            wire_thread_id,
            markdown,
            model,
            thoughts,
        } = event
        {
            self.apply_background_message(wire_thread_id, markdown, model, thoughts);
            return;
        }

        // Effective-cwd update: session-scoped, keyed by the wire thread
        // id, never by a request. It must not pass through the
        // request-lifecycle routing below (it carries no request id at all).
        if let BackendEvent::CwdUpdate {
            wire_thread_id,
            cwd,
        } = event
        {
            self.apply_cwd_update(wire_thread_id, cwd);
            return;
        }

        // Route to the owning chat by thread id (empty → active chat).
        let route_thread_id: Option<String> = match &event {
            BackendEvent::Queued { thread_id, .. } => Some(thread_id.clone()),
            BackendEvent::Running { thread_id, .. } => Some(thread_id.clone()),
            BackendEvent::AgentProgress { thread_id, .. } => Some(thread_id.clone()),
            BackendEvent::Result { thread_id, .. } => Some(thread_id.clone()),
            BackendEvent::Error { thread_id, .. } => thread_id.clone(),
            // Internal pump signal: handled by the event loop, never here.
            BackendEvent::QueueDrain { .. } => return,
            // Unreachable: handled by the early returns above.
            BackendEvent::BackgroundMessage { .. } | BackendEvent::CwdUpdate { .. } => return,
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

        // mid-turn subagent progress folds into the
        // owning chat's counters regardless of the request state — the
        // frames are non-terminal, background subagents keep their count
        // alive after the turn's result frame (idle chat), and a late
        // kill-flush for an older request still clears its own entry.
        // Keyed by the frame's own request id, so no other entry is touched.
        if let BackendEvent::AgentProgress {
            request_id,
            event,
            active,
            total,
            ..
        } = &event
        {
            self.chats[chat_index].apply_subagent_event(*request_id, *event, *active, *total);
            return;
        }

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
                usage,
                thoughts,
                ..
            } => {
                if event_matches_request(request_id, &tracked_request_id) {
                    // a terminal result whose tracked
                    // id is a hidden exchange is suppressed from the
                    // transcript and resolved into the picker/toast state
                    // instead. The lifecycle still returns to Idle so the
                    // busy rules (and the FIFO drain) keep working.
                    let purpose = self.hidden_requests.remove(&tracked_request_id);
                    //retain the latest-turn protocol metadata before
                    // any resolution path runs. Usage is sticky last-known: a
                    // frame without usage (command results, hidden exchanges)
                    // must not wipe the previous value. The owning chat
                    // mirrors the usage so the thread snapshot persisted
                    // right after this event carries it.
                    if let Some(u) = usage {
                        self.chats[chat_index].last_usage = Some(u);
                        self.last_turn_usage = Some(u);
                    }
                    // Thoughts are sticky per chat: only a VISIBLE result
                    // carrying reasoning extends the owning chat's trace —
                    // hidden exchanges (model picker) and fieldless frames
                    // (command results) never touch it, so the block survives
                    // the plumbing between two LLM answers. Chain members
                    // APPEND (see `retain_thoughts`), never overwrite.
                    if purpose.is_none() {
                        if let Some(t) = thoughts {
                            retain_thoughts(&mut self.chats[chat_index], &t);
                        }
                    }
                    self.chats[chat_index].lifecycle = ChatLifecycle::Idle;
                    match purpose {
                        Some(HiddenPurpose::FetchListing) => {
                            self.resolve_hidden_listing(&markdown, chat_index);
                        }
                        Some(HiddenPurpose::SelectModel) => {
                            self.resolve_hidden_selection(&markdown, model.as_deref(), chat_index);
                        }
                        None => {
                            // a visible reply that
                            // stamps its OWN model label retires any
                            // hidden-switch override: the message-derived
                            // label is the fresher truth again. Fieldless /
                            // ACK resolutions keep the override (no signal
                            // that the model reverted).
                            let stamped = model
                                .as_deref()
                                .map(str::trim)
                                .filter(|m| !m.is_empty())
                                .map(str::to_owned);
                            let thread_id = self.chats[chat_index].id.clone();
                            let chat = &mut self.chats[chat_index];
                            if is_invisible_result(&markdown) {
                                // an empty or pure-ACK answer is
                                // a protocol-level acknowledgement, not a user-facing
                                // reply; absorb it invisibly. The pending placeholder
                                // is dropped (no empty bubble), the lifecycle resolves
                                // to Idle so the active-chat spinner stops cleanly and
                                // re-arms on the next prompt, and NO error/toast fires.
                                // The per-thread queue drain is unaffected: the live
                                // glue emits QueueDrain after every terminal event
                                // regardless of content, so queued prompts still send.
                                drop_live_pending_placeholder(chat);
                            } else {
                                resolve_live_placeholder(chat, markdown, model);
                                // a real reply for a
                                // background thread is content the user has not
                                // seen yet.
                                self.mark_unread_if_background(chat_index);
                            }
                            if let Some(label) = stamped {
                                self.chats[chat_index].last_model = Some(label);
                                self.picker_model_labels.remove(&thread_id);
                            }
                            self.scroll = 0;
                            // A visible reply is fresh thread activity: lift
                            // it to the sidebar top (done last: the reply
                            // handlers above still use the pre-move index).
                            self.touch_chat(chat_index);
                        }
                    }
                }
            }
            BackendEvent::Error {
                message,
                request_id,
                ..
            } => {
                if event_matches_request(request_id, &tracked_request_id) {
                    // hidden exchanges respect
                    // cancel/errors through the error-popup path: there is
                    // no pending placeholder to resolve inline, so the modal
                    // popup (with its `R` reconnect escape) IS the honest
                    // surface. The picker, if still open, closes with it.
                    let purpose = self.hidden_requests.remove(&tracked_request_id);
                    self.chats[chat_index].lifecycle = ChatLifecycle::Idle;
                    if purpose.is_some() {
                        if is_transport_failure(&message) {
                            self.connection = Connection::Disconnected;
                        }
                        if matches!(self.mode, Mode::ModelPicking { .. }) {
                            self.mode = Mode::Normal;
                            self.focus = Focus::Chat;
                        }
                        self.show_error(message);
                        self.scroll = 0;
                        return;
                    }

                    let chat = &mut self.chats[chat_index];
                    resolve_live_placeholder(chat, format!("**Error:** {message}"), None);
                    // an inline failure is also a
                    // reply the user has not seen in a background thread.
                    self.mark_unread_if_background(chat_index);

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
                    // An inline failure is a reply in the thread: lift it to
                    // the sidebar top (after the index-based handling above).
                    self.touch_chat(chat_index);
                }
            }
            // Handled by the early returns above; arms kept for exhaustiveness.
            BackendEvent::AgentProgress { .. }
            | BackendEvent::QueueDrain { .. }
            | BackendEvent::Disconnected => {}
            // Unreachable: handled by the early returns above.
            BackendEvent::BackgroundMessage { .. } | BackendEvent::CwdUpdate { .. } => {}
        }
    }

    /// Apply an effective-working-directory update (`cwd_update` frame) to
    /// the per-thread live-cwd map.
    ///
    /// The update is a NEW value for the thread's agent cwd: it never
    /// touches any chat transcript, lifecycle or sticky state — it only
    /// refreshes the readout the status strip shows for the owning thread
    /// (see [`App::status_cwd`]). Unknown thread ids are still recorded:
    /// the frame is authoritative backend state, and the thread may become
    /// active later in the session.
    fn apply_cwd_update(&mut self, wire_thread_id: i64, cwd: String) {
        self.thread_cwds.insert(wire_thread_id, cwd);
    }

    /// Apply an out-of-band continuation answer (`message` frame) to the
    /// chat that owns the wire thread id.
    ///
    /// The continuation is a NEW assistant message: it never resolves the
    /// chat's pending placeholder (that belongs to whichever request, if
    /// any, is still in flight), never touches the sticky ctx/usage state
    /// (the frame carries no usage), and never changes the lifecycle. A
    /// model label stamps the message AND the chat's last-known model (the
    /// continuation really was produced by that model); reasoning APPENDS to
    /// the chat's per-chat thoughts chain like any other visible answer of
    /// the owning chat. Empty/pure-ACK continuations are absorbed invisibly,
    /// same as results.
    fn apply_background_message(
        &mut self,
        wire_thread_id: i64,
        markdown: String,
        model: Option<String>,
        thoughts: Option<String>,
    ) {
        let Some(chat_index) = self
            .chats
            .iter()
            .position(|c| crate::live::wire_thread_id(&c.id) == wire_thread_id)
        else {
            return; // unknown thread: not ours
        };
        if is_invisible_result(&markdown) {
            return;
        }
        let stamped = model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        let thread_id = self.chats[chat_index].id.clone();
        {
            let chat = &mut self.chats[chat_index];
            let message = match &stamped {
                Some(label) => Message::assistant_with_model(markdown, label.clone()),
                None => Message::assistant(markdown),
            };
            chat.messages.push(message);
            // Per-chat thoughts: a visible answer of the owning chat
            // carrying reasoning is a legitimate writer of its block.
            // Continuation reasoning is a CHAIN member, not a replacement:
            // it APPENDS to the trace (see `retain_thoughts`).
            if let Some(t) = thoughts {
                retain_thoughts(chat, &t);
            }
        }
        if let Some(label) = stamped {
            self.chats[chat_index].last_model = Some(label);
            self.picker_model_labels.remove(&thread_id);
        }
        self.mark_unread_if_background(chat_index);
        self.scroll = 0;
        // A continuation answer is fresh thread activity: lift it to the
        // sidebar top (after the index-based handling above).
        self.touch_chat(chat_index);
    }

    /// resolve a hidden bare-`/model` result.
    ///
    /// * parseable, non-empty listing → the OPEN picker flips to `Ready`
    ///   with best-effort preselection of the chat's last-known model
    ///   (ambiguous or unknown → first row). A result arriving with the
    ///   picker already closed is absorbed silently (plumbing nobody
    ///   awaits).
    /// * unparsable/empty listing → honest degradation while the picker is
    ///   open: info toast + the raw exchange becomes a NORMAL visible chat
    ///   exchange (user bubble `/model` + the backend's raw answer), so the
    ///   user sees reality instead of a silently dead popup. With the
    ///   picker closed the garbage is absorbed silently too.
    fn resolve_hidden_listing(&mut self, markdown: &str, chat_index: usize) {
        let entries = parse_model_listing(markdown);
        if entries.is_empty() {
            if matches!(self.mode, Mode::ModelPicking { .. }) {
                self.mode = Mode::Normal;
                self.focus = Focus::Chat;
                self.show_status(MODEL_LIST_UNAVAILABLE);
                // The fallback is honest, not silent: surface the raw
                // exchange the user actually caused with ^M.
                let chat = &mut self.chats[chat_index];
                chat.messages.push(Message::user(MODEL_LISTING_PROMPT));
                chat.messages.push(Message::assistant(markdown));
                self.scroll = 0;
            }
            return;
        }
        // Best-effort preselection against the active chat's last-known
        // model label (hidden-switch override first, then the
        // message metadata); ambiguity → none.
        // Computed BEFORE the mutable borrow of the picker state below.
        let label = self.last_known_model_label(chat_index);
        let Mode::ModelPicking { state } = &mut self.mode else {
            return; // picker closed meanwhile: absorb silently
        };
        state.phase = ModelPickerPhase::Ready;
        state.entries = entries;
        state.selected = preselect_model_index(&state.entries, label.as_deref()).unwrap_or(0);
    }

    /// resolve a hidden `/model <n>` confirmation
    /// into the compact status toast (never a bubble; same suppression as
    /// the fetch). Unrecognized bodies fall back to the raw text so a
    /// backend wording change degrades visibly instead of lying.
    ///
    /// Task interplay: the switch ALSO updates the chat's last-known model
    /// metadata (a session-scoped override — see
    /// [`App::picker_model_labels`]) so the status strip reflects it with
    /// zero coupling. The wire `model` field wins when the backend stamps
    /// it (same source visible replies use); the confirmation text tail is
    /// the honest fallback when it does not.
    fn resolve_hidden_selection(&mut self, markdown: &str, model: Option<&str>, chat_index: usize) {
        let label =
            parse_selection_confirmation(markdown).unwrap_or_else(|| markdown.trim().to_owned());
        if label.is_empty() {
            self.show_status("model switched");
        } else {
            self.show_status(format!("model: {label}"));
        }
        let stamped = model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned)
            .or(if label.is_empty() { None } else { Some(label) });
        if let Some(label) = stamped {
            let thread_id = self.chats[chat_index].id.clone();
            self.chats[chat_index].last_model = Some(label.clone());
            self.picker_model_labels.insert(thread_id, label);
        }
    }

    /// the chat's best-effort last-known model label
    /// for picker preselection — the hidden-switch override first (a
    /// just-made `/model <n>` switch leaves no transcript bubble to derive
    /// from), then the message-label metadata.
    fn last_known_model_label(&self, chat_index: usize) -> Option<String> {
        let chat = &self.chats[chat_index];
        self.picker_model_labels.get(&chat.id).cloned().or_else(|| {
            chat.messages
                .iter()
                .rev()
                .find_map(|m| m.model_label())
                .map(str::to_owned)
        })
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

    // ---- mouse text selection --------------------------------------------

    /// Start a drag selection: the press point becomes BOTH the anchor and
    /// the head. Called from the mouse router for a left press inside the
    /// chat pane (Normal mode, no error popup).
    pub fn begin_selection(&mut self, point: SelectionPoint) {
        self.selection = Some(ChatSelection {
            anchor: point,
            head: point,
            dragging: true,
        });
    }

    /// Extend the live drag to `point` (moves the head). A no-op without a
    /// live drag — a drag event after a release must not resurrect the
    /// selection.
    pub fn drag_selection(&mut self, point: SelectionPoint) {
        if let Some(sel) = &mut self.selection {
            if sel.dragging {
                sel.head = point;
            }
        }
    }

    /// Finalize the drag (mouse release). A REAL selection (anchor ≠ head)
    /// is kept on screen and its plain text returned for the clipboard
    /// copy; a press+release without drag is a PLAIN CLICK: the selection
    /// clears and nothing is copied. `None` without a live drag.
    pub fn release_selection(&mut self) -> Option<String> {
        if !self.selection.as_ref().is_some_and(|sel| sel.dragging) {
            return None;
        }
        let sel = self.selection.as_mut().expect("live drag checked above");
        sel.dragging = false;
        if sel.anchor == sel.head {
            self.selection = None; // plain click
            return None;
        }
        self.selection_text()
    }

    /// Clear the selection (Esc / a plain click / a thread switch).
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Plain text of the current selection: chars of the wrapped rows
    /// between the ordered endpoints, CONCATENATED across wrap points of
    /// the same logical line and newline-SEPARATED at real line breaks
    /// (a change of the owning logical line). The geometry must belong to
    /// the active thread, else `None` (the one-iteration staleness guard).
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        let (lo, hi) = sel.ordered();
        let geom = self.chat_geometry.as_ref()?;
        if Some(geom.chat_id.as_str()) != self.active_thread_id() {
            return None;
        }
        let last = geom.rows.len().saturating_sub(1);
        let (lo_row, hi_row) = (lo.row.min(last), hi.row.min(last));
        let mut out = String::new();
        let mut prev_end: Option<(usize, usize)> = None; // (logical, row end)
        for r in lo_row..=hi_row {
            let meta = &geom.rows[r];
            match prev_end {
                // Newline at real line breaks only — never at wrap points.
                Some((prev_logical, _)) if prev_logical != meta.logical => out.push('\n'),
                // Same logical line: the wrap may have dropped break space
                // between the rows (a gap in the char ranges) — restore one
                // space so words never glue together in the copy.
                Some((_, prev_row_end)) if meta.start > prev_row_end => out.push(' '),
                _ => {}
            }
            let chars: Vec<char> = meta.text.chars().collect();
            let start = if r == lo_row {
                lo.col.min(chars.len())
            } else {
                0
            };
            let end = if r == hi_row {
                hi.col.min(chars.len())
            } else {
                chars.len()
            };
            if end > start {
                out.extend(&chars[start..end]);
            }
            prev_end = Some((meta.logical, meta.end));
        }
        Some(out).filter(|text| !text.is_empty())
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
/// text of an iterator of messages.
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
/// `model` is stamped ONLY onto this one message at
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
/// is this answer content invisible-by-contract?
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
///. Targets the same row [`resolve_live_placeholder`]
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

/// Retain one more member of the chat's reasoning chain.
///
/// A turn's reasoning arrives as a CHAIN of payloads: the terminal result
/// frame carries the request's own accumulated trace, and every background
/// continuation answer (`message` frame) adds its reasoning delta
/// afterwards. Each payload APPENDS to the retained trace (newline-joined)
/// instead of replacing it, so the whole chain stays visible above the
/// latest answer; the renderer's trailing-window cap
/// (`ui::THOUGHTS_DISPLAY_LINES`) bounds the block. The chain resets where
/// a new visible request starts in THIS chat (`begin_request` /
/// `dequeue_next_for`) or on /reset. Whitespace-only payloads never touch
/// the retained trace: they must neither extend it nor wipe it.
fn retain_thoughts(chat: &mut Chat, thoughts: &str) {
    let piece = thoughts.trim();
    if piece.is_empty() {
        return;
    }
    match &chat.last_thoughts {
        Some(prev) if !prev.trim().is_empty() => {
            chat.last_thoughts = Some(format!("{prev}\n{piece}"));
        }
        _ => chat.last_thoughts = Some(piece.to_owned()),
    }
}

/// Does a numeric [`BackendEvent`] id refer to the tracked protocol request?
/// The live glue task derives event ids from the protocol UUID
/// (see `crate::live::submitted_event_id`).
fn event_matches_request(event_id: u64, tracked_request_id: &str) -> bool {
    event_id == crate::live::wire_thread_id(tracked_request_id) as u64
}

/// best-effort preselection index — the single parsed
/// row whose name equals (case-insensitively) the chat's last-known model
/// label. Ambiguous (duplicate names across providers are common in real
/// listings) or unknown labels yield `None` → the picker starts at row 0.
fn preselect_model_index(entries: &[ModelEntry], label: Option<&str>) -> Option<usize> {
    let label = label?.trim().to_lowercase();
    if label.is_empty() {
        return None;
    }
    let hits: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.name.trim().to_lowercase() == label)
        .map(|(i, _)| i)
        .collect();
    if hits.len() == 1 {
        hits.into_iter().next()
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
