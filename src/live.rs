//! `LiveBackend` — real [`Backend`] implementation wrapping the protocol v1
//! [`RequestPipeline`] (tasks 3a/3b).
//!
//! Responsibilities:
//! * build a protocol `Request` per submitted prompt — ids come from
//!   [`crate::app::Submitted`] (client-chosen UUIDs), `workspace_root` from
//!   the pipeline's own configuration;
//! * translate the two progress sources into the single [`BackendEvent`]
//!   stream the mock-era UI consumes:
//!     - `status` frames (broadcast channel) → `Queued` / `Running`,
//!     - the per-request final outcome → `Result` / `Error`,
//! * targeted `cancel` delegation.
//!
//! The wire form of `thread_id` is a deterministic i64 hash of the chat's
//! stable UUID ([`wire_thread_id`]): protocol v1 defines `thread_id: integer`
//! while the TUI needs stable, persistable identity — hashing bridges both
//! without touching the frozen protocol types.
//!
//! One spawned glue task per submitted request; it terminates right after the
//! terminal event is forwarded (the status pump is aborted, never leaked).

use std::hash::{Hash, Hasher};
use std::path::Path;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::app::Submitted;
use crate::backend::{Backend, BackendEvent};
use crate::backend_client::BackendError;
use crate::protocol::{ServerMessage, StatusState};
use crate::request_pipeline::{PipelineResult, RequestArgs, RequestPipeline};

/// Real backend: one child process (`chibi ide --stdio`-compatible JSONL
/// peer) plus per-request glue tasks.
///
/// `Clone` gives the event loop cheap owned handles for spawned tasks
/// (targeted cancels) without borrow gymnastics — every clone talks to the
/// same pipeline actor.
#[derive(Clone)]
pub struct LiveBackend {
    pipeline: RequestPipeline,
}

impl LiveBackend {
    /// Spawn the backend process, run the mandatory handshake.
    ///
    /// Same seam as [`RequestPipeline::connect`]: the peer script defaults to
    /// `tests/fake_backend.py` and can be overridden with
    /// `CHIBI_FAKE_BACKEND`.
    pub async fn connect(workspace_root: impl AsRef<Path>) -> Result<Self, BackendError> {
        let pipeline = RequestPipeline::connect(workspace_root.as_ref(), 64).await?;
        Ok(Self { pipeline })
    }

    /// Submit a fully-formed submission bundle (ids from [`crate::App::take_input`],
    /// prompt text verbatim). This is the real entry point the app loop uses.
    pub fn submit_encoded(&self, submitted: Submitted, tx: mpsc::Sender<BackendEvent>) {
        let pipeline = self.pipeline.clone();
        tokio::spawn(forward_one_request(pipeline, submitted, tx));
    }

    /// Cancel one in-flight request by protocol id.
    pub async fn cancel(&self, request_id: &str) -> Result<(), BackendError> {
        self.pipeline.cancel(request_id).await
    }

    /// Graceful shutdown of the backend process and its actor task.
    pub async fn shutdown(&self) -> Result<(), BackendError> {
        self.pipeline.shutdown().await
    }
}

impl Backend for LiveBackend {
    fn submit(&mut self, _prompt: String, tx: mpsc::Sender<BackendEvent>) {
        // The mock-era trait carries only (prompt, tx). Live mode never goes
        // through this generic path (it uses [`LiveBackend::submit_encoded`]
        // with full id bundles); a plain-prompt call is rejected loudly but
        // without hanging the UI.
        let _ = tx.try_send(BackendEvent::Error {
            request_id: 0,
            message: "internal error: live backend requires submit_encoded with ids".to_owned(),
        });
    }
}

// ---------------------------------------------------------------------------
// Wire thread id
// ---------------------------------------------------------------------------

/// Deterministic i64 form of a chat's stable UUID for the protocol's
/// `thread_id: integer` field. Stable across restarts (no random salt).
pub fn wire_thread_id(chat_uuid: &str) -> i64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    chat_uuid.hash(&mut hasher);
    hasher.finish() as i64
}

// ---------------------------------------------------------------------------
// Per-request glue task
// ---------------------------------------------------------------------------

