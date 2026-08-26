//! Application state: chats, selection, input buffer, request lifecycle.

use std::collections::VecDeque;

use crate::backend::BackendEvent;
use crate::model::{ChatLifecycle, Message};
use crate::popup::ErrorPopup;
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

/// Input mode of the whole app (feature: inline thread rename).
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
}

impl Mode {
    /// True unless a rename session is open.
    pub fn is_normal(&self) -> bool {
        matches!(self, Mode::Normal)
    }
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
    /// Cancel target `(request_id, thread_id)` produced by Ctrl+C; the event
    /// loop sends the actual frame. Targets ONLY the active chat's in-flight
    /// request — queued prompts survive a cancel.
    pub pending_cancel: Option<(String, String)>,
    /// Global input mode (feature: inline thread rename).
    pub mode: Mode,
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
            pending_cancel: None,
            mode: Mode::Normal,
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

    /// Create a chat with a fresh UUID thread_id, select it.
    pub fn new_chat(&mut self) {
        let n = self.chats.len() + 1;
        self.chats.push(Chat::new(format!("New chat {n}")));
        self.active = self.chats.len() - 1;
        self.scroll = 0;
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
            Mode::Normal => None,
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
            Mode::Normal => self.input.lines().len(),
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
            Mode::Normal => return false,
        };
        self.mode = Mode::Normal;
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

    /// Local fallback for Ctrl+C when there is no live backend to receive a
    /// cancel frame (mock mode has no cancel protocol; placeholder mode has
    /// no backend at all). Resolves the active chat's pending placeholder
    /// immediately and returns THAT chat to Idle so its spinner can never get
    /// stuck. The chat's queue survives; mock mode simply has no auto-send
    /// pump (same trade-off as before this feature).
    pub fn resolve_cancel_locally(&mut self) {
        if let Some(chat) = self.chats.get_mut(self.active) {
            resolve_live_placeholder(chat, "_Cancelled._".to_owned());
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
                ..
            } => {
                if event_matches_request(request_id, &tracked_request_id) {
                    let chat = &mut self.chats[chat_index];
                    resolve_live_placeholder(chat, markdown);
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
                    resolve_live_placeholder(chat, format!("**Error:** {message}"));
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

/// Resolve the LIVE pending placeholder of a chat: the newest pending row
/// that is not a queued marker (those belong to prompts still waiting in the
/// FIFO queue and are owned by [`App::dequeue_next_for`]). Terminal outcomes
/// (`result`, `error`, cancel) must never touch queued markers.
fn resolve_live_placeholder(chat: &mut Chat, markdown: String) {
    if let Some(row) = chat
        .messages
        .iter_mut()
        .rev()
        .find(|m| m.pending && !is_queued_marker(m))
    {
        row.pending = false;
        row.markdown = markdown;
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
        let request_id = app.chats[index].lifecycle.request_id().unwrap().to_owned();
        let thread_id = app.chats[index].id.clone();
        app.apply_backend_event(BackendEvent::Result {
            request_id: event_id_of(&request_id),
            markdown: "**done**".into(),
            thread_id,
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
}
