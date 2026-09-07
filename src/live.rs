//! `LiveBackend` — real [`Backend`] implementation wrapping the protocol v1
//! [`RequestPipeline`] (tasks 3a/3b).
//!
//! Responsibilities:
//! * build a protocol `Request` per submitted prompt — ids come from
//!   [`crate::app::Submitted`] (client-chosen UUIDs), `workspace_root` from
//!   the pipeline's own configuration;
//! * translate the three progress sources into the single [`BackendEvent`]
//!   stream the mock-era UI consumes:
//!     - `status` frames (broadcast channel) → `Queued` / `Running`,
//!     - mid-turn `agent_event` frames (broadcast channel) → `AgentProgress`,
//!     - the per-request final outcome → `Result` / `Error`,
//! * targeted `cancel` delegation.
//!
//! The wire form of `thread_id` is a deterministic i64 hash of the chat's
//! stable UUID ([`wire_thread_id`]): protocol v1 defines `thread_id: integer`
//! while the TUI needs stable, persistable identity — hashing bridges both
//! without touching the frozen protocol types.
//!
//! One spawned glue task per submitted request; it terminates right after
//! the terminal event is forwarded (the status pump is aborted, never
//! leaked). The subagent pump outlives the terminal event on purpose:
//! background subagents finish after the result frame, so it keeps draining
//! until the backend retires the request's counter (zero-active `finished`)
//! or the agent channel closes.

use std::hash::{Hash, Hasher};
use std::path::Path;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::app::Submitted;
use crate::backend::{Backend, BackendEvent};
use crate::backend_client::BackendError;
use crate::protocol::{AgentEventKind, ServerMessage, StatusState};
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
    /// Slash commands the backend advertised at handshake (feat_thread_clone
    /// feature gate). Copied out of the pipeline handle because a
    /// reconnect respawns the same program and keeps the set valid.
    commands: Vec<String>,
}

impl LiveBackend {
    /// Spawn the backend process, run the mandatory handshake.
    ///
    /// Same seam as [`RequestPipeline::connect`]: the peer defaults to the real
    /// `chibi` binary (from PATH) and can be overridden with `CHIBI_FAKE_BACKEND`
    /// (seam for integration tests).
    pub async fn connect(workspace_root: impl AsRef<Path>) -> Result<Self, BackendError> {
        Self::connect_with_args(workspace_root, &[]).await
    }

    /// [`LiveBackend::connect`] with extra peer argv (fake-backend behaviour
    /// flags in integration tests, e.g. `--result-without-model`).
    pub async fn connect_with_args(
        workspace_root: impl AsRef<Path>,
        extra_args: &[String],
    ) -> Result<Self, BackendError> {
        let pipeline =
            RequestPipeline::connect_with_args(workspace_root.as_ref(), 64, extra_args).await?;
        let commands = pipeline.commands().to_vec();
        Ok(Self { pipeline, commands })
    }

    /// Slash commands the backend advertised in the handshake `ready` frame.
    /// Empty when the peer sent no capabilities (mocks, very old backends).
    pub fn backend_commands(&self) -> &[String] {
        &self.commands
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
            thread_id: None,
        });
    }
}

// ---------------------------------------------------------------------------
// Wire thread id
// ---------------------------------------------------------------------------

/// Deterministic, always-non-negative i64 form of a chat's stable UUID for
/// the protocol's `thread_id: integer` field. Stable across restarts (no
/// random salt). The sign bit is masked off so the result fits the Python
/// backend's `thread_id >= 0` validation constraint.
pub fn wire_thread_id(chat_uuid: &str) -> i64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    chat_uuid.hash(&mut hasher);
    // Mask off sign bit so result is always >= 0 (u64 high bit -> i64 negative).
    (hasher.finish() & 0x7FFF_FFFF_FFFF_FFFF) as i64
}

// ---------------------------------------------------------------------------
// Per-request glue task
// ---------------------------------------------------------------------------