/// Glue task for one submitted prompt: send, pump statuses, await the final
/// outcome, deliver exactly one terminal event, stop.
async fn forward_one_request(
    pipeline: RequestPipeline,
    submitted: Submitted,
    tx: mpsc::Sender<BackendEvent>,
) {
    // Subscribe BEFORE sending: a fast peer may emit `status queued` before
    // send_request even returns. Broadcast only carries post-subscription
    // updates, so ordering here removes the race entirely. Missing statuses
    // are cosmetically harmless anyway (the UI enters Queued eagerly).
    let status_rx = pipeline.subscribe_status();

    // Debug counter for BackendEvent ids (UI ignores them; kept monotonic).
    let event_id = submitted_event_id(&submitted);

    let args = RequestArgs::new(
        submitted.request_id.as_str(),
        wire_thread_id(&submitted.thread_id),
        submitted.prompt.as_str(),
    );
    let result_rx = match pipeline.send_request(args).await {
        Ok(rx) => rx,
        Err(e) => {
            let _ = tx
                .send(BackendEvent::Error {
                    request_id: event_id,
                    message: e.to_string(),
                })
                .await;
            return;
        }
    };

    let pump = tokio::spawn(pump_statuses(
        status_rx,
        submitted.request_id.clone(),
        event_id,
        tx.clone(),
    ));

    let event = terminal_event(result_rx.await, event_id);
    pump.abort();
    let _ = tx.send(event).await;
}

/// Monotonic-ish numeric stand-in for `BackendEvent::request_id: u64` (the UI
/// ignores it; derived from the UUID so concurrent requests stay distinct).
fn submitted_event_id(submitted: &Submitted) -> u64 {
    wire_thread_id(&submitted.request_id) as u64
}

/// Translate the per-request final outcome into the terminal UI event.
fn terminal_event(
    outcome: Result<PipelineResult, oneshot::error::RecvError>,
    event_id: u64,
) -> BackendEvent {
    match outcome {
        Ok(Ok(ServerMessage::Result { content, .. })) => BackendEvent::Result {
            request_id: event_id,
            markdown: content,
        },
        Ok(Ok(unexpected)) => BackendEvent::Error {
            request_id: event_id,
            message: format!("unexpected final frame from backend: {unexpected:?}"),
        },
        Ok(Err(e)) => BackendEvent::Error {
            request_id: event_id,
            message: render_pipeline_error(&e),
        },
        Err(_) => BackendEvent::Error {
            request_id: event_id,
            message: "backend connection lost".to_owned(),
        },
    }
}

/// Human-friendly text for pipeline failures (special-case cancellations).
fn render_pipeline_error(e: &BackendError) -> String {
    match e {
        BackendError::RequestFailed { code, message, .. } => match code {
            crate::protocol::ErrorCode::Cancelled => "Cancelled".to_owned(),
            _ => format!("Request failed ({code:?}): {message}"),
        },
        other => other.to_string(),
    }
}

