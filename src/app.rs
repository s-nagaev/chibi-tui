//! Application state: chats, selection, input buffer, request lifecycle.

use std::collections::{HashMap, VecDeque};

use ratatui::layout::{Position, Rect};

use crate::backend::BackendEvent;
use crate::diag::LogEntry;
use crate::markdown;
use crate::model::{ChatLifecycle, Message};
use crate::model_picker::{parse_model_listing, parse_selection_confirmation, ModelEntry};
use crate::popup::ErrorPopup;
use crate::protocol::{AgentEventKind, Usage};
use crate::theme::Theme;
use tui_textarea::TextArea;
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
        let mut input = TextArea::default();
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
    /// transports are [`crate::clipboard::copy_text`]: OSC 52 first, plus
    /// the `CHIBI_TUI_COPY_CMD` fallback when set. The header gets a brief
    /// feedback note either way (`copied` / `copy: unavailable`).
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
        // Reset the TextArea in place, keeping its placeholder.
        self.input = TextArea::default();
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

    // ---- helpers ---------------------------------------------------------

    fn app_with_chats(n: usize) -> App {
        let chats = (0..n)
            .map(|i| Chat::new(format!("chat-{i}")))
            .collect::<Vec<_>>();
        App::new(chats)
    }

    /// Current sidebar index of the chat named `name`. Activity lifts
    /// threads to the top, so tests must look chats up by their stable
    /// name instead of by a position captured earlier.
    fn idx(app: &App, name: &str) -> usize {
        app.chats
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("chat {name} not in the sidebar"))
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

    // ---- latest-turn usage/thoughts retention -------

    fn sample_usage() -> Usage {
        Usage {
            input_tokens: 120,
            output_tokens: 45,
            context_window: Some(200_000),
        }
    }

    #[test]
    fn result_retains_latest_turn_usage_and_thoughts() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "hello");
        assert_eq!(app.last_turn_usage, None, "no turn yet");
        app.apply_backend_event(BackendEvent::Result {
            usage: Some(sample_usage()),
            thoughts: Some("thinking...".into()),
            request_id: event_id_of(&submitted.request_id),
            markdown: "answer".into(),
            thread_id: submitted.thread_id,
            model: None,
        });
        assert_eq!(app.last_turn_usage, Some(sample_usage()));
        assert_eq!(app.chats[0].last_thoughts.as_deref(), Some("thinking..."));
    }
    /// A CHAIN of thoughts — the turn's terminal result plus every
    /// background continuation's reasoning delta — must ACCUMULATE in the
    /// chat's retained trace (append, never overwrite), so every chain
    /// member stays visible above the latest answer. Regression for the
    /// owner report: with plain overwrite only one payload of the chain
    /// was ever retained.
    #[test]
    fn thought_chain_accumulates_across_result_and_continuations() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "do the thing");
        let thread_id = submitted.thread_id.clone();
        app.apply_backend_event(BackendEvent::Result {
            request_id: event_id_of(&submitted.request_id),
            markdown: "step one done".into(),
            thread_id: thread_id.clone(),
            model: None,
            usage: None,
            thoughts: Some("first thought".into()),
        });
        assert_eq!(
            app.chats[0].last_thoughts.as_deref(),
            Some("first thought"),
            "the result frame seeds the chain"
        );

        // Two continuation answers, each carrying its reasoning delta: the
        // chain must GROW, not replace.
        for (markdown, thought) in [
            ("step two done", "second thought"),
            ("step three done", "third thought"),
        ] {
            app.apply_backend_event(BackendEvent::BackgroundMessage {
                wire_thread_id: crate::live::wire_thread_id(&thread_id),
                markdown: markdown.into(),
                model: None,
                thoughts: Some(thought.into()),
            });
        }
        assert_eq!(
            app.chats[0].last_thoughts.as_deref(),
            Some("first thought\nsecond thought\nthird thought"),
            "every chain member must be retained in arrival order"
        );

        // The next visible request in THIS chat still resets the chain.
        let _next = submit_text(&mut app, "again");
        assert_eq!(
            app.chats[0].last_thoughts, None,
            "a new visible request clears the accumulated chain"
        );
    }

    /// A whitespace-only thoughts payload must neither extend the retained
    /// chain nor wipe it: the block the user is reading survives blanks.
    #[test]
    fn blank_thoughts_payload_never_wipes_the_chain() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "hello");
        let thread_id = submitted.thread_id.clone();
        app.apply_backend_event(BackendEvent::Result {
            request_id: event_id_of(&submitted.request_id),
            markdown: "answer".into(),
            thread_id: thread_id.clone(),
            model: None,
            usage: None,
            thoughts: Some("real reasoning".into()),
        });
        app.apply_backend_event(BackendEvent::BackgroundMessage {
            wire_thread_id: crate::live::wire_thread_id(&thread_id),
            markdown: "continuation".into(),
            model: None,
            thoughts: Some("   \n  ".into()),
        });
        assert_eq!(
            app.chats[0].last_thoughts.as_deref(),
            Some("real reasoning"),
            "a blank payload must not destroy the retained chain"
        );
    }

    #[test]
    fn new_request_start_keeps_usage_and_clears_thoughts() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "hello");
        app.apply_backend_event(BackendEvent::Result {
            usage: Some(sample_usage()),
            thoughts: Some("thinking...".into()),
            request_id: event_id_of(&submitted.request_id),
            markdown: "answer".into(),
            thread_id: submitted.thread_id,
            model: None,
        });
        assert!(app.last_turn_usage.is_some());
        submit_text(&mut app, "next prompt");
        assert_eq!(
            app.last_turn_usage,
            Some(sample_usage()),
            "usage survives a new request start"
        );
        assert_eq!(
            app.chats[0].last_thoughts, None,
            "cleared on new request start"
        );
    }

    #[test]
    fn dequeued_prompt_keeps_usage_and_clears_thoughts() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "hello");
        let thread_id = submitted.thread_id.clone();
        // Queue a follow-up while the chat is busy (this does NOT start it).
        type_in(&mut app, "queued prompt");
        assert!(app.take_input().is_none(), "busy chat enqueues, no start");
        app.apply_backend_event(BackendEvent::Result {
            usage: Some(sample_usage()),
            thoughts: Some("thinking...".into()),
            request_id: event_id_of(&submitted.request_id),
            markdown: "answer".into(),
            thread_id,
            model: None,
        });
        assert!(
            app.last_turn_usage.is_some(),
            "terminal result retains the turn"
        );
        let thread_id = app.chats[0].id.clone();
        app.dequeue_next_for(&thread_id)
            .expect("queued prompt drained");
        assert_eq!(
            app.last_turn_usage,
            Some(sample_usage()),
            "usage survives a dequeued request start"
        );
        assert_eq!(
            app.chats[0].last_thoughts, None,
            "cleared on dequeued start"
        );
    }

    // sticky per-thread reasoning -------------------

    /// Deliver a terminal Result carrying reasoning to a chat's tracked
    /// request (the full thoughts payload a real LLM turn brings).
    fn finish_turn_with_thoughts(app: &mut App, index: usize, thoughts: &str) {
        app.select_chat(index);
        let submitted = submit_text(app, "turn prompt");
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: Some(thoughts.to_owned()),
            request_id: event_id_of(&submitted.request_id),
            markdown: "answer".into(),
            thread_id: submitted.thread_id,
            model: None,
        });
    }

    /// A result frame routed to a BACKGROUND chat lands in that chat's own
    /// reasoning mirror only — the chat the user is reading keeps its own
    /// (stale) state, and no global field exists for a background reply to
    /// pollute.
    #[test]
    fn background_result_routes_thoughts_to_the_owning_chat() {
        let mut app = app_with_chats(2);
        finish_turn_with_thoughts(&mut app, 0, "alpha reasoning");

        app.select_chat(idx(&app, "chat-1"));
        let background = submit_text(&mut app, "background question");
        app.select_chat(idx(&app, "chat-0"));
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: Some("beta reasoning".into()),
            request_id: event_id_of(&background.request_id),
            markdown: "beta answer".into(),
            thread_id: background.thread_id,
            model: None,
        });

        assert_eq!(
            app.chats[idx(&app, "chat-1")].last_thoughts.as_deref(),
            Some("beta reasoning"),
            "the owning chat mirrors its own reasoning"
        );
        assert_eq!(
            app.chats[idx(&app, "chat-0")].last_thoughts.as_deref(),
            Some("alpha reasoning"),
            "the viewed chat must not show a background chat's thoughts"
        );
    }

    /// A visible request start clears THIS chat's reasoning only — another
    /// chat mid-turn (or holding its last trace) is untouched.
    #[test]
    fn new_visible_request_clears_only_the_owning_chats_thoughts() {
        let mut app = app_with_chats(2);
        finish_turn_with_thoughts(&mut app, 0, "alpha reasoning");
        finish_turn_with_thoughts(&mut app, 1, "beta reasoning");

        app.select_chat(idx(&app, "chat-0"));
        submit_text(&mut app, "second question");

        assert_eq!(
            app.chats[idx(&app, "chat-0")].last_thoughts,
            None,
            "a new request start clears the requesting chat's thoughts"
        );
        assert_eq!(
            app.chats[idx(&app, "chat-1")].last_thoughts.as_deref(),
            Some("beta reasoning"),
            "other chats' thoughts are not the requester's business"
        );
    }

    /// A dequeued prompt is a visible request start for ITS chat only.
    #[test]
    fn dequeued_prompt_clears_only_that_chats_thoughts() {
        let mut app = app_with_chats(2);
        finish_turn_with_thoughts(&mut app, 0, "alpha reasoning");

        app.select_chat(idx(&app, "chat-1"));
        let first = submit_text(&mut app, "first");
        type_in(&mut app, "queued behind it");
        assert!(app.take_input().is_none(), "busy chat enqueues");
        app.select_chat(idx(&app, "chat-0"));
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: Some("beta reasoning".into()),
            request_id: event_id_of(&first.request_id),
            markdown: "beta answer".into(),
            thread_id: first.thread_id,
            model: None,
        });

        let background_thread = app.chats[idx(&app, "chat-1")].id.clone();
        let drained = app
            .dequeue_next_for(&background_thread)
            .expect("queued prompt drained");
        assert_eq!(drained.prompt, "queued behind it");
        assert_eq!(
            app.chats[idx(&app, "chat-1")].last_thoughts,
            None,
            "the dequeued start clears that chat's thoughts"
        );
        assert_eq!(
            app.chats[idx(&app, "chat-0")].last_thoughts.as_deref(),
            Some("alpha reasoning"),
            "the other chat keeps its trace"
        );
    }

    /// A hidden exchange (model-picker listing) resolves through the same
    /// request pipeline but must not touch the chat's sticky thoughts — not
    /// even when the frame hypothetically carries a thoughts field.
    #[test]
    fn hidden_exchange_never_touches_sticky_thoughts() {
        let mut app = app_with_chats(1);
        finish_turn_with_thoughts(&mut app, 0, "alpha reasoning");

        app.begin_model_picker();
        let bundle = app.take_picker_submission().expect("fetch staged");
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: Some("picker reasoning leak".into()),
            request_id: event_id_of(&bundle.request_id),
            markdown: "1. glm-5.2\n2. kimi-k2.7".into(),
            thread_id: bundle.thread_id,
            model: None,
        });

        assert_eq!(
            app.chats[0].last_thoughts.as_deref(),
            Some("alpha reasoning"),
            "a hidden exchange must never touch the visible thoughts"
        );
    }

    // ---- sticky last-known display state (ctx + model) --------------------

    /// Submit a prompt on the ACTIVE chat and resolve it with a labelled LLM
    /// answer carrying usage (the full latest-turn metadata a real chat turn
    /// brings).
    fn finish_llm_turn(app: &mut App, model: &str, usage: Usage) {
        let submitted = submit_text(app, "turn prompt");
        app.apply_backend_event(BackendEvent::Result {
            usage: Some(usage),
            thoughts: None,
            request_id: event_id_of(&submitted.request_id),
            markdown: "**answer**".into(),
            thread_id: submitted.thread_id,
            model: Some(model.to_owned()),
        });
    }

    #[test]
    fn command_frame_keeps_last_known_ctx_and_model() {
        let mut app = app_with_chats(1);
        finish_llm_turn(&mut app, "glm-5.2", sample_usage());

        // A visible command exchange: the answer frame carries neither usage
        // nor model (command results are backend plumbing, not LLM turns).
        let command = submit_text(&mut app, "/help");
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of(&command.request_id),
            markdown: "available commands: /help /reset".into(),
            thread_id: command.thread_id,
            model: None,
        });

        assert_eq!(
            app.last_turn_usage,
            Some(sample_usage()),
            "ctx keeps the last known usage across the command frame"
        );
        assert_eq!(
            app.active_model_label(),
            Some("glm-5.2"),
            "panel keeps the last known model across the command frame"
        );
        let messages = &app.chats[0].messages;
        assert_eq!(
            messages[messages.len() - 1].model_label(),
            None,
            "the command answer carries no annotation"
        );
        assert_eq!(
            messages[messages.len() - 3].model_label(),
            Some("glm-5.2"),
            "the LLM answer keeps its own annotation"
        );
    }

    #[test]
    fn model_change_mid_session_updates_panel_on_next_result() {
        let mut app = app_with_chats(1);
        finish_llm_turn(&mut app, "glm-5.2", sample_usage());
        assert_eq!(app.active_model_label(), Some("glm-5.2"));

        let switched = Usage {
            input_tokens: 900,
            output_tokens: 10,
            context_window: Some(131_072),
        };
        finish_llm_turn(&mut app, "kimi-k2.7", switched);

        assert_eq!(
            app.active_model_label(),
            Some("kimi-k2.7"),
            "panel follows the next frame that carries a model"
        );
        assert_eq!(
            app.last_turn_usage,
            Some(switched),
            "ctx follows the next frame that carries usage"
        );
    }

    #[test]
    fn fresh_session_starts_with_unknown_ctx_and_model() {
        let app = app_with_chats(1);
        assert_eq!(app.last_turn_usage, None, "no usage before any frame");
        assert_eq!(
            app.active_model_label(),
            None,
            "no model label before any frame"
        );
        assert!(
            app.chats[0]
                .messages
                .iter()
                .all(|m| m.model_label().is_none()),
            "no annotation anywhere in a fresh session"
        );
    }

    #[test]
    fn restart_restores_sticky_display_state_from_persisted_snapshot() {
        let mut app = app_with_chats(1);
        finish_llm_turn(&mut app, "glm-5.2", sample_usage());
        assert!(app.last_turn_usage.is_some());
        assert_eq!(app.active_model_label(), Some("glm-5.2"));

        // Restart seam: the event loop persists the chat and a new App loads
        // it. Per-message labels travel with their messages, while the
        // thread-level last-known usage/model pair rides along with the
        // snapshot and seeds the display state.
        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-sticky-restart-{}-{}",
            std::process::id(),
            crate::history::new_thread_id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        crate::history::save_chat_in(Some(&dir), &app.chats[0]).expect("save");
        let restored = crate::history::load_chats_from(Some(&dir));
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(restored.len(), 1, "one snapshot file");
        assert_eq!(
            restored[0].messages.last().and_then(|m| m.model_label()),
            Some("glm-5.2"),
            "the answer's label persists with its message"
        );
        assert_eq!(
            restored[0].last_usage,
            Some(sample_usage()),
            "last-known usage persists with the thread"
        );
        assert_eq!(
            restored[0].last_model.as_deref(),
            Some("glm-5.2"),
            "last-known model persists with the thread"
        );

        let fresh = App::new(restored);
        assert_eq!(
            fresh.last_turn_usage,
            Some(sample_usage()),
            "ctx segment is seeded from the persisted usage"
        );
        assert_eq!(
            fresh.active_model_label(),
            Some("glm-5.2"),
            "panel model is seeded from the persisted label"
        );
        assert_eq!(
            fresh.chats[0].messages.last().and_then(|m| m.model_label()),
            Some("glm-5.2"),
            "restored answers keep their captured labels"
        );
    }

    // restore activation -------------------------

    /// A restored pointer must behave exactly like the user picking the
    /// thread: the selection moves, and the sticky ctx segment is re-seeded
    /// from THAT thread's snapshot (not the first chat's). A dangling id
    /// changes nothing at all.
    #[test]
    fn activate_thread_opens_restored_thread_and_seeds_its_sticky_state() {
        let mut alpha = Chat::new("alpha");
        alpha.last_usage = Some(Usage {
            input_tokens: 10,
            output_tokens: 1,
            context_window: Some(100_000),
        });
        let mut beta = Chat::new("beta");
        beta.last_usage = Some(Usage {
            input_tokens: 900_000,
            output_tokens: 1,
            context_window: Some(1_000_000),
        });
        beta.last_model = Some("glm-5.2".to_owned());

        let mut app = App::new(vec![alpha, beta]);
        assert_eq!(app.active, 0, "default startup selects the first chat");
        assert_eq!(
            app.last_turn_usage.map(|u| u.input_tokens),
            Some(10),
            "ctx segment seeded from the first chat"
        );

        let restored_id = app.chats[1].id.clone();
        app.activate_thread(&restored_id);
        assert_eq!(app.active_thread_id(), Some(restored_id.as_str()));
        assert_eq!(app.chat_title(), "beta");
        assert_eq!(
            app.last_turn_usage.map(|u| u.input_tokens),
            Some(900_000),
            "ctx segment re-seeded from the restored thread's snapshot"
        );
        assert_eq!(
            app.active_model_label(),
            Some("glm-5.2"),
            "panel model staged for the restored thread"
        );

        // A dangling pointer is a silent no-op: selection and sticky state
        // are untouched.
        app.activate_thread(&crate::history::new_thread_id());
        assert_eq!(app.active_thread_id(), Some(restored_id.as_str()));
        assert_eq!(app.chat_title(), "beta");
        assert_eq!(app.last_turn_usage.map(|u| u.input_tokens), Some(900_000));
    }

    #[test]
    fn switching_threads_updates_ctx_value() {
        let mut alpha = Chat::new("alpha");
        alpha.last_usage = Some(Usage {
            input_tokens: 10,
            output_tokens: 1,
            context_window: Some(100_000),
        });
        let mut beta = Chat::new("beta");
        beta.last_usage = Some(Usage {
            input_tokens: 900_000,
            output_tokens: 1,
            context_window: Some(1_000_000),
        });

        let mut app = App::new(vec![alpha, beta]);
        assert_eq!(
            app.last_turn_usage.map(|u| u.input_tokens),
            Some(10),
            "ctx segment seeded from the startup chat"
        );

        app.select_next();
        assert_eq!(
            app.last_turn_usage.map(|u| u.input_tokens),
            Some(900_000),
            "switching threads re-seeds the ctx segment from the entered thread"
        );

        app.select_prev();
        assert_eq!(
            app.last_turn_usage.map(|u| u.input_tokens),
            Some(10),
            "switching back re-seeds from the entered thread again"
        );
    }

    #[test]
    fn switching_to_thread_without_usage_shows_neutral_ctx() {
        let mut alpha = Chat::new("alpha");
        alpha.last_usage = Some(Usage {
            input_tokens: 10,
            output_tokens: 1,
            context_window: Some(100_000),
        });
        let mut beta = Chat::new("beta");
        // Deterministic order: alpha on top, beta behind (newest-first).
        beta.updated_at = alpha.updated_at.saturating_sub(1);

        let mut app = App::new(vec![alpha, beta]);
        assert_eq!(app.last_turn_usage.map(|u| u.input_tokens), Some(10));

        app.select_next();
        assert_eq!(
            app.last_turn_usage, None,
            "a thread without usage resets the ctx segment to neutral, not stale"
        );

        app.select_prev();
        assert_eq!(
            app.last_turn_usage.map(|u| u.input_tokens),
            Some(10),
            "switching back restores the carrying thread's value"
        );
    }

    #[test]
    fn usage_frame_after_switch_still_updates_ctx() {
        let mut alpha = Chat::new("alpha");
        alpha.last_usage = Some(Usage {
            input_tokens: 10,
            output_tokens: 1,
            context_window: Some(100_000),
        });
        let mut beta = Chat::new("beta");
        // Deterministic order: alpha on top, beta behind (newest-first).
        beta.updated_at = alpha.updated_at.saturating_sub(1);

        let mut app = App::new(vec![alpha, beta]);
        // Selections go by NAME: activity lifts threads to the top, so
        // positional arrows are not stable here.
        app.select_chat(idx(&app, "beta"));
        assert_eq!(app.last_turn_usage, None, "neutral after the switch");

        finish_llm_turn(&mut app, "glm-5.2", sample_usage());
        assert_eq!(
            app.last_turn_usage,
            Some(sample_usage()),
            "an incoming usage frame overwrites the ctx segment after a switch"
        );

        app.select_chat(idx(&app, "alpha"));
        assert_eq!(
            app.last_turn_usage.map(|u| u.input_tokens),
            Some(10),
            "switching away reads the per-thread mirror, not the sticky field"
        );
        app.select_chat(idx(&app, "beta"));
        assert_eq!(
            app.last_turn_usage,
            Some(sample_usage()),
            "the live turn's usage is mirrored onto its thread for later switches"
        );
    }

    #[test]
    fn terminal_turn_persists_last_known_usage_and_model() {
        let mut app = app_with_chats(1);
        finish_llm_turn(&mut app, "glm-5.2", sample_usage());

        // The event-loop persist seam (terminal events and the final
        // snapshot before shutdown both go through save_chat_in) must carry
        // the thread's last-known pair into the JSON document itself.
        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-persist-last-{}-{}",
            std::process::id(),
            crate::history::new_thread_id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = crate::history::save_chat_in(Some(&dir), &app.chats[0]).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read snapshot");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(
            doc["last_model"], "glm-5.2",
            "model recorded on the terminal turn: {raw}"
        );
        assert_eq!(
            doc["last_usage"]["input_tokens"], 120,
            "usage recorded on the terminal turn: {raw}"
        );
        assert_eq!(doc["last_usage"]["output_tokens"], 45);
        assert_eq!(doc["last_usage"]["context_window"], 200_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_change_is_reflected_in_the_snapshot_on_next_persist() {
        let mut app = app_with_chats(1);
        finish_llm_turn(&mut app, "glm-5.2", sample_usage());
        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-model-switch-{}-{}",
            std::process::id(),
            crate::history::new_thread_id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        crate::history::save_chat_in(Some(&dir), &app.chats[0]).expect("first save");

        let switched = Usage {
            input_tokens: 900,
            output_tokens: 10,
            context_window: Some(131_072),
        };
        finish_llm_turn(&mut app, "kimi-k2.7", switched);
        crate::history::save_chat_in(Some(&dir), &app.chats[0]).expect("resave");

        let loaded = crate::history::load_chats_from(Some(&dir));
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(loaded.len(), 1, "same thread id, same snapshot file");
        assert_eq!(
            loaded[0].last_model.as_deref(),
            Some("kimi-k2.7"),
            "the next persist carries the switched model"
        );
        assert_eq!(
            loaded[0].last_usage,
            Some(switched),
            "the next persist carries the switched turn's usage"
        );
    }

    /// [`finish_chat`] with a model label.
    fn finish_chat_with_model(app: &mut App, index: usize, model: Option<&str>) {
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

    /// Deliver a mid-turn `agent_event` to `index`'s tracked request id
    /// (same correlation path the live glue task uses).
    fn agent_event(app: &mut App, index: usize, event: AgentEventKind, active: u64, total: u64) {
        let request_id = app.chats[index].lifecycle.request_id().unwrap().to_owned();
        agent_frame(app, index, event_id_of(&request_id), event, active, total);
    }

    /// Deliver a mid-turn `agent_event` carrying an arbitrary numeric
    /// request id — the shape of late frames (kill-flush, kept-request
    /// overlap) whose id differs from the chat's currently tracked request.
    fn agent_frame(
        app: &mut App,
        index: usize,
        request_id: u64,
        event: AgentEventKind,
        active: u64,
        total: u64,
    ) {
        let thread_id = app.chats[index].id.clone();
        app.apply_backend_event(BackendEvent::AgentProgress {
            request_id,
            thread_id,
            event,
            active,
            total,
        });
    }

    #[test]
    fn subagent_events_aggregate_started_update_finished() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "spawn helpers");

        agent_event(&mut app, 0, AgentEventKind::Started, 2, 5);
        assert_eq!(app.active_chat_subagents(), Some(2));
        assert!(
            matches!(app.chats[0].lifecycle, ChatLifecycle::Awaiting { .. }),
            "mid-turn progress must never touch the lifecycle"
        );

        // A second spawn within the same request updates the entry in place.
        agent_event(&mut app, 0, AgentEventKind::Started, 3, 5);
        assert_eq!(app.active_chat_subagents(), Some(3));

        // Some subagents finish but the request keeps live ones.
        agent_event(&mut app, 0, AgentEventKind::Finished, 1, 5);
        assert_eq!(app.active_chat_subagents(), Some(1));

        // The final finish reports active == 0: the entry is removed.
        agent_event(&mut app, 0, AgentEventKind::Finished, 0, 5);
        assert!(
            app.chats[0].subagent_counts.is_empty(),
            "entry removed at active == 0"
        );
        assert_eq!(app.active_chat_subagents(), None);
    }

    #[test]
    fn subagent_counter_shows_only_the_active_chats_count() {
        let mut app = app_with_chats(2);
        submit_text(&mut app, "active request");
        app.active = 1;
        submit_text(&mut app, "background request");

        // Both chats run concurrently with their own counters (sidebar
        // positions may have shifted — look them up by name).
        let a = idx(&app, "chat-0");
        let b = idx(&app, "chat-1");
        agent_event(&mut app, a, AgentEventKind::Started, 2, 4);
        agent_event(&mut app, b, AgentEventKind::Started, 1, 4);

        // The active chat (chat-1) sees only its own counter…
        app.select_chat(b);
        assert_eq!(app.active_chat_subagents(), Some(1));
        // …and switching back to chat-0 sees only chat-0's.
        app.select_chat(a);
        assert_eq!(app.active_chat_subagents(), Some(2));
    }

    #[test]
    fn subagent_counter_outlives_the_result_frame_and_folds_late_frames() {
        let mut app = app_with_chats(1);
        let first = submit_text(&mut app, "first");
        agent_event(&mut app, 0, AgentEventKind::Started, 2, 4);
        assert_eq!(app.active_chat_subagents(), Some(2));

        // Terminal event → idle: the answer is rendered, but two background
        // subagents of the turn still run. The counter must stay visible
        // independently of the request lifecycle (the reported bug).
        finish_chat(&mut app, 0);
        assert!(matches!(app.chats[0].lifecycle, ChatLifecycle::Idle));
        assert_eq!(
            app.active_chat_subagents(),
            Some(2),
            "counter must outlive the result frame while subagents run"
        );

        // A LATE frame for the finished request folds under its own request
        // id — the count follows live and never touches another entry.
        agent_frame(
            &mut app,
            0,
            event_id_of(&first.request_id),
            AgentEventKind::Finished,
            1,
            4,
        );
        assert_eq!(app.active_chat_subagents(), Some(1));

        // The final finish reports active == 0: the entry is removed and
        // the idle chat's counter disappears.
        agent_frame(
            &mut app,
            0,
            event_id_of(&first.request_id),
            AgentEventKind::Finished,
            0,
            4,
        );
        assert_eq!(
            app.active_chat_subagents(),
            None,
            "the count dropping to 0 hides the counter"
        );
        assert!(app.chats[0].subagent_counts.is_empty());
    }

    #[test]
    fn subagent_counter_sums_live_subagents_across_thread_requests() {
        let mut app = app_with_chats(1);
        let first = submit_text(&mut app, "first");
        agent_event(&mut app, 0, AgentEventKind::Started, 2, 4);
        finish_chat(&mut app, 0);

        // A new request starts while the previous turn's subagents still
        // run: frames fold per request id, the display sums the thread.
        let second = submit_text(&mut app, "second");
        agent_event(&mut app, 0, AgentEventKind::Started, 1, 1);
        assert_eq!(
            app.active_chat_subagents(),
            Some(3),
            "the old request's live subagents plus the new request's"
        );

        // The old request's late finish removes only its own entry.
        agent_frame(
            &mut app,
            0,
            event_id_of(&first.request_id),
            AgentEventKind::Finished,
            0,
            4,
        );
        assert_eq!(app.active_chat_subagents(), Some(1));

        agent_frame(
            &mut app,
            0,
            event_id_of(&second.request_id),
            AgentEventKind::Finished,
            0,
            1,
        );
        assert_eq!(app.active_chat_subagents(), None);
    }

    #[test]
    fn subagent_counter_follows_the_thread_switched_to() {
        let mut app = app_with_chats(2);
        submit_text(&mut app, "work");
        agent_event(&mut app, 0, AgentEventKind::Started, 2, 4);
        finish_chat(&mut app, 0);
        assert_eq!(app.active_chat_subagents(), Some(2));

        // Switching to a thread without subagents hides the indicator —
        // the idle thread must not show another thread's count.
        app.active = 1;
        assert_eq!(
            app.active_chat_subagents(),
            None,
            "a thread with no subagents renders no counter"
        );

        app.active = 0;
        assert_eq!(
            app.active_chat_subagents(),
            Some(2),
            "switching back restores the owning thread's count"
        );
    }

    #[test]
    fn subagent_counter_with_zero_active_renders_nothing() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "work");
        agent_event(&mut app, 0, AgentEventKind::Started, 0, 3);
        assert_eq!(
            app.active_chat_subagents(),
            None,
            "active == 0 → no counter"
        );
    }

    // creation / switching --------------------------------

    #[test]
    fn new_chat_inserts_on_top_selects_and_gets_fresh_uuid() {
        let mut app = app_with_chats(2);
        let existing_ids: Vec<String> = app.chats.iter().map(|c| c.id.clone()).collect();
        app.new_chat();
        assert_eq!(app.chats.len(), 3);
        assert_eq!(app.active, 0, "the fresh chat must be selected");
        assert_eq!(app.chat_title(), "New chat 3");
        let fresh = &app.chats[0];
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

    // Chat ↔ Sidebar focus state ----------------------

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

    /// ^N new-chat is editor-bound: creating a thread
    /// always lands focus back on Chat, whether the chord came from either
    /// pane.
    #[test]
    fn new_chat_lands_focus_on_chat() {
        let mut app = app_with_chats(2);
        app.focus = Focus::Sidebar;
        app.new_chat();
        assert_eq!(app.focus, Focus::Chat);
        assert_eq!(app.active, 0, "new chat is selected (and on top)");
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

    // per-message labelling --------------------

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

    // ^S toggle ----------------------------------------

    /// The thoughts block starts visible (ON by contract) and toggling
    /// round-trips without ever touching the retained reasoning.
    #[test]
    fn thoughts_toggle_defaults_on_and_round_trips() {
        let mut app = app_with_chats(1);
        assert!(app.thoughts_visible, "thoughts must start visible (ON)");
        app.chats[0].last_thoughts = Some("step by step".into());
        app.toggle_thoughts();
        assert!(!app.thoughts_visible);
        assert_eq!(
            app.chats[0].last_thoughts.as_deref(),
            Some("step by step"),
            "toggle is render-only: thoughts must survive it"
        );
        app.toggle_thoughts();
        assert!(app.thoughts_visible);
    }

    // strip state + segments --------------------------

    /// Default visible; ^O flips visibility round-trip.
    #[test]
    fn status_strip_starts_visible_and_toggles_round_trip() {
        let mut app = app_with_chats(1);
        assert!(
            app.status_strip_visible,
            "strip must start visible (task contract)"
        );
        app.toggle_status_strip();
        assert!(!app.status_strip_visible);
        app.toggle_status_strip();
        assert!(app.status_strip_visible);
    }

    /// View state like Focus: opening and closing every modal family must
    /// leave the visibility flag untouched.
    #[test]
    fn status_strip_visibility_survives_modal_open_close() {
        let mut app = app_with_chats(1);

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

    /// The cwd segment is the workspace root's path TAIL: the last three
    /// components with a leading `/`; shorter paths show what they have
    /// and pathless roots degrade to the raw string.
    #[test]
    fn status_cwd_is_the_workspace_path_tail() {
        let mut app = app_with_chats(1);
        assert_eq!(app.status_cwd(), None, "unwired root yields None");

        app.workspace_root = Some("/Users/sergio/Develop/personal/chibi-tui".into());
        assert_eq!(
            app.status_cwd().as_deref(),
            Some("/Develop/personal/chibi-tui")
        );

        // Exactly three components: the whole path behind the prefix.
        app.workspace_root = Some("/home/sergio/chibi-tui".into());
        assert_eq!(app.status_cwd().as_deref(), Some("/home/sergio/chibi-tui"));

        app.workspace_root = Some("/Users/sergio/".into());
        assert_eq!(
            app.status_cwd().as_deref(),
            Some("/Users/sergio"),
            "trailing slash tolerated"
        );

        app.workspace_root = Some("/chibi-tui".into());
        assert_eq!(app.status_cwd().as_deref(), Some("/chibi-tui"));

        app.workspace_root = Some("/".into());
        assert_eq!(
            app.status_cwd().as_deref(),
            Some("/"),
            "pathless root degrades to raw"
        );

        app.workspace_root = Some(".".into());
        assert_eq!(app.status_cwd().as_deref(), Some("."));
    }

    /// The cwd segment prefers the LIVE effective cwd the backend reported
    /// for the ACTIVE thread via `cwd_update` frames, falls back to the CLI
    /// workspace root when the thread has no report yet, and follows the
    /// thread when the active chat switches.
    #[test]
    fn status_cwd_prefers_live_thread_cwd() {
        let mut app = app_with_chats(2);
        app.workspace_root = Some("/Users/sergio/Develop/personal/chibi-tui".into());
        let active_wire = crate::live::wire_thread_id(app.active_thread_id().unwrap());

        // A report for an unrelated thread must not move the readout.
        app.apply_backend_event(BackendEvent::CwdUpdate {
            wire_thread_id: active_wire.wrapping_add(99),
            cwd: "/elsewhere".into(),
        });
        assert_eq!(
            app.status_cwd().as_deref(),
            Some("/Develop/personal/chibi-tui"),
            "no report for the active thread yet → workspace fallback"
        );

        // The active thread's live report wins over the workspace root.
        app.apply_backend_event(BackendEvent::CwdUpdate {
            wire_thread_id: active_wire,
            cwd: "/Users/sergio/Develop/personal/chibi".into(),
        });
        assert_eq!(
            app.status_cwd().as_deref(),
            Some("/Develop/personal/chibi"),
            "live thread cwd takes precedence"
        );

        // Switching to a chat with no report falls back, and its own report
        // takes over once it arrives.
        app.select_chat(1);
        assert_eq!(
            app.status_cwd().as_deref(),
            Some("/Develop/personal/chibi-tui"),
            "the entered thread has no report → workspace fallback"
        );
        let other_wire = crate::live::wire_thread_id(app.active_thread_id().unwrap());
        app.apply_backend_event(BackendEvent::CwdUpdate {
            wire_thread_id: other_wire,
            cwd: "/Users/sergio/other".into(),
        });
        assert_eq!(app.status_cwd().as_deref(), Some("/Users/sergio/other"));
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

    /// [`finish_chat_with_model`] with arbitrary answer content.
    fn finish_chat_with_content(app: &mut App, index: usize, markdown: &str) {
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
        app.select_chat(idx(&app, "chat-1")); // the "background" one
        submit_text(&mut app, "background question");
        app.select_chat(idx(&app, "chat-0")); // foreground stays empty

        {
            let i = idx(&app, "chat-1");
            finish_chat_with_content(&mut app, i, ACK_MARKER);
        }

        assert_eq!(
            app.chats[idx(&app, "chat-1")].messages.len(),
            1,
            "no bubble in bg chat"
        );
        assert!(app.chats[idx(&app, "chat-1")]
            .messages
            .iter()
            .all(|m| !m.pending));
        assert_eq!(
            app.chats[idx(&app, "chat-1")].lifecycle,
            ChatLifecycle::Idle
        );
        assert!(
            app.chats[idx(&app, "chat-0")].messages.is_empty(),
            "foreground untouched"
        );
        assert!(app.error_popup.is_none());
    }

    // state --------------------------------

    /// A visible reply landing in a BACKGROUND chat flags it unread; the
    /// selected chat's own reply never flags anything. The editor draft of
    /// the other thread stays untouched by the arrival.
    #[test]
    fn background_result_marks_chat_unread_and_active_result_does_not() {
        let mut app = app_with_chats(2);
        submit_text(&mut app, "background work"); // chat 0 goes busy
        app.select_next(); // user reads chat 1 while chat 0 works
        type_in(&mut app, "draft in flight, do not disturb");
        assert!(!app.chats[0].unread, "nothing arrived yet");

        finish_chat(&mut app, 0);
        assert!(app.chats[0].unread, "background reply must flag the thread");
        assert!(!app.chats[1].unread);
        assert_eq!(
            app.input.lines().join(""),
            "draft in flight, do not disturb",
            "mid-typing draft untouched by the background arrival"
        );

        // The active chat's own reply is on screen: no flag. (Submitting
        // lifts chat-1 to the top, so look it up by name, not position.)
        submit_text(&mut app, "foreground work");
        {
            let i = idx(&app, "chat-1");
            finish_chat(&mut app, i);
        }
        assert!(
            !app.chats[idx(&app, "chat-1")].unread,
            "selected chat never flags itself"
        );
        assert!(
            app.chats[idx(&app, "chat-0")].unread,
            "other flag survives unrelated traffic"
        );
    }

    /// Selecting the flagged thread clears its marker (the single
    /// select_chat point behind both arrows), and passing a flagged thread
    /// by without entering it keeps the flag.
    #[test]
    fn selecting_the_thread_clears_its_unread_marker() {
        let mut app = app_with_chats(3);
        submit_text(&mut app, "work in chat 0");
        app.select_next(); // active = 1
        app.select_next(); // active = 2
        finish_chat(&mut app, 0);
        assert!(app.chats[0].unread);

        app.select_prev(); // into chat 1, not the flagged one
        assert!(app.chats[0].unread, "passing by must not clear the flag");
        app.select_prev(); // INTO chat 0
        assert!(
            !app.chats[0].unread,
            "entering the thread clears its marker"
        );

        // Re-flag, then clear via the other arrow direction.
        submit_text(&mut app, "work again in chat 0");
        app.select_next(); // chat 1
        finish_chat(&mut app, 0);
        assert!(app.chats[0].unread);
        app.select_prev(); // back into chat 0
        assert!(!app.chats[0].unread);
    }

    /// The global-search jump activates the target thread through the same
    /// selection point, so its marker clears on entry too.
    #[test]
    fn global_search_jump_clears_the_target_thread_marker() {
        let mut app = app_with_chats(2);
        app.chats[1]
            .messages
            .push(Message::assistant("needle here"));
        // Flag chat 1 the way a background reply would (white-box: the
        // routing that sets the flag is covered by the dedicated tests).
        app.chats[1].unread = true;

        app.begin_search_all();
        for ch in "needle".chars() {
            app.search_all_push(ch);
        }
        assert_eq!(app.search_all_matches()[0].chat_index, 1);
        assert!(app.jump_to_selected_all());
        assert_eq!(app.active, 1, "target thread activated");
        assert!(
            !app.chats[1].unread,
            "entering via the search jump clears the flag"
        );
    }

    /// Only readable content flags a background thread: a blank/pure-ACK
    /// absorb and a hidden plumbing exchange stay unflagged, while an
    /// inline error reply does flag.
    #[test]
    fn unread_flag_follows_visible_content_only() {
        // Blank/ACK absorb: nothing to read, no flag.
        let mut app = app_with_chats(2);
        submit_text(&mut app, "bg blank");
        app.select_next();
        finish_chat_with_content(&mut app, 0, ACK_MARKER);
        assert!(!app.chats[0].unread, "absorbed ACK must not flag");

        // Hidden exchange (model listing fetch) started on chat 0, then the
        // user switches away before it resolves: plumbing nobody reads,
        // no flag, the picker still resolves.
        let mut app = app_with_chats(2);
        open_picker(&mut app);
        let req = app.chats[0].lifecycle.request_id().unwrap().to_owned();
        app.select_next();
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of(&req),
            markdown: CAPTURED_LISTING.into(),
            thread_id: app.chats[0].id.clone(),
            model: None,
        });
        assert!(
            !app.chats[0].unread,
            "hidden plumbing exchange must not flag"
        );
        assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);

        // An inline error reply IS content the user has not seen.
        let mut app = app_with_chats(2);
        let submitted = submit_text(&mut app, "bg doomed");
        app.select_next();
        app.apply_backend_event(BackendEvent::Error {
            request_id: event_id_of(&submitted.request_id),
            message: "backend exploded".into(),
            thread_id: Some(app.chats[0].id.clone()),
        });
        assert!(app.chats[0].unread, "background error reply flags");
    }

    /// A clone ack lists and selects the new thread through the single
    /// selection point; the fresh copy starts with no marker.
    #[test]
    fn clone_insert_starts_without_unread_marker() {
        let mut app = clone_capable_app(1);
        app.begin_clone_thread();
        let submitted = app.take_clone_submission().expect("staged");
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of(&submitted.request_id),
            markdown: "ack".to_owned(),
            thread_id: submitted.thread_id,
            model: None,
        });
        assert_eq!(app.active, 0, "clone selected");
        assert!(!app.chats[0].unread, "fresh clone carries no marker");
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
        app.select_chat(idx(&app, "chat-1"));
        let second = submit_text(&mut app, "parallel in B");
        assert_ne!(first.thread_id, second.thread_id);
        assert!(matches!(
            app.chats[idx(&app, "chat-0")].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));
        assert!(matches!(
            app.chats[idx(&app, "chat-1")].lifecycle,
            ChatLifecycle::Awaiting { .. }
        ));
        assert!(
            app.chats[idx(&app, "chat-0")].queue.is_empty(),
            "A's queue untouched"
        );
        assert!(
            app.chats[idx(&app, "chat-1")].queue.is_empty(),
            "B started immediately"
        );

        // B's answer lands first and only touches B.
        let second_req = second.request_id.clone();
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of(&second_req),
            markdown: "B done".into(),
            thread_id: second.thread_id.clone(),
            model: None,
        });
        assert_eq!(
            app.chats[idx(&app, "chat-0")].messages.len(),
            2,
            "A still pending"
        );
        assert_eq!(
            app.chats[idx(&app, "chat-1")].messages[1].markdown,
            "B done"
        );
        assert_eq!(
            app.chats[idx(&app, "chat-1")].lifecycle,
            ChatLifecycle::Idle
        );
        assert!(
            matches!(
                app.chats[idx(&app, "chat-0")].lifecycle,
                ChatLifecycle::Awaiting { .. }
            ),
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
            usage: None,
            thoughts: None,
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
            usage: None,
            thoughts: None,
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
            ids.insert(app.chats.first().unwrap().id.clone());
        }
        assert_eq!(ids.len(), 50, "every new chat gets a unique thread id");
    }

    // error popup ------------------------------------------

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

    // clear input (Ctrl+L) ---------------------------------

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

    // local cancel fallback ---------------------------------

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

    // transport-failure escalation --------------------------

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

    //------------------------------------------------------------------------

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

    // busy-chat rename ------------------------------

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

    // editor block height helper ---------------------

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

    // confirm popup state logic --------------------

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

    // detection, guards, ack/error resolution -------

    fn clone_capable_app(n: usize) -> App {
        let mut app = app_with_chats(n);
        app.set_backend_commands(vec![
            "/reset".to_owned(),
            "/new_thread_with_current_context".to_owned(),
        ]);
        app
    }

    #[test]
    fn clone_without_capability_shows_informative_popup() {
        let mut app = app_with_chats(1);
        assert!(!app.supports_thread_clone(), "nothing advertised yet");
        app.begin_clone_thread();

        let popup = app.error_popup.as_ref().expect("informative popup");
        assert!(
            popup.message.contains("does not support thread cloning"),
            "popup must explain the missing capability: {popup:?}"
        );
        assert!(app.take_clone_submission().is_none(), "nothing staged");
        assert!(app.pending_clone.is_none(), "no orphan flight state");
        assert_eq!(app.chats.len(), 1, "sidebar unchanged");
    }

    #[test]
    fn clone_gate_ignores_unrelated_commands() {
        let mut app = app_with_chats(1);
        app.set_backend_commands(vec!["/reset".to_owned(), "/model".to_owned()]);
        assert!(!app.supports_thread_clone());
        app.begin_clone_thread();

        assert!(app.error_popup.is_some(), "still the unsupported path");
        assert!(app.take_clone_submission().is_none());
    }

    #[test]
    fn clone_stages_request_with_wire_shape_and_copied_name() {
        let mut app = clone_capable_app(1);
        app.chats[0].name = "My thread".to_owned();
        let source_id = app.active_thread_id().unwrap().to_owned();
        app.begin_clone_thread();

        let submitted = app.take_clone_submission().expect("staged");
        let pending = app.pending_clone.as_ref().expect("clone in flight");
        // Name defaulting: "<name> (copy)".
        assert_eq!(pending.chat.name, "My thread (copy)");
        // Fresh identity: a new UUID, never the source's.
        assert_ne!(pending.chat.id, source_id);
        assert_eq!(pending.chat.lifecycle, ChatLifecycle::Idle);
        assert!(pending.chat.queue.is_empty(), "queue is never copied");
        // The request rides ON the new chat (its UUID is the frame thread
        // id); the args carry the source wire id and the clone title.
        assert_eq!(submitted.thread_id, pending.chat.id);
        assert_eq!(
            submitted.prompt,
            format!(
                "{} {} My thread (copy)",
                App::CLONE_COMMAND,
                crate::live::wire_thread_id(&source_id)
            )
        );
        assert_eq!(submitted.request_id, pending.request_id);

        // Single-flight guard: a re-press while in flight must not mint a
        // second clone.
        app.begin_clone_thread();
        assert!(app.take_clone_submission().is_none());
        assert!(app.status_message.is_some(), "second press gets a toast");
    }

    #[test]
    fn clone_refused_while_source_has_turn_in_flight() {
        let mut app = clone_capable_app(1);
        submit_text(&mut app, "long running");
        app.begin_clone_thread();

        assert!(
            app.error_popup.is_none(),
            "refusal follows the delete precedent: toast, not popup"
        );
        assert!(app.status_message.is_some(), "busy toast shown");
        assert!(app.take_clone_submission().is_none());
        assert!(app.pending_clone.is_none());
        assert_eq!(app.chats.len(), 1);
    }

    #[test]
    fn clone_refused_while_source_has_queued_prompts() {
        let mut app = clone_capable_app(1);
        app.chats[0].queue.push_back("queued prompt".to_owned());
        app.begin_clone_thread();

        assert!(app.status_message.is_some(), "busy toast covers the queue");
        assert!(app.take_clone_submission().is_none());
    }

    #[test]
    fn clone_noop_outside_normal_mode() {
        let mut app = clone_capable_app(1);
        app.mode = Mode::ConfirmDelete;
        app.begin_clone_thread();

        assert!(app.take_clone_submission().is_none());
        assert!(app.error_popup.is_none());
        assert!(app.status_message.is_none());
    }

    #[test]
    fn clone_ack_inserts_on_top_selects_and_copies_mirror() {
        let mut app = clone_capable_app(3);
        app.select_next(); // active = 1 (chat-1)
        app.chats[1].messages.push(Message::user("question"));
        app.chats[1].messages.push(Message::assistant("**answer**"));
        app.begin_clone_thread();
        let submitted = app.take_clone_submission().expect("staged");
        assert_eq!(app.chats.len(), 3, "clone stays unlisted before the ack");

        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of(&submitted.request_id),
            markdown: "Thread cloned: chat-1 (copy) (ID: 42). 2 messages copied.".to_owned(),
            thread_id: submitted.thread_id,
            model: None,
        });

        assert_eq!(app.chats.len(), 4);
        assert_eq!(
            app.chats[0].name, "chat-1 (copy)",
            "clone listed on top (newest-first sidebar)"
        );
        assert_eq!(app.chats[0].messages.len(), 2, "display mirror copied");
        assert_eq!(app.active, 0, "clone selected");
        assert_eq!(app.scroll, 0, "follow-bottom for the new chat");
        assert!(app.pending_clone.is_none(), "flight state cleared");
    }

    #[test]
    fn clone_progress_frames_are_consumed_while_unlisted() {
        let mut app = clone_capable_app(1);
        app.begin_clone_thread();
        let submitted = app.take_clone_submission().expect("staged");

        app.apply_backend_event(BackendEvent::Queued {
            request_id: event_id_of(&submitted.request_id),
            thread_id: submitted.thread_id.clone(),
        });
        app.apply_backend_event(BackendEvent::Running {
            request_id: event_id_of(&submitted.request_id),
            thread_id: submitted.thread_id.clone(),
        });

        assert!(app.pending_clone.is_some(), "still awaiting the ack");
        assert!(
            app.chats.iter().all(|c| c.lifecycle == ChatLifecycle::Idle),
            "no listed chat tracks the clone request"
        );
    }

    #[test]
    fn clone_ack_with_stray_ids_is_ignored() {
        let mut app = clone_capable_app(1);
        app.begin_clone_thread();
        let submitted = app.take_clone_submission().expect("staged");

        // Wrong request id, right thread: not ours.
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of("unrelated-request"),
            markdown: "ack".to_owned(),
            thread_id: submitted.thread_id.clone(),
            model: None,
        });
        // Right request id, wrong thread: not ours either.
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of(&submitted.request_id),
            markdown: "ack".to_owned(),
            thread_id: "unrelated-thread".to_owned(),
            model: None,
        });

        assert_eq!(app.chats.len(), 1, "no listing without a matching ack");
        assert!(app.pending_clone.is_some(), "clone stays in flight");
        assert!(app.error_popup.is_none());
    }

    #[test]
    fn clone_error_drops_clone_and_surfaces_backend_text() {
        let mut app = clone_capable_app(1);
        app.begin_clone_thread();
        let submitted = app.take_clone_submission().expect("staged");

        app.apply_backend_event(BackendEvent::Error {
            request_id: event_id_of(&submitted.request_id),
            message: "Source thread 123 is busy. Wait for it to finish before cloning.".to_owned(),
            thread_id: Some(submitted.thread_id),
        });

        assert!(app.pending_clone.is_none(), "clone dropped, no orphan");
        assert_eq!(app.chats.len(), 1, "sidebar unchanged");
        let popup = app.error_popup.as_ref().expect("backend error surfaced");
        assert!(
            popup.message.contains("Wait for it to finish"),
            "the backend's own text is shown: {popup:?}"
        );
        assert!(app.take_clone_submission().is_none());
    }

    #[test]
    fn clone_error_for_other_requests_never_touches_the_clone() {
        let mut app = clone_capable_app(1);
        app.begin_clone_thread();
        let _submitted = app.take_clone_submission().expect("staged");

        app.apply_backend_event(BackendEvent::Error {
            request_id: event_id_of("unrelated-request"),
            message: "some other failure".to_owned(),
            thread_id: None,
        });

        assert!(app.pending_clone.is_some(), "clone untouched");
        assert!(app.error_popup.is_none(), "no popup for a foreign error");
    }

    #[test]
    fn clone_ack_lists_on_top_even_if_source_deleted() {
        let mut app = clone_capable_app(2);
        app.begin_clone_thread();
        let submitted = app.take_clone_submission().expect("staged");
        // Source removed while the clone request is in flight.
        app.chats.remove(0);

        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of(&submitted.request_id),
            markdown: "ack".to_owned(),
            thread_id: submitted.thread_id,
            model: None,
        });

        assert_eq!(app.chats.len(), 2);
        assert_eq!(
            app.chats[0].name, "chat-0 (copy)",
            "source gone: clone still lists on top (newest-first)"
        );
        assert_eq!(app.active, 0);
    }

    #[test]
    fn clone_persists_via_existing_history_layer() {
        let mut app = clone_capable_app(1);
        app.chats[0].messages.push(Message::user("question"));
        app.begin_clone_thread();
        let submitted = app.take_clone_submission().expect("staged");
        app.apply_backend_event(BackendEvent::Result {
            usage: None,
            thoughts: None,
            request_id: event_id_of(&submitted.request_id),
            markdown: "ack".to_owned(),
            thread_id: submitted.thread_id,
            model: None,
        });

        // Event-loop seam: the loop persists the selected chat after the
        // event; the clone must survive a restart through the same layer.
        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-clone-restart-{}-{}",
            std::process::id(),
            crate::history::new_thread_id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        crate::history::save_chat_in(Some(&dir), &app.chats[app.active]).expect("save");

        let restored = crate::history::load_chats_from(Some(&dir));
        assert_eq!(restored.len(), 1, "exactly one snapshot file");
        assert_eq!(restored[0].name, "chat-0 (copy)");
        assert_eq!(restored[0].messages.len(), 1, "mirror survives restart");
        assert_eq!(restored[0].lifecycle, ChatLifecycle::Idle);
        assert!(restored[0].queue.is_empty());
        std::fs::remove_dir_all(&dir).ok();
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

    // popup state logic ----------------------------

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
            usage: None,
            thoughts: None,
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
            usage: None,
            thoughts: None,
            request_id: event_id_of(&bg_req),
            markdown: "**done**".into(),
            thread_id: bg.thread_id.clone(),
            model: None,
        });
        assert_eq!(app.chats[0].messages[1].markdown, "**done**");
        assert_eq!(app.chats[0].lifecycle, ChatLifecycle::Idle);
    }

    // popup state logic ----------------------

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

    // log viewer state ---------------------------

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

    /// Search semantics at the state level:
    /// commit jumps to the nearest match at or after the cursor, n/N walk
    /// with wraparound, and a snapshot refresh reindexes while keeping the
    /// current hit when it still exists.
    #[test]
    fn log_search_commit_jump_wraparound_and_reindex() {
        let lines = vec![
            "alpha one".to_owned(),
            "beta ALPHA two".to_owned(),
            "gamma".to_owned(),
            "alpha three".to_owned(),
        ];
        let mk = |cursor: usize, lines: Vec<String>, search: Option<LogSearch>| Mode::LogViewer {
            state: LogViewerState {
                cursor,
                wrap: false,
                row_offset: 0,
                lines: lines
                    .into_iter()
                    .map(crate::diag::LogEntry::parse)
                    .collect(),
                snapshot_total: crate::diag::total_appended(),
                search_buf: None,
                search,
                copy_note: None,
                copy_note_at: None,
            },
        };

        // Commit from the middle: the cursor jumps DOWN to the nearest hit.
        let mut app = App::new(Vec::new());
        app.mode = mk(2, lines.clone(), None);
        app.log_open_search();
        for ch in "alpha".chars() {
            app.log_search_push(ch);
        }
        app.log_commit_search();
        let state = log_state(&app);
        assert_eq!(state.search.as_ref().unwrap().matches, vec![0, 1, 3]);
        assert_eq!(state.search.as_ref().unwrap().current, None);
        assert_eq!(state.cursor, 3, "jumped to the nearest match >= cursor");

        // n wraps from the last hit back to the first.
        app.log_search_next();
        let state = log_state(&app);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.search.as_ref().unwrap().current, Some(0));

        // N goes back (wrapping to the tail hit).
        app.log_search_prev();
        let state = log_state(&app);
        assert_eq!(state.cursor, 3);
        assert_eq!(state.search.as_ref().unwrap().current, Some(2));

        // A snapshot refresh (tail re-arm) reindexes the same pattern; the
        // current hit survives while its ordinal stays valid.
        app.mode = mk(
            0,
            vec!["alpha again".to_owned(), "unrelated".to_owned()],
            Some(LogSearch {
                pattern: "alpha".to_owned(),
                matches: vec![0, 3, 7],
                current: Some(2),
            }),
        );
        if let Mode::LogViewer { state } = &mut app.mode {
            state.reindex_search();
        }
        let state = log_state(&app);
        assert_eq!(state.search.as_ref().unwrap().matches, vec![0]);
        assert_eq!(
            state.search.as_ref().unwrap().current,
            None,
            "ordinal beyond the new list drops the current hit"
        );

        // An empty pattern commit switches the search off.
        app.log_open_search();
        app.log_commit_search();
        assert!(log_state(&app).search.is_none(), "empty pattern = off");
    }

    /// y copies the full logical line and leaves the brief header feedback
    /// behind (hermetic: hand-built state, no diag stream involvement).
    #[test]
    fn log_copy_selected_sets_feedback_note() {
        let mut app = App::new(Vec::new());
        app.mode = Mode::LogViewer {
            state: LogViewerState {
                cursor: 1,
                wrap: true,
                row_offset: 0,
                lines: vec![
                    "first".to_owned(),
                    "second line with the content to copy".to_owned(),
                ]
                .into_iter()
                .map(crate::diag::LogEntry::parse)
                .collect(),
                snapshot_total: crate::diag::total_appended(),
                search_buf: None,
                search: None,
                copy_note: None,
                copy_note_at: None,
            },
        };
        app.log_copy_selected();
        let state = log_state(&app);
        assert_eq!(state.copy_note.as_deref(), Some("copied"));
        assert!(state.copy_note_at.is_some(), "expiry anchor recorded");

        // The note expires: after its window the header is clean again.
        if let Mode::LogViewer { state } = &mut app.mode {
            state.copy_note_at = Some(
                std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_secs(3))
                    .expect("clock moved backwards"),
            );
            state.expire_copy_note();
        }
        assert!(log_state(&app).copy_note.is_none());
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
        assert!(
            state.at_tail(),
            "opens live-tailing: cursor on the newest line"
        );
        assert!(
            state.lines.iter().any(|e| e.text == *marker),
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

        // Closing while PINNED (cursor above the tail) does not mark: the
        // missed lines stay unseen.
        let mut app = App::new(Vec::new());
        for i in 0..10 {
            crate::diag::append(format!("filler-{i}"));
        }
        app.begin_log_viewer();
        let seen_at_open = app.log_seen_total;
        app.log_cursor_up(5);
        assert!(
            !log_state(&app).at_tail(),
            "precondition: cursor up pinned the view away from the tail"
        );
        crate::diag::append("missed-while-pinned");
        assert!(app.close_log_viewer());
        assert_eq!(
            app.log_seen_total, seen_at_open,
            "close while pinned must not mark the missed lines seen"
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

        // Already-open viewer: re-open must not reset the cursor.
        let mut app = app_with_chats(0);
        for i in 0..12 {
            crate::diag::append(format!("filler-{i}"));
        }
        app.begin_log_viewer();
        app.log_cursor_up(7);
        let pinned_at = log_state(&app).cursor;
        app.begin_log_viewer();
        assert_eq!(
            log_state(&app).cursor,
            pinned_at,
            "re-open does not clobber"
        );
    }

    #[test]
    fn log_navigation_pins_and_return_to_bottom_re_arms_live_tail() {
        let mut app = app_with_chats(0);
        for i in 0..40 {
            crate::diag::append(format!("filler-{i}"));
        }
        app.begin_log_viewer();
        let baseline = log_state(&app).snapshot_total;

        // Detach: stepping the cursor up freezes the snapshot (baseline
        // stays put). Steps walk LOGICAL lines one by one.
        app.log_cursor_up(30);
        app.log_cursor_up(30);
        assert_eq!(log_state(&app).cursor, 0, "cursor clamps at the top");
        app.log_cursor_down(10);
        assert_eq!(log_state(&app).cursor, 10);
        assert!(
            !log_state(&app).at_tail(),
            "still pinned away from the tail"
        );

        // Lines arriving while pinned count against the baseline…
        let detached_marker = unique_line("detached");
        crate::diag::append(&detached_marker);
        assert_eq!(
            log_state(&app).snapshot_total,
            baseline,
            "snapshot frozen while pinned"
        );
        assert!(
            !log_state(&app)
                .lines
                .iter()
                .any(|e| e.text == *detached_marker),
            "frozen snapshot does not show the arrival"
        );
        let total_now = crate::diag::total_appended();
        assert!(
            total_now >= baseline,
            "monotonic total includes the detached arrival"
        );

        // …and G (jump to bottom) re-arms the tail: the snapshot refreshes
        // to CURRENT content.
        app.log_jump_bottom();
        let state = log_state(&app);
        assert!(state.at_tail(), "back at the bottom");
        assert!(
            state.lines.iter().any(|e| e.text == *detached_marker),
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
        for i in 0..12 {
            crate::diag::append(format!("filler-{i}"));
        }
        app.begin_log_viewer();
        let seen_at_open = app.log_seen_total;
        app.log_cursor_up(9);
        assert!(!log_state(&app).at_tail(), "precondition: pinned");
        crate::diag::append("missed-while-pinned");
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
        app.log_cursor_up(3);
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

    /// `w` flips the wrap flag; the cursor keeps
    /// pointing at the SAME logical line, and one cursor step still walks
    /// one logical line even when wrap splits it over several rows.
    #[test]
    fn wrap_toggle_keeps_cursor_over_logical_lines() {
        let mut app = app_with_chats(0);
        crate::diag::append("short head");
        crate::diag::append(format!("LONG-{}", "x".repeat(300)));
        crate::diag::append("short tail");
        app.begin_log_viewer();

        // The diag stream is process-global and other tests append to it in
        // parallel, so all indices are relative to the snapshot taken at
        // open: the three lines above are the LAST three snapshot entries.
        let len = log_state(&app).lines.len();
        let tail = len - 1;

        assert!(!log_state(&app).wrap, "wrap starts off");
        assert!(log_state(&app).at_tail(), "cursor parked on the tail");
        app.log_toggle_wrap();
        assert!(log_state(&app).wrap, "`w` turns wrap on");
        assert_eq!(
            log_state(&app).cursor,
            tail,
            "cursor still on the same logical line"
        );

        // One step up = one logical line: from `short tail` straight onto
        // the 300-char line, regardless of its wrapped row count.
        app.log_cursor_up(1);
        assert_eq!(log_state(&app).cursor, tail - 1);
        app.log_cursor_up(1);
        assert_eq!(log_state(&app).cursor, tail - 2);
        app.log_cursor_up(1);
        assert_eq!(
            log_state(&app).cursor,
            tail.saturating_sub(3),
            "clamped only at the very top"
        );

        app.log_toggle_wrap();
        assert!(!log_state(&app).wrap, "second `w` turns wrap back off");
    }

    /// PgUp/PgDn move the cursor by one viewport of
    /// rows (lines while wrap is off); landing back on the newest line
    /// re-arms the live tail.
    #[test]
    fn page_navigation_moves_cursor_by_viewport_rows() {
        let mut app = app_with_chats(0);
        for i in 0..60 {
            crate::diag::append(format!("filler-{i}"));
        }
        app.begin_log_viewer();
        // Default page seam before the first render (same as chat pane).
        assert_eq!(app.log_visible_rows, 20);
        // Indices relative to the open-time snapshot: the diag stream is
        // process-global and other tests append to it in parallel.
        let tail = log_state(&app).lines.len() - 1;
        assert_eq!(log_state(&app).cursor, tail, "opens on the newest line");

        app.log_page_up();
        assert_eq!(log_state(&app).cursor, tail - 20, "PgUp = one page up");
        app.log_page_up();
        assert_eq!(log_state(&app).cursor, tail - 40, "PgUp accumulates");
        app.log_page_down();
        assert_eq!(log_state(&app).cursor, tail - 20, "PgDn = one page down");
        let baseline = log_state(&app).snapshot_total;
        app.log_page_down();
        assert!(
            log_state(&app).at_tail(),
            "PgDn lands back on the tail and re-arms"
        );
        assert!(
            log_state(&app).snapshot_total >= baseline,
            "tail re-armed: snapshot refreshed to current ring content"
        );
    }

    /// the shared wrap chunker must agree with the
    /// renderer row-for-row (character-level, display width, exact fit stays
    /// one row, degenerate width never loops).
    #[test]
    fn wrap_line_chunks_by_display_width() {
        assert_eq!(super::wrap_line("abc", 10), vec!["abc"]);
        assert_eq!(super::wrap_line("abc", 3), vec!["abc"]);
        assert_eq!(super::wrap_line("abcd", 3), vec!["abc", "d"]);
        assert_eq!(super::wrap_line("abcdef", 2), vec!["ab", "cd", "ef"]);
        // Wide (CJK) chars count their display width, not their char count.
        assert_eq!(
            super::wrap_line("\u{4f60}\u{597d}\u{4e16}", 4),
            vec!["\u{4f60}\u{597d}", "\u{4e16}"]
        );
        // Degenerate width: clamped to 1, no division by zero, no hang.
        assert_eq!(super::wrap_line("ab", 0), vec!["a", "b"]);
        assert_eq!(super::wrapped_row_count("", 5), 1);
    }

    //------------------------------------------------------------------------

    /// The REAL captured `/model` listing — the parser's and the picker's
    /// ground truth (see `model_picker.rs` for provenance).
    const CAPTURED_LISTING: &str = include_str!("../tests/fixtures/model_listing_captured.txt");

    /// Open the picker as ^M does and consume the staged hidden fetch the
    /// way the event loop does, so the test controls delivery timing.
    fn open_picker(app: &mut App) {
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
            super::preselect_model_index(
                &parse_model_listing("1. Alpha (A)\n2. beta (B)\n"),
                Some("  BETA ")
            ),
            Some(1)
        );
        assert_eq!(super::preselect_model_index(&[], Some("Alpha")), None);
        assert_eq!(
            super::preselect_model_index(&parse_model_listing("1. Alpha (A)\n"), Some("unknown")),
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

    /// Fresh user activity (request start) lifts the thread to the sidebar
    /// top and the selection follows it; other chats keep their order.
    #[test]
    fn begin_request_lifts_active_thread_to_top() {
        let mut app = app_with_chats(3);
        app.select_chat(2);
        let submitted = submit_text(&mut app, "top please");

        assert_eq!(app.chats[0].id, submitted.thread_id, "thread on top");
        assert_eq!(app.active, 0, "selection follows the lifted thread");
        assert_eq!(app.chats[1].name, "chat-0", "others keep their order");
        assert_eq!(app.chats[2].name, "chat-1", "others keep their order");
    }

    /// A visible reply in a BACKGROUND thread lifts it to the top WITHOUT
    /// stealing the selection, and the lifted thread carries its snapshot
    /// stamp (updated_at) for the next persistence round.
    #[test]
    fn visible_reply_lifts_background_thread_to_top() {
        let mut app = app_with_chats(3);
        submit_text(&mut app, "work in chat 0");
        app.select_chat(idx(&app, "chat-1"));
        let background = submit_text(&mut app, "background request");
        app.select_chat(idx(&app, "chat-2")); // user reads chat-2
        {
            let i = idx(&app, "chat-1");
            finish_chat(&mut app, i);
        }

        assert_eq!(
            app.chats[0].id, background.thread_id,
            "the answered thread is on top"
        );
        assert_eq!(
            app.chats[app.active].id,
            app.chats[idx(&app, "chat-2")].id,
            "selection stays on the thread the user was reading"
        );
    }

    /// A queued prompt (busy chat) is activity too: the thread lifts to the
    /// top at enqueue time.
    #[test]
    fn queued_prompt_lifts_thread_to_top() {
        let mut app = app_with_chats(2);
        submit_text(&mut app, "first in chat 0");
        app.select_chat(idx(&app, "chat-1"));
        submit_text(&mut app, "second in chat 1"); // chat-1 lifts to the top
        app.select_chat(idx(&app, "chat-0")); // user goes back to busy chat-0
        type_in(&mut app, "queued behind the running turn");
        assert!(app.take_input().is_none(), "busy chat enqueues");

        assert_eq!(
            app.chats[0].name, "chat-0",
            "the enqueued thread is back on top"
        );
        assert_eq!(
            app.chats[app.active].name, "chat-0",
            "selection follows the enqueued thread"
        );
        assert_eq!(app.chats[1].name, "chat-1", "the other busy thread below");
    }

    /// A background continuation (`message` frame) lifts its thread to the
    /// top as well.
    #[test]
    fn background_continuation_lifts_thread_to_top() {
        let mut app = app_with_chats(2);
        let target = app.chats[1].id.clone();
        app.apply_backend_event(BackendEvent::BackgroundMessage {
            wire_thread_id: crate::live::wire_thread_id(&target),
            markdown: "continuation".into(),
            model: None,
            thoughts: None,
        });

        assert_eq!(app.chats[0].id, target, "continuation lifts the thread");
    }

    /// touch_chat: the moved chat keeps its selection, chats it jumped over
    /// shift right by one, and the stamp is refreshed.
    #[test]
    fn touch_chat_lifts_thread_and_fixes_indexes() {
        let mut app = app_with_chats(3);
        app.select_chat(2);
        let before = app.chats[2].updated_at;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        app.touch_chat(2);

        assert_eq!(app.chats[0].name, "chat-2", "lifted to the top");
        assert_eq!(app.active, 0, "the lifted chat stays selected");
        assert_eq!(app.chats[1].name, "chat-0", "shifted right");
        assert_eq!(app.chats[2].name, "chat-1", "shifted right");
        assert!(
            app.chats[0].updated_at > before,
            "the activity stamp is refreshed"
        );
    }

    /// touch_chat is a safe no-op for an out-of-range index.
    #[test]
    fn touch_chat_out_of_range_is_a_noop() {
        let mut app = app_with_chats(2);
        let snapshot: Vec<String> = app.chats.iter().map(|c| c.name.clone()).collect();
        app.touch_chat(9);
        let after: Vec<String> = app.chats.iter().map(|c| c.name.clone()).collect();
        assert_eq!(snapshot, after, "nothing moved");
    }

    // ---- mouse text selection --------------------------------------------

    use crate::app::{ChatGeometry, ChatRowMeta, SelectionPoint};

    fn row_meta(logical: usize, start: usize, end: usize, text: &str) -> ChatRowMeta {
        ChatRowMeta {
            logical,
            start,
            end,
            text: text.to_owned(),
        }
    }

    fn geometry_with_rows(chat_id: &str, rows: Vec<ChatRowMeta>) -> ChatGeometry {
        ChatGeometry {
            chat_id: chat_id.to_owned(),
            inner: Rect::new(26, 1, 94, 10),
            skip: 0,
            rows,
        }
    }

    fn point(row: usize, col: usize) -> SelectionPoint {
        SelectionPoint { row, col }
    }

    /// THE plain-text extraction criterion: chars of the wrapped rows
    /// between the ordered endpoints, concatenated ACROSS wrap points of
    /// one logical line and newline-SEPARATED at real line breaks.
    #[test]
    fn selection_text_joins_wraps_and_breaks_at_logical_lines() {
        let mut app = app_with_chats(1);
        // Logical line 0 ("alpha beta gamma delta") wrapped at width 10
        // into three rows; logical line 1 is a separate line.
        app.chat_geometry = Some(geometry_with_rows(
            &app.chats[0].id,
            vec![
                row_meta(0, 0, 10, "alpha beta"),
                row_meta(0, 11, 16, "gamma"),
                row_meta(0, 17, 22, "delta"),
                row_meta(1, 0, 6, "second"),
            ],
        ));

        // Wrap-spanning drag within ONE logical line: rows concatenate
        // without newlines, reverse drag normalized.
        app.selection = Some(crate::app::ChatSelection {
            anchor: point(2, 5),
            head: point(0, 6),
            dragging: false,
        });
        assert_eq!(
            app.selection_text().as_deref(),
            Some("beta gamma delta"),
            "wrap points join without newlines"
        );

        // A drag crossing into the NEXT logical line: the real line break
        // becomes a newline (mid-word cut included); the wrap gap between
        // "gamma" and "delta" restores one space.
        app.selection = Some(crate::app::ChatSelection {
            anchor: point(1, 0),
            head: point(3, 4),
            dragging: false,
        });
        assert_eq!(app.selection_text().as_deref(), Some("gamma delta\nseco"));

        // Same logical line only: no newline even across rows.
        app.selection = Some(crate::app::ChatSelection {
            anchor: point(1, 0),
            head: point(2, 5),
            dragging: false,
        });
        assert_eq!(app.selection_text().as_deref(), Some("gamma delta"));
    }

    /// The extraction is guarded against a geometry from a foreign thread
    /// (the one-iteration staleness window after a thread switch).
    #[test]
    fn selection_text_rejects_a_foreign_chats_geometry() {
        let mut app = app_with_chats(1);
        app.chat_geometry = Some(geometry_with_rows(
            "some-other-thread",
            vec![row_meta(0, 0, 5, "hello")],
        ));
        app.selection = Some(crate::app::ChatSelection {
            anchor: point(0, 0),
            head: point(0, 5),
            dragging: false,
        });
        assert_eq!(app.selection_text(), None, "stale geometry is rejected");
    }

    /// Lifecycle: press starts a live drag, drag moves the head only while
    /// live, release with a real selection keeps it and extracts the text,
    /// a plain click (release without drag) clears instead.
    #[test]
    fn selection_lifecycle_press_drag_release_and_plain_click() {
        let mut app = app_with_chats(1);
        app.chat_geometry = Some(geometry_with_rows(
            &app.chats[0].id,
            vec![row_meta(0, 0, 11, "hello world")],
        ));

        app.begin_selection(point(0, 0));
        let sel = app.selection.expect("press starts the selection");
        assert!(sel.dragging);

        app.drag_selection(point(0, 5));
        app.drag_selection(point(0, 11));
        assert_eq!(
            app.selection.expect("still live").head,
            point(0, 11),
            "drag moves the head"
        );

        let text = app.release_selection().expect("real selection copies");
        assert_eq!(text, "hello world");
        let sel = app.selection.expect("held after release");
        assert!(!sel.dragging, "released");

        // Drag AFTER the release must not resurrect/move the selection.
        app.drag_selection(point(0, 2));
        assert_eq!(app.selection.expect("held").head, point(0, 11));

        // Plain click: press + release at the same point clears, no copy.
        app.begin_selection(point(0, 3));
        assert_eq!(app.release_selection(), None, "plain click copies nothing");
        assert!(app.selection.is_none(), "plain click clears");
    }

    /// A selection is cleared by a thread switch (the single
    /// [`App::select_chat`] seam behind every switching path) and by the
    /// explicit clear (Esc), and it never lands in the persisted snapshot:
    /// it lives on [`App`], not on [`Chat`].
    #[test]
    fn selection_clears_on_thread_switch_and_is_never_chat_state() {
        let mut app = app_with_chats(2);
        app.begin_selection(point(0, 0));
        app.drag_selection(point(0, 4));
        assert!(app.selection.is_some());

        app.clear_selection();
        assert!(app.selection.is_none(), "explicit clear (Esc)");

        app.begin_selection(point(0, 0));
        app.select_next();
        assert!(
            app.selection.is_none(),
            "thread switch clears the selection"
        );
        // The chat struct carries no selection: serialization of a message
        // snapshot (the persisted unit) is unaffected by the feature.
        let mut chat = Chat::new("clean");
        chat.messages.push(crate::model::Message::user("hello"));
        let json = serde_json::to_string(&chat.messages).unwrap();
        assert!(
            !json.contains("selection"),
            "nothing selection-shaped persisted"
        );
    }

    /// [`ChatGeometry::position_at`]: terminal rows map to document rows
    /// through the scroll offset (skip), columns to char offsets through
    /// display width, the right edge clamps at the row end, and points
    /// outside the inner rect are rejected.
    #[test]
    fn chat_geometry_position_at_maps_rows_columns_and_clamps() {
        let rows: Vec<ChatRowMeta> = (0..5)
            .map(|i| row_meta(i, 0, 4, &format!("r{i}")))
            .chain([
                row_meta(5, 0, 11, "hello world"),
                row_meta(5, 12, 15, "abc"),
            ])
            .collect();
        let geom = ChatGeometry {
            chat_id: "t".into(),
            inner: Rect::new(26, 1, 10, 2),
            skip: 5,
            rows,
        };
        // First visible row = document row 5 (skip); col 0 at the pane edge.
        assert_eq!(geom.position_at(26, 1), Some(point(5, 0)));
        // 6 columns in → 6 chars ("hello ") fully left of the pointer.
        assert_eq!(geom.position_at(32, 1), Some(point(5, 6)));
        // Second visible row = document row 6; past its end clamps at 3.
        assert_eq!(geom.position_at(35, 2), Some(point(6, 3)));
        // Below the pane: rejected (the router clamps drags separately).
        assert_eq!(geom.position_at(30, 3), None);
        // Outside the pane horizontally (divider column) too.
        assert_eq!(geom.position_at(25, 1), None);
        // No rows at all: nothing to hit.
        let empty = ChatGeometry {
            chat_id: "t".into(),
            inner: Rect::new(26, 1, 10, 2),
            skip: 0,
            rows: Vec::new(),
        };
        assert_eq!(empty.position_at(26, 1), None);
    }
}
