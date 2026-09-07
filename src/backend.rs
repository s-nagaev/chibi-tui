//! Backend abstraction. The UI never talks to a process/network — it only
//! feeds prompts to a `Backend` and consumes `BackendEvent`s from a channel.
//! Swapping mocks for the real JSONL client later means providing another
//! implementation; UI code stays untouched.

use tokio::sync::mpsc;

use crate::protocol::{AgentEventKind, Usage};

/// Events pushed from the backend towards the UI, mirroring the coarse
/// lifecycle of protocol v1 (`status queued/running`, then `result`/`error`).
///
/// Request-scoped variants carry `thread_id` so the app can route them to the
/// right chat even while several chats run concurrently (per-thread async).
/// The mock backend fills it with a placeholder — its events only ever target
/// the active chat.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum BackendEvent {
    /// Request accepted, will start soon.
    Queued { request_id: u64, thread_id: String },
    /// Backend is processing the request.
    Running { request_id: u64, thread_id: String },
    /// Mid-turn subagent progress for a request (real backend only, emitted
    /// for clients that declared `capabilities.subagents`). NON-terminal:
    /// the app folds it into the live subagent counter and must never
    /// resolve the request lifecycle from it. `request_id` follows the same
    /// numeric correlation convention as every other variant.
    AgentProgress {
        request_id: u64,
        thread_id: String,
        event: AgentEventKind,
        active: u64,
        total: u64,
    },
    /// Final markdown answer.
    ///
    /// `model` is the display label of the model that produced the answer
    /// (feat_agent_model_label) — already resolved by the source (prefer
    /// `model`, fall back to `provider`); `None` renders the plain role
    /// header (fieldless frame, old backend, mock fallback variant).
    Result {
        request_id: u64,
        markdown: String,
        thread_id: String,
        model: Option<String>,
        /// Wave-2 latest-turn token accounting; retained in App state.
        usage: Option<Usage>,
        /// Wave-2 latest-turn LLM reasoning; retained in App state.
        thoughts: Option<String>,
    },
    /// Request failed.
    Error {
        request_id: u64,
        message: String,
        thread_id: Option<String>,
    },
    /// Out-of-band continuation answer from a `message` frame (opt-in via
    /// `capabilities.background_messages`): the backend produced an answer
    /// AFTER a background tool result arrived, when the parent request's
    /// lifecycle is already over. Carries the WIRE thread id (i64 hash) —
    /// the consumer maps it to the owning chat; no request id exists.
    /// NON-terminal by construction: it must never resolve any request
    /// lifecycle, spinner or queue state.
    BackgroundMessage {
        wire_thread_id: i64,
        markdown: String,
        model: Option<String>,
        thoughts: Option<String>,
    },
    /// The event source itself reported the transport link down (no
    /// particular request to blame). Flips the connection indicator; never
    /// opens the popup by itself.
    Disconnected,
    /// Per-thread async: the request of `thread_id` reached its terminal
    /// state, so any prompts waiting in that chat's FIFO queue may now be
    /// sent. Internal glue signal — carries no user-visible content.
    QueueDrain { thread_id: String },
}

/// Source of assistant answers. Implementations MUST NOT spawn processes,
/// open sockets or touch the filesystem in this demo — mocks are static data.
pub trait Backend: Send + 'static {
    /// Submit a user prompt; progress events are delivered on `tx`,
    /// correlated by `request_id`, terminated by `Result` or `Error`.
    fn submit(&mut self, prompt: String, tx: mpsc::Sender<BackendEvent>);
}

/// Mock backend: static canned replies with simulated latency and the same
/// event shape a real JSONL client would emit.
pub struct MockBackend {
    reply_counter: usize,
    next_request_id: u64,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            reply_counter: 0,
            next_request_id: 1,
        }
    }
}

impl Backend for MockBackend {
    fn submit(&mut self, _prompt: String, tx: mpsc::Sender<BackendEvent>) {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        self.reply_counter = (self.reply_counter + 1) % crate::mock::MOCK_REPLIES.len();
        let markdown = crate::mock::MOCK_REPLIES[self.reply_counter].to_string();

        // feat_agent_model_label: mock results alternate between a labelled
        // reply (even index) and a fieldless one (odd index) so both the
        // `● Chibi (model)` and the plain fallback rendering are exercised
        // end-to-end in demo/mock mode.
        let model = if self.reply_counter.is_multiple_of(2) {
            Some("chibi-mock".to_owned())
        } else {
            None
        };

        tokio::spawn(async move {
            let _ = tx
                .send(BackendEvent::Queued {
                    request_id,
                    thread_id: String::new(),
                })
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = tx
                .send(BackendEvent::Running {
                    request_id,
                    thread_id: String::new(),
                })
                .await;
            // ≈1.5–2.5s total perceived latency before the answer lands.
            tokio::time::sleep(std::time::Duration::from_millis(1200 + jitter_ms())).await;
            let _ = tx
                .send(BackendEvent::Result {
                    request_id,
                    markdown,
                    thread_id: String::new(),
                    model,
                    usage: None,
                    thoughts: None,
                })
                .await;
        });
    }
}

/// Cheap latency jitter in `[0, 900)` ms (no external crates).
fn jitter_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % 900)
        .unwrap_or(300)
}