/// Glue task for one submitted prompt: send, pump statuses, await the final
/// outcome, deliver exactly one terminal event, stop. The status pump dies
/// with the request; the subagent pump is never aborted — it outlives the
/// terminal event so post-result `agent_event` frames (background subagents
/// finishing after the answer) still reach the UI, and retires itself on the
/// backend's zero-active retirement frame or when the channel closes.
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
    // Same race removed for mid-turn subagent progress: subscribed before
    // the request hits the wire, so the first `agent_event` cannot slip by.
    let agent_rx = pipeline.subscribe_agent_events();

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
                    thread_id: Some(submitted.thread_id.clone()),
                })
                .await;
            return;
        }
    };

    let pump = tokio::spawn(pump_statuses(
        status_rx,
        submitted.request_id.clone(),
        submitted.thread_id.clone(),
        event_id,
        tx.clone(),
    ));
    // Deliberately not aborted with the status pump: the pump owns its
    // retirement (zero-active finished / channel close) so subagent frames
    // that arrive after the result below still flow.
    tokio::spawn(pump_agent_events(
        agent_rx,
        submitted.request_id.clone(),
        submitted.thread_id.clone(),
        event_id,
        tx.clone(),
    ));

    let event = terminal_event(result_rx.await, event_id, submitted.thread_id.clone());
    pump.abort();

    // Per-thread async: after THIS chat's terminal event the event loop may
    // need to start the next queued prompt of that chat. `send().await`
    // guarantees ordering — the drain signal never overtakes the terminal
    // event in the UI channel.
    let _ = tx.send(event).await;
    let _ = tx
        .send(BackendEvent::QueueDrain {
            thread_id: submitted.thread_id,
        })
        .await;
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
    thread_id: String,
) -> BackendEvent {
    match outcome {
        Ok(Ok(ServerMessage::Result {
            content,
            model,
            provider,
            usage,
            thoughts,
            ..
        })) => BackendEvent::Result {
            request_id: event_id,
            markdown: content,
            thread_id,
            model: resolve_model_label(model.as_deref(), provider.as_deref()),
            usage,
            thoughts,
        },
        Ok(Ok(unexpected)) => BackendEvent::Error {
            request_id: event_id,
            message: format!("unexpected final frame from backend: {unexpected:?}"),
            thread_id: Some(thread_id),
        },
        Ok(Err(e)) => BackendEvent::Error {
            request_id: event_id,
            message: render_pipeline_error(&e),
            thread_id: Some(thread_id),
        },
        Err(_) => BackendEvent::Error {
            request_id: event_id,
            message: "backend connection lost".to_owned(),
            thread_id: Some(thread_id),
        },
    }
}

/// feat_agent_model_label: pick the display label from a `result` frame.
///
/// The backend sends `model`/`provider` when it knows them. If both are
/// present, `model` wins (short display name); a `provider`-only frame still
/// labels the reply; whitespace-only values count as absent. `None` means
/// "no reliable identity" → the UI keeps the plain header (no "unknown").
fn resolve_model_label(model: Option<&str>, provider: Option<&str>) -> Option<String> {
    fn clean(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|s| !s.is_empty())
    }
    clean(model).or_else(|| clean(provider)).map(str::to_owned)
}