/// Forward `status` frames for `request_id` as UI events until aborted.
async fn pump_statuses(
    mut status_rx: broadcast::Receiver<crate::request_pipeline::StatusUpdate>,
    request_id: String,
    event_id: u64,
    tx: mpsc::Sender<BackendEvent>,
) {
    loop {
        match status_rx.recv().await {
            Ok(update) => {
                if update.request_id.0 != request_id {
                    continue; // some other request's progress
                }
                let event = match update.state {
                    StatusState::Queued => BackendEvent::Queued {
                        request_id: event_id,
                    },
                    StatusState::Running => BackendEvent::Running {
                        request_id: event_id,
                    },
                };
                if tx.send(event).await.is_err() {
                    return; // UI receiver dropped
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue, // best effort
            Err(broadcast::error::RecvError::Closed) => return,      // pipeline gone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn sample_submitted() -> Submitted {
        Submitted {
            request_id: "11111111-2222-3333-4444-555555555555".to_owned(),
            thread_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            prompt: "Explain what this function does.".to_owned(),
        }
    }

    #[test]
    fn wire_thread_id_is_stable_and_distinct() {
        let a = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let b = "bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee";
        assert_eq!(wire_thread_id(a), wire_thread_id(a), "stable across calls");
        assert_ne!(wire_thread_id(a), wire_thread_id(b));
    }

    /// Terminal-event mapping: a normal result frame becomes
    /// `BackendEvent::Result` carrying the content verbatim.
    #[test]
    fn terminal_event_maps_result_content_verbatim() {
        let outcome: Result<PipelineResult, oneshot::error::RecvError> =
            Ok(Ok(ServerMessage::Result {
                request_id: "x".into(),
                content: "**42**".into(),
                model: None,
                provider: None,
            }));
        match terminal_event(outcome, 7) {
            BackendEvent::Result {
                request_id,
                markdown,
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(markdown, "**42**");
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    /// Terminal-event mapping: a cancelled request surfaces as a clean
    /// "Cancelled" error, not raw debug noise.
    #[test]
    fn terminal_event_maps_cancelled_to_clean_error() {
        let outcome: Result<PipelineResult, oneshot::error::RecvError> =
            Ok(Err(BackendError::RequestFailed {
                request_id: Some("tui-1".into()),
                code: crate::protocol::ErrorCode::Cancelled,
                message: "Request cancelled.".into(),
            }));
        match terminal_event(outcome, 1) {
            BackendEvent::Error { message, .. } => assert_eq!(message, "Cancelled"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Terminal-event mapping: a broken pipe (peer died) becomes an Error
    /// event instead of hanging the UI.
    #[test]
    fn terminal_event_maps_broken_pipe_to_error() {
        let outcome: Result<PipelineResult, oneshot::error::RecvError> =
            Ok(Err(BackendError::Broken("child died".into())));
        match terminal_event(outcome, 2) {
            BackendEvent::Error { message, .. } => {
                assert!(message.contains("child died"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Full round trip against the deterministic fake peer:
    /// submit → Queued → Running → Result reaches the UI channel.
    ///
    /// Uses `LiveBackend::submit_encoded`, the real entry point main.rs uses
    /// after `App::take_input`.
    #[tokio::test]
    async fn submit_delivers_status_and_result_events_via_fake_peer() {
        let live = tokio::time::timeout(TIMEOUT, LiveBackend::connect("."))
            .await
            .expect("connect within timeout")
            .expect("handshake ok");

        let (tx, mut rx) = mpsc::channel(16);

        // Drain events concurrently; assert on what arrives within budget.
        let drainer = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..3 {
                match tokio::time::timeout(TIMEOUT, rx.recv()).await {
                    Ok(Some(evt)) => seen.push(evt),
                    _ => break,
                }
            }
            seen
        });

        live.submit_encoded(sample_submitted(), tx);

        let events = drainer.await.expect("drainer joins");
        assert!(
            matches!(events[0], BackendEvent::Queued { .. }),
            "first event must be Queued, got {:?}",
            events[0]
        );
        assert!(
            matches!(events[1], BackendEvent::Running { .. }),
            "second event must be Running, got {:?}",
            events[1]
        );
        match &events[2] {
            BackendEvent::Result { markdown, .. } => {
                assert_eq!(
                    markdown, "This function `foo` returns the integer `42`.",
                    "result content passes through verbatim"
                );
            }
            other => panic!("third event must be Result, got {other:?}"),
        }

        let _ = live.shutdown().await;
    }

    /// Two sequential requests against the fake peer both complete with
    /// distinct protocol request ids (correlation sanity at the glue level).
    #[tokio::test]
    async fn two_sequential_requests_both_complete() {
        let live = tokio::time::timeout(TIMEOUT, LiveBackend::connect("."))
            .await
            .expect("no timeout")
            .expect("handshake ok");

        let (tx, mut rx) = mpsc::channel(32);
        let mut s1 = sample_submitted();
        live.submit_encoded(s1.clone(), tx.clone());
        // Consume all three events of request 1 first.
        let mut first_terminal = None;
        for _ in 0..3 {
            let evt = tokio::time::timeout(TIMEOUT, rx.recv())
                .await
                .expect("event in time")
                .expect("channel alive");
            first_terminal = Some(evt);
        }
        assert!(
            matches!(first_terminal, Some(BackendEvent::Result { .. })),
            "first request completes with Result"
        );

        s1.request_id = "99999999-8888-7777-6666-555555555555".to_owned();
        live.submit_encoded(s1, tx);
        let mut second_terminal = None;
        for _ in 0..3 {
            let evt = tokio::time::timeout(TIMEOUT, rx.recv())
                .await
                .expect("event in time")
                .expect("channel alive");
            second_terminal = Some(evt);
        }
        assert!(
            matches!(second_terminal, Some(BackendEvent::Result { .. })),
            "second request also completes with Result"
        );

        let _ = live.shutdown().await;
    }

    /// Cancel path: submitting then immediately cancelling yields the clean
    /// "Cancelled" error as the terminal event.
    #[tokio::test]
    async fn cancel_yields_clean_cancelled_error() {
        let live = tokio::time::timeout(TIMEOUT, LiveBackend::connect("."))
            .await
            .expect("no timeout")
            .expect("handshake ok");

        let (tx, mut rx) = mpsc::channel(16);
        let submitted = sample_submitted();
        live.submit_encoded(submitted.clone(), tx);

        // Wait until running, then cancel through the public API.
        loop {
            let evt = tokio::time::timeout(TIMEOUT, rx.recv())
                .await
                .expect("status in time")
                .expect("channel alive");
            if matches!(evt, BackendEvent::Running { .. }) {
                break;
            }
        }
        live.cancel(&submitted.request_id)
            .await
            .expect("cancel accepted");

        // Next event must be the terminal cancelled error.
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let evt = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("terminal event in time")
                .expect("channel alive");
            match evt {
                BackendEvent::Error { message, .. } => {
                    assert_eq!(message, "Cancelled");
                    break;
                }
                BackendEvent::Queued { .. } | BackendEvent::Running { .. } => continue,
                BackendEvent::Result { .. } => panic!("cancelled request must not yield Result"),
                BackendEvent::Disconnected => continue,
            }
        }

        let _ = live.shutdown().await;
    }
}
