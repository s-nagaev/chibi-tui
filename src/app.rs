//! Application state: chats, selection, input buffer, request lifecycle.

use crate::backend::BackendEvent;
use crate::model::{ChatStatus, Message};
use crate::popup::ErrorPopup;
use tui_textarea::TextArea;

/// Liveness of the backend link as shown by the status-bar indicator.
///
/// This tracks the *transport* only. Per-request progress (queued/running)
/// lives in [`ChatStatus`] and the spinner line — the two never duplicate
/// each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    /// Backend process is up and the handshake succeeded.
    Connected,
    /// (Re)connect attempt in progress.
    Connecting,
    /// No usable backend; recoverable via `R` from the error popup.
    Disconnected,
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
pub struct Chat {
    pub name: String,
    pub id: String,
    pub messages: Vec<Message>,
}

impl Chat {
    /// Fresh chat with a generated stable thread id.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            id: crate::history::new_thread_id(),
            messages: Vec::new(),
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

/// Whole UI state.
pub struct App {
    pub chats: Vec<Chat>,
    pub active: usize,
    pub status: ChatStatus,
    pub scroll: u16,
    pub input: TextArea<'static>,
    pub spinner_frame: usize,
    pub should_quit: bool,
    /// Protocol request id of the in-flight request (if any), kept for
    /// targeted cancel.
    pub active_request_id: Option<String>,
    /// Modal error popup (backend failures). When set, it captures input
    /// until dismissed.
    pub error_popup: Option<ErrorPopup>,
    /// Transport liveness for the status-bar indicator.
    pub connection: Connection,
    /// Set by `R` in the error popup; the event loop performs the async
    /// reconnect and clears it.
    pub reconnect_requested: Option<ReconnectRequest>,
    /// Cancel target `(request_id, thread_id)` produced by Ctrl+C; the event
    /// loop sends the actual frame.
    pub pending_cancel: Option<(String, String)>,
}

const SPINNER: [&str; 10] = [
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];

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
            status: ChatStatus::Idle,
            scroll: 0,
            input,
            spinner_frame: 0,
            should_quit: false,
            active_request_id: None,
            error_popup: None,
            connection: Connection::Connecting,
            reconnect_requested: None,
            pending_cancel: None,
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

    /// Extract the current input buffer as a [`Submitted`] prompt bundle.
    ///
    /// Returns None when the buffer is empty/whitespace or a request is
    /// already in flight (one at a time). Consumes (clears) the buffer on
    /// success; state mutation happens in [`App::begin_request`].
    pub fn take_input(&mut self) -> Option<Submitted> {
        if self.status != ChatStatus::Idle {
            return None;
        }
        let text = self.input.lines().join("\n");
        let text = text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        // Reset the TextArea in place, keeping its placeholder.
        self.input = TextArea::default();
        self.input
            .set_placeholder_text("Type a message…  (\u{23ce} send)");
        Some(Submitted {
            request_id: crate::history::new_request_id(),
            thread_id: self
                .chats
                .get(self.active)
                .map(|c| c.id.clone())
                .unwrap_or_default(),
            prompt: text,
        })
    }

    /// Register the in-flight request produced by [`App::take_input`]:
    /// appends the outgoing user message plus the pending assistant
    /// placeholder, stores the request id for cancel, moves to Queued.
    pub fn begin_request(&mut self, submitted: &Submitted) {
        if let Some(chat) = self.chats.get_mut(self.active) {
            chat.messages.push(Message::user(submitted.prompt.clone()));
            chat.messages.push(Message::assistant_pending());
        }
        self.status = ChatStatus::Queued;
        self.active_request_id = Some(submitted.request_id.clone());
        self.scroll = 0; // stick to bottom
    }