/// The chat's stable thread id travels with the glue task (it owns the
/// [`Submitted`] bundle); status frames only carry the protocol request id,
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
/// `thread_id` is the owning chat's stable id, stamped onto every event so
/// the app can route progress while several chats run concurrently.
async fn pump_statuses(
    mut status_rx: broadcast::Receiver<crate::request_pipeline::StatusUpdate>,
    request_id: String,
    thread_id: String,
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
                        thread_id: thread_id.clone(),
                    },
                    StatusState::Running => BackendEvent::Running {
                        request_id: event_id,
                        thread_id: thread_id.clone(),
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

/// Forward mid-turn `agent_event` frames for `request_id` as UI events
/// (same shape as [`pump_statuses`]): filter by request id, stamp the
/// owning chat's thread id, never terminal. The frame's `name` field is
/// parsed upstream but not carried — nothing displays it in v1.
///
/// Lifecycle: outlives the request's result frame so background subagents
/// finishing after the answer still update the counter. Retirement points:
/// the backend's zero-active `finished` for this request (its authoritative
/// end-of-stream — the counter state is retired exactly there and no
/// further frame for the request can follow), the agent channel closing
/// (backend gone / shutdown), or the UI receiver dropping. Idle pumps cost
/// one broadcast receiver each; the channel itself stays capacity-bounded.
async fn pump_agent_events(
    mut agent_rx: broadcast::Receiver<crate::request_pipeline::AgentEventUpdate>,
    request_id: String,
    thread_id: String,
    event_id: u64,
    tx: mpsc::Sender<BackendEvent>,
) {
    loop {
        match agent_rx.recv().await {
            Ok(update) => {
                if update.request_id != request_id {
                    continue; // some other request's subagents
                }
                let event = BackendEvent::AgentProgress {
                    request_id: event_id,
                    thread_id: thread_id.clone(),
                    event: update.event,
                    active: update.active,
                    total: update.total,
                };
                if tx.send(event).await.is_err() {
                    return; // UI receiver dropped
                }
                if update.event == AgentEventKind::Finished && update.active == 0 {
                    return; // backend retired this request's counter: end-of-stream
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

    // Tests in this module target the fake backend — set the env var once.
    static ONCE: std::sync::Once = std::sync::Once::new();
    fn with_fake_backend() {
        ONCE.call_once(|| {
            std::env::set_var("CHIBI_FAKE_BACKEND", "tests/fake_backend.py");
        });
    }

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

    /// `wire_thread_id` must always return >= 0 (Python backend rejects negatives).
    /// Stress-test across many synthetic UUIDs to ensure the sign-bit mask is effective.
    #[test]
    fn wire_thread_id_always_non_negative() {
        for i in 0..2000u64 {
            let uuid = format!("{:036x}-{:04x}-{:04x}-{:04x}-{:012x}", i, i, i, i, i);
            let id = wire_thread_id(&uuid);
            assert!(
                id >= 0,
                "wire_thread_id({uuid}) = {id} is negative — sign bit not masked"
            );
        }
    }

    /// Terminal-event mapping: a normal result frame becomes
    /// `BackendEvent::Result` carrying the content verbatim; a fieldless
    /// frame (old backend / fixtures without model) yields no label.
    #[test]
    fn terminal_event_maps_result_content_verbatim() {
        let outcome: Result<PipelineResult, oneshot::error::RecvError> =
            Ok(Ok(ServerMessage::Result {
                request_id: "x".into(),
                content: "**42**".into(),
                model: None,
                provider: None,
                usage: None,
                thoughts: None,
            }));
        match terminal_event(outcome, 7, "thread-a".to_owned()) {
            BackendEvent::Result {
                request_id,
                markdown,
                thread_id,
                model,
                ..
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(markdown, "**42**");
                assert_eq!(thread_id, "thread-a");
                assert_eq!(model, None, "fieldless frame → plain label");
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    // ---- feat_agent_model_label: result-frame model label -----------------

    /// Both fields present → the short `model` display name wins.
    #[test]
    fn resolve_model_label_prefers_model_over_provider() {
        assert_eq!(
            resolve_model_label(Some("glm-5.2"), Some("zhipu")),
            Some("glm-5.2".to_owned())
        );
    }

    /// Provider-only frames still label the reply; whitespace-only values
    /// count as absent; nothing at all stays `None`.
    #[test]
    fn resolve_model_label_falls_back_to_provider() {
        assert_eq!(
            resolve_model_label(None, Some("moonshot")),
            Some("moonshot".to_owned())
        );
        assert_eq!(
            resolve_model_label(Some("  "), Some("moonshot")),
            Some("moonshot".to_owned()),
            "blank model must not shadow a usable provider"
        );
        assert_eq!(
            resolve_model_label(Some("kimi-k2.7"), None),
            Some("kimi-k2.7".to_owned())
        );
        assert_eq!(resolve_model_label(None, None), None);
        assert_eq!(resolve_model_label(Some(" "), Some("  ")), None);
    }

    /// A labelled result frame stamps the resolved label onto the event that
    /// resolves the pending message.
    #[test]
    fn terminal_event_carries_resolved_model_label() {
        let outcome: Result<PipelineResult, oneshot::error::RecvError> =
            Ok(Ok(ServerMessage::Result {
                request_id: "x".into(),
                content: "answer".into(),
                model: Some("glm-5.2".into()),
                provider: Some("zhipu".into()),
                usage: None,
                thoughts: None,
            }));
        match terminal_event(outcome, 1, "t".to_owned()) {
            BackendEvent::Result { model, .. } => assert_eq!(model.as_deref(), Some("glm-5.2")),
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
        match terminal_event(outcome, 1, "thread-b".to_owned()) {
            BackendEvent::Error {
                message, thread_id, ..
            } => {
                assert_eq!(message, "Cancelled");
                assert_eq!(thread_id.as_deref(), Some("thread-b"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Terminal-event mapping: a broken pipe (peer died) becomes an Error
    /// event instead of hanging the UI.
    #[test]
    fn terminal_event_maps_broken_pipe_to_error() {
        let outcome: Result<PipelineResult, oneshot::error::RecvError> =
            Ok(Err(BackendError::Broken("child died".into())));
        match terminal_event(outcome, 2, "thread-c".to_owned()) {
            BackendEvent::Error { message, .. } => {
                assert!(message.contains("child died"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Every terminal event is followed by exactly one `QueueDrain` signal
    /// for the same chat — the per-thread FIFO pump depends on it.
    #[test]
    fn terminal_events_are_always_followed_by_queue_drain_signal() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(4);

            // Normal result → drain.
            let outcome_ok: Result<PipelineResult, oneshot::error::RecvError> =
                Ok(Ok(ServerMessage::Result {
                    request_id: "r".into(),
                    content: "ok".into(),
                    model: None,
                    provider: None,
                    usage: None,
                    thoughts: None,
                }));
            tx.send(terminal_event(outcome_ok, 1, "t1".into()))
                .await
                .unwrap();
            tx.send(BackendEvent::QueueDrain {
                thread_id: "t1".into(),
            })
            .await
            .unwrap();
            let first = rx.recv().await.expect("first");
            let second = rx.recv().await.expect("second");
            assert!(matches!(first, BackendEvent::Result { .. }));
            match second {
                BackendEvent::QueueDrain { thread_id } => assert_eq!(thread_id, "t1"),
                other => panic!("expected QueueDrain after Result, got {other:?}"),
            }

            // Broken pipe → still a drain.
            let outcome_err: Result<PipelineResult, oneshot::error::RecvError> =
                Ok(Err(BackendError::Broken("child died".into())));
            tx.send(terminal_event(outcome_err, 2, "t2".into()))
                .await
                .unwrap();
            tx.send(BackendEvent::QueueDrain {
                thread_id: "t2".into(),
            })
            .await
            .unwrap();
            let third = rx.recv().await.expect("third");
            let fourth = rx.recv().await.expect("fourth");
            assert!(matches!(third, BackendEvent::Error { .. }));
            match fourth {
                BackendEvent::QueueDrain { thread_id } => assert_eq!(thread_id, "t2"),
                other => panic!("expected QueueDrain after Error, got {other:?}"),
            }
            drop(tx);
        });
    }

    /// Full round trip against the deterministic fake peer:
    /// submit → Queued → Running → Result reaches the UI channel.
    ///
    /// Uses `LiveBackend::submit_encoded`, the real entry point main.rs uses
    /// after `App::take_input`.
    #[tokio::test]
    async fn submit_delivers_status_and_result_events_via_fake_peer() {
        with_fake_backend();
        let live = tokio::time::timeout(TIMEOUT, LiveBackend::connect("."))
            .await
            .expect("connect within timeout")
            .expect("handshake ok");

        let (tx, mut rx) = mpsc::channel(16);

        // Drain events concurrently; assert on what arrives within budget.
        let sample = sample_submitted();
        let drainer = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..4 {
                match tokio::time::timeout(TIMEOUT, rx.recv()).await {
                    Ok(Some(evt)) => seen.push(evt),
                    _ => break,
                }
            }
            seen
        });

        live.submit_encoded(sample.clone(), tx);

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
            BackendEvent::Result {
                markdown,
                thread_id,
                model,
                ..
            } => {
                assert_eq!(
                    markdown, "This function `foo` returns the integer `42`.",
                    "result content passes through verbatim"
                );
                assert_eq!(
                    thread_id.as_str(),
                    sample.thread_id,
                    "event routed by thread id"
                );
                assert_eq!(
                    model.as_deref(),
                    Some("gpt-example"),
                    "labelled fake frame carries the model label end-to-end"
                );
            }
            other => panic!("third event must be Result, got {other:?}"),
        }
        // Per-thread async: the terminal event is followed by the queue-drain
        // signal for the same chat.
        match &events[3] {
            BackendEvent::QueueDrain { thread_id } => {
                assert_eq!(thread_id.as_str(), sample.thread_id)
            }
            other => panic!("fourth event must be QueueDrain, got {other:?}"),
        }

        let _ = live.shutdown().await;
    }

    /// feat_agent_model_label fallback, end-to-end: a backend variant that
    /// omits `model`/`provider` on the result frame must yield `None` on the
    /// terminal event — the UI then keeps the plain `● Chibi` header.
    #[tokio::test]
    async fn fieldless_result_frame_yields_no_model_label_end_to_end() {
        with_fake_backend();
        let fieldless: Vec<String> = vec!["--result-without-model".to_owned()];
        let live = tokio::time::timeout(TIMEOUT, LiveBackend::connect_with_args(".", &fieldless))
            .await
            .expect("connect within timeout")
            .expect("handshake ok");

        let (tx, mut rx) = mpsc::channel(16);
        live.submit_encoded(sample_submitted(), tx);

        // Drain the four deterministic events of one request.
        let mut terminal = None;
        for _ in 0..4 {
            let evt = tokio::time::timeout(TIMEOUT, rx.recv())
                .await
                .expect("event in time")
                .expect("channel alive");
            if matches!(evt, BackendEvent::Result { .. }) {
                terminal = Some(evt);
            }
        }
        match terminal {
            Some(BackendEvent::Result { model, .. }) => {
                assert_eq!(model, None, "fieldless frame must not invent a label");
            }
            other => panic!("expected terminal Result, got {other:?}"),
        }

        let _ = live.shutdown().await;
    }

    /// Two sequential requests against the fake peer both complete with
    /// distinct protocol request ids (correlation sanity at the glue level).
    #[tokio::test]
    async fn two_sequential_requests_both_complete() {
        with_fake_backend();
        let live = tokio::time::timeout(TIMEOUT, LiveBackend::connect("."))
            .await
            .expect("no timeout")
            .expect("handshake ok");

        let (tx, mut rx) = mpsc::channel(32);
        let mut s1 = sample_submitted();
        live.submit_encoded(s1.clone(), tx.clone());
        // Consume all four events of request 1 (statuses + terminal + drain).
        let mut first_terminal = None;
        for _ in 0..4 {
            let evt = tokio::time::timeout(TIMEOUT, rx.recv())
                .await
                .expect("event in time")
                .expect("channel alive");
            if !matches!(
                evt,
                BackendEvent::Queued { .. }
                    | BackendEvent::Running { .. }
                    | BackendEvent::QueueDrain { .. }
            ) {
                first_terminal = Some(evt);
            }
        }
        assert!(
            matches!(first_terminal, Some(BackendEvent::Result { .. })),
            "first request completes with Result"
        );

        s1.request_id = "99999999-8888-7777-6666-555555555555".to_owned();
        live.submit_encoded(s1, tx);
        let mut second_terminal = None;
        for _ in 0..4 {
            let evt = tokio::time::timeout(TIMEOUT, rx.recv())
                .await
                .expect("event in time")
                .expect("channel alive");
            if !matches!(
                evt,
                BackendEvent::Queued { .. }
                    | BackendEvent::Running { .. }
                    | BackendEvent::QueueDrain { .. }
            ) {
                second_terminal = Some(evt);
            }
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
        with_fake_backend();
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
                BackendEvent::Queued { .. }
                | BackendEvent::Running { .. }
                | BackendEvent::AgentProgress { .. }
                | BackendEvent::QueueDrain { .. } => continue,
                BackendEvent::Result { .. } => panic!("cancelled request must not yield Result"),
                BackendEvent::Disconnected => continue,
            }
        }

        let _ = live.shutdown().await;
    }

    // ---- feat_thread_clone: command round trip on the fake peer ------------

    /// The clone command rides ON the destination thread: the fake peer
    /// echoes the received frame inside the ack, so this asserts the wire
    /// shape end to end (frame thread_id = destination wire id, args =
    /// source wire id + clone title).
    #[tokio::test]
    async fn clone_command_round_trips_on_destination_thread() {
        with_fake_backend();
        let live = tokio::time::timeout(TIMEOUT, LiveBackend::connect("."))
            .await
            .expect("connect within timeout")
            .expect("handshake ok");
        // Detection over a real handshake: the fake advertises the command,
        // exactly like the real backend since the clone support task.
        assert!(
            live.backend_commands()
                .iter()
                .any(|c| c == "/new_thread_with_current_context"),
            "capabilities must list the clone command: {:?}",
            live.backend_commands()
        );

        let (tx, mut rx) = mpsc::channel(16);
        let source_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let dest_uuid = "bbbbbbbb-cccc-dddd-eeee-ffffffffffff";
        live.submit_encoded(
            Submitted {
                request_id: "99999999-8888-7777-6666-555555555555".to_owned(),
                thread_id: dest_uuid.to_owned(),
                prompt: format!(
                    "/new_thread_with_current_context {} Source (copy)",
                    wire_thread_id(source_uuid)
                ),
            },
            tx,
        );

        let mut terminal = None;
        for _ in 0..4 {
            let evt = tokio::time::timeout(TIMEOUT, rx.recv())
                .await
                .expect("event in time")
                .expect("channel alive");
            if matches!(evt, BackendEvent::Result { .. }) {
                terminal = Some(evt);
            }
        }
        match terminal {
            Some(BackendEvent::Result {
                markdown,
                thread_id,
                ..
            }) => {
                assert_eq!(thread_id, dest_uuid, "ack routed to the destination thread");
                assert!(
                    markdown.contains(&format!("dest={}", wire_thread_id(dest_uuid))),
                    "frame thread_id must be the DESTINATION wire id: {markdown}"
                );
                assert!(
                    markdown.contains(&format!("args={}", wire_thread_id(source_uuid))),
                    "args must carry the source wire id: {markdown}"
                );
            }
            other => panic!("expected terminal Result, got {other:?}"),
        }

        let _ = live.shutdown().await;
    }

    /// Malformed clone args come back as a readable error: the fake answers
    /// the same frontend-facing code the real backend uses for validation
    /// failures, which must parse or the request would hang forever.
    #[tokio::test]
    async fn clone_command_error_surfaces_readable_text() {
        with_fake_backend();
        let live = tokio::time::timeout(TIMEOUT, LiveBackend::connect("."))
            .await
            .expect("connect within timeout")
            .expect("handshake ok");

        let (tx, mut rx) = mpsc::channel(16);
        live.submit_encoded(
            Submitted {
                request_id: "99999999-8888-7777-6666-555555555554".to_owned(),
                thread_id: "cccccccc-cccc-cccc-cccc-cccccccccccc".to_owned(),
                prompt: "/new_thread_with_current_context".to_owned(),
            },
            tx,
        );

        let mut terminal = None;
        for _ in 0..4 {
            let evt = tokio::time::timeout(TIMEOUT, rx.recv())
                .await
                .expect("event in time")
                .expect("channel alive");
            if matches!(evt, BackendEvent::Error { .. }) {
                terminal = Some(evt);
            }
        }
        match terminal {
            Some(BackendEvent::Error { message, .. }) => {
                assert!(message.contains("Usage:"), "readable text: {message}");
            }
            other => panic!("expected terminal Error, got {other:?}"),
        }

        let _ = live.shutdown().await;
    }
}
