//! Backend abstraction. The UI never talks to a process/network — it only
//! feeds prompts to a `Backend` and consumes `BackendEvent`s from a channel.
//! Swapping mocks for the real JSONL client later means providing another
//! implementation; UI code stays untouched.

use tokio::sync::mpsc;

/// Events pushed from the backend towards the UI, mirroring the coarse
/// lifecycle of protocol v1 (`status queued/running`, then `result`/`error`).
/// `request_id` is kept in the event shape now so the future real client
/// needs no UI-side changes; the mock UI ignores it.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum BackendEvent {
    /// Request accepted, will start soon.
    Queued { request_id: u64 },
    /// Backend is processing the request.
    Running { request_id: u64 },
    /// Final markdown answer.
    Result { request_id: u64, markdown: String },
    /// Request failed.
    Error { request_id: u64, message: String },
    /// The event source itself reported the transport link down (no
    /// particular request to blame). Flips the connection indicator; never
    /// opens the popup by itself.
    Disconnected,
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

        tokio::spawn(async move {
            let _ = tx.send(BackendEvent::Queued { request_id }).await;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = tx.send(BackendEvent::Running { request_id }).await;
            // ≈1.5–2.5s total perceived latency before the answer lands.
            tokio::time::sleep(std::time::Duration::from_millis(1200 + jitter_ms())).await;
            let _ = tx
                .send(BackendEvent::Result {
                    request_id,
                    markdown,
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