    /// Target [`Submitted`]-compatible pair `(request_id, thread_id)` for
    /// cancelling the in-flight request of the active chat, if any.
    ///
    /// The UI state itself is left untouched here — the resulting
    /// cancelled-error event from the backend resolves the pending
    /// placeholder and returns the app to Idle.
    #[allow(clippy::option_option)]
    pub fn cancel_active(&self) -> Option<(String, String)> {
        match (&self.active_request_id, self.active_thread_id()) {
            (Some(request_id), Some(thread_id)) => Some((request_id.clone(), thread_id.to_owned())),
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

    /// True while a request is in flight (`Queued` or `Running`).
    pub fn is_busy(&self) -> bool {
        self.status != ChatStatus::Idle
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
    /// no backend at all). Resolves the pending placeholder immediately and
    /// returns the app to Idle so the spinner can never get stuck.
    pub fn resolve_cancel_locally(&mut self) {
        if let Some(chat) = self.chats.get_mut(self.active) {
            if let Some(last) = chat.messages.last_mut() {
                last.pending = false;
                last.markdown = "_Cancelled._".to_owned();
            }
        }
        self.status = ChatStatus::Idle;
        self.active_request_id = None;
    }

    /// Apply an event from the backend source.
    ///
    /// Request-scoped events are only applied while a request is actually
    /// tracked (`active_request_id` set); stray events for already-finished
    /// requests are ignored so they cannot corrupt state or resurrect the
    /// spinner. The connection-level [`BackendEvent::Disconnected`] is
    /// applied unconditionally — it carries no request.
    pub fn apply_backend_event(&mut self, event: BackendEvent) {
        if let BackendEvent::Disconnected = event {
            // The event source itself reported the link down.
            self.connection = Connection::Disconnected;
            return;
        }
        if self.active_request_id.is_none() {
            return;
        }
        match event {
            BackendEvent::Queued { .. } => {
                self.status = ChatStatus::Queued;
            }
            BackendEvent::Running { .. } => {
                self.status = ChatStatus::Running;
            }
            BackendEvent::Result { markdown, .. } => {
                if let Some(chat) = self.chats.get_mut(self.active) {
                    if let Some(last) = chat.messages.last_mut() {
                        last.pending = false;
                        last.markdown = markdown;
                    }
                }
                self.status = ChatStatus::Idle;
                self.active_request_id = None;
            }
            BackendEvent::Error { message, .. } => {
                if let Some(chat) = self.chats.get_mut(self.active) {
                    if let Some(last) = chat.messages.last_mut() {
                        last.pending = false;
                        last.markdown = format!("**Error:** {message}");
                    }
                }
                self.status = ChatStatus::Idle;
                self.active_request_id = None;

                // Transport-level failures (broken pipe, lost backend, failed
                // cancel of a dead link) escalate to the modal popup; the
                // connection indicator flips to disconnected. Per-request
                // errors (bad prompt, refused request) stay inline.
                if is_transport_failure(&message) {
                    self.connection = Connection::Disconnected;
                    self.show_error(message);
                }
            }
            // Handled by the early return above; arm kept for exhaustiveness.
            BackendEvent::Disconnected => {}
        }
        self.scroll = 0;
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn submit_text(app: &mut App, text: &str) -> Submitted {
        type_in(app, text);
        let submitted = app.take_input().expect("prompt taken");
        app.begin_request(&submitted);
        submitted
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

    // ---- lifecycle -------------------------------------------------------

    #[test]
    fn lifecycle_idle_queued_running_result_idle() {
        let mut app = App::new(vec![Chat::new("lifecycle")]);
        assert_eq!(app.status, ChatStatus::Idle);

        let submitted = submit_text(&mut app, "explain lifetimes");
        assert_eq!(submitted.prompt, "explain lifetimes");
        assert!(!submitted.thread_id.is_empty());
        assert!(!submitted.request_id.is_empty());
        assert_eq!(app.status, ChatStatus::Queued);
        assert_eq!(
            app.active_request_id.as_deref(),
            Some(submitted.request_id.as_str())
        );
        assert_eq!(app.chats[0].messages.len(), 2, "user + pending assistant");

        app.apply_backend_event(BackendEvent::Running { request_id: 1 });
        assert_eq!(app.status, ChatStatus::Running);

        app.apply_backend_event(BackendEvent::Result {
            request_id: 1,
            markdown: "**done**".into(),
        });
        assert_eq!(app.status, ChatStatus::Idle);
        assert_eq!(app.active_request_id, None);
        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 2);
        assert!(!msgs[1].pending);
        assert_eq!(msgs[1].markdown, "**done**");
    }

    #[test]
    fn lifecycle_error_returns_to_idle_with_error_message() {
        let mut app = App::new(vec![Chat::new("errors")]);
        submit_text(&mut app, "boom");
        app.apply_backend_event(BackendEvent::Error {
            request_id: 9,
            message: "backend exploded".into(),
        });
        assert_eq!(app.status, ChatStatus::Idle);
        assert_eq!(app.active_request_id, None);
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
        assert_eq!(app.status, ChatStatus::Idle);
        assert!(app.active_request_id.is_none());
        assert!(app.chats[0].messages.is_empty());
    }

    #[test]
    fn submit_while_busy_is_rejected_and_buffer_preserved() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "first");
        // Second Enter mid-flight must neither consume the buffer nor queue up.
        type_in(&mut app, "second");
        assert!(app.take_input().is_none(), "no second submit while busy");
        assert_eq!(app.input.lines().join(""), "second");
        assert_eq!(app.status, ChatStatus::Queued);
    }

    #[test]
    fn request_ids_differ_between_submissions() {
        let mut app = app_with_chats(1);
        let first = submit_text(&mut app, "one");
        app.apply_backend_event(BackendEvent::Result {
            request_id: 0,
            markdown: "ok".into(),
        });
        let second = submit_text(&mut app, "two");
        assert_ne!(first.request_id, second.request_id);
    }

    #[test]
    fn cancel_active_reports_inflight_request_only() {
        let mut app = app_with_chats(1);
        assert!(app.cancel_active().is_none(), "nothing in flight yet");

        let submitted = submit_text(&mut app, "to cancel");
        let (request_id, thread_id) = app.cancel_active().expect("cancel target present");
        assert_eq!(request_id, submitted.request_id);
        assert_eq!(thread_id, app.chats[0].id);
        // UI stays busy: only the backend's cancelled-error resolves it.
        assert_eq!(app.status, ChatStatus::Queued);
        assert!(app.active_request_id.is_some());
    }

    #[test]
    fn stray_events_after_completion_are_ignored() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "one shot");
        app.apply_backend_event(BackendEvent::Result {
            request_id: 0,
            markdown: "answer".into(),
        });
        // Late duplicate / stale event for an already-finished request:
        app.apply_backend_event(BackendEvent::Error {
            request_id: 0,
            message: "late failure".into(),
        });
        assert_eq!(app.status, ChatStatus::Idle);
        assert_eq!(app.active_request_id, None);
        let msgs = &app.chats[0].messages;
        assert_eq!(msgs[1].markdown, "answer", "result not overwritten");
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
    fn is_busy_tracks_lifecycle() {
        let mut app = app_with_chats(1);
        assert!(!app.is_busy());
        submit_text(&mut app, "work");
        assert!(app.is_busy());
        app.apply_backend_event(BackendEvent::Result {
            request_id: 0,
            markdown: "done".into(),
        });
        assert!(!app.is_busy());
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
        assert_eq!(app.status, ChatStatus::Idle);
        assert_eq!(app.active_request_id, None);
        assert!(app.cancel_active().is_none());
    }

    // ---- ux_polish: transport-failure escalation --------------------------

    #[test]
    fn transport_failures_open_error_popup_and_flip_connection() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "go");

        app.apply_backend_event(BackendEvent::Error {
            request_id: 1,
            message: "backend connection lost".into(),
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
        let mut app = app_with_chats(1);
        app.connection = Connection::Connected;
        submit_text(&mut app, "go");

        app.apply_backend_event(BackendEvent::Error {
            request_id: 1,
            message: "Request failed (InvalidArgument): prompt too long".into(),
        });
        assert!(
            app.error_popup.is_none(),
            "per-request errors must not open the popup"
        );
        assert_eq!(app.connection, Connection::Connected);
        assert_eq!(app.status, ChatStatus::Idle);
    }

    #[test]
    fn disconnected_event_flips_indicator_only() {
        let mut app = app_with_chats(1);
        app.connection = Connection::Connected;
        app.apply_backend_event(BackendEvent::Disconnected);
        assert_eq!(app.connection, Connection::Disconnected);
        assert!(app.error_popup.is_none(), "no popup without a request");
        assert_eq!(app.status, ChatStatus::Idle);
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
}
