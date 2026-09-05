//! Request pipeline for the real backend client (task 3b).
//!
//! Sits on top of [`crate::backend_client`] (task 3a) and adds what 3a
//! deliberately left out:
//!
//! * **Correlation** — every [`ClientMessage::Request`] gets a
//!   `request_id → sender` entry; the reader sub-task feeds parsed frames to
//!   the actor, which resolves the entry with the final
//!   [`ServerMessage::Result`].
//! * **Progress without blocking** — [`ServerMessage::Status`] frames go to a
//!   broadcast channel ([`RequestPipeline::subscribe_status`]); they are
//!   structurally unable to consume a request's final-outcome receiver.
//! * **Per-request cancel** — [`RequestPipeline::cancel`] sends a targeted
//!   `cancel` frame; the backend answers `error { code: cancelled }`, which is
//!   routed to exactly that request. Never thread-wide.
//! * **Broken-pipe handling + reconnect** — when stdout EOFs or I/O fails,
//!   all pending requests are failed with [`BackendError::Broken`], and
//!   [`RequestPipeline::reconnect`] respawns + re-handshakes a fresh backend.
//!
//! Architecture: ONE owner actor task holds the pending map (no locks, no
//! data races). The public handle only clones cheap channel endpoints:
//!
//! ```text
//! RequestPipeline (clone-able handle)
//!   ├─ cmd_tx ──────────────► Actor task (owns pending map + stdin)
//!   │                           ├─ reader sub-task: read_line → ActorEvent
//!   │                           ├─ Status    → broadcast fan-out
//!   │                           ├─ Result/Error → per-request oneshot pair
//!   │                           └─ Died      → fail all pending as Broken
//!   ├─ result rx (per request) ◄┘  (final outcome ONLY)
//!   └─ status_rx (broadcast)   ◄── statuses never touch the result path
//! ```
//!
//! Global errors (`error` with `request_id: null`) are intentionally dropped:
//! there is no request to correlate them with, and handshake-phase artifacts
//! are already handled inside [`crate::backend_client`]. A repeated `ready`
//! after the handshake is likewise ignored.
//!
//! Spec: `.project/ide_integration/architecture_specification.md` §4.
//! Fixtures: `tests/fixtures/ide_protocol/`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::backend_client::{spawn_argv, BackendError, ReapHandle};
use crate::diag;
use crate::protocol::{ClientMessage, ServerMessage, StatusState};

/// How long [`Command::Shutdown`] waits for the backend to exit gracefully
/// before escalating to kill. Same budget as the raw client.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Capacity of the internal actor-event queue (frames from the reader +
/// death notices). Deep enough that a chatty backend never blocks the reader.
const EVENT_CHANNEL_CAPACITY: usize = 256;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// Coarse lifecycle update delivered out-of-band (never blocks or consumes
/// the per-request result channel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusUpdate {
    pub request_id: StatusRequestId,
    pub state: StatusState,
}

/// Newtype over `String`: request ids stay opaque to consumers of the status
/// stream.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StatusRequestId(pub String);

impl std::fmt::Display for StatusUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.request_id.0, self.state)
    }
}

/// A fully-formed protocol request ready for
/// [`RequestPipeline::send_request`].
///
/// Construct with [`RequestArgs::new`] plus the optional builder methods.
/// Serialization happens inside the actor, so callers cannot emit multi-line
/// frames; the workspace root always comes from the pipeline's configuration.
#[derive(Clone, Debug)]
pub struct RequestArgs {
    pub(crate) request_id: String,
    pub(crate) thread_id: i64,
    pub(crate) prompt: String,
    /// Never serialized from here: the actor substitutes the pipeline's
    /// configured root (kept so a caller may inspect/override later).
    #[allow(dead_code)]
    pub(crate) workspace_root: String,
    pub(crate) active_file: Option<String>,
    /// Boxed: `Selection` carries inline file text and is rarely present,
    /// keeping `RequestArgs` cheap to clone.
    pub(crate) selection: Option<Box<crate::protocol::Selection>>,
    pub(crate) cursor_position: Option<crate::protocol::CursorPosition>,
    pub(crate) language_id: Option<String>,
}

impl RequestArgs {
    /// Minimal request: id, thread and prompt.
    pub fn new(request_id: impl Into<String>, thread_id: i64, prompt: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            thread_id,
            prompt: prompt.into(),
            workspace_root: String::new(), // filled from config in the actor
            active_file: None,
            selection: None,
            cursor_position: None,
            language_id: None,
        }
    }

    pub fn active_file(mut self, path: impl Into<String>) -> Self {
        self.active_file = Some(path.into());
        self
    }

    pub fn selection(mut self, selection: crate::protocol::Selection) -> Self {
        self.selection = Some(Box::new(selection));
        self
    }

    pub fn cursor_position(mut self, position: crate::protocol::CursorPosition) -> Self {
        self.cursor_position = Some(position);
        self
    }

    pub fn language_id(mut self, language: impl Into<String>) -> Self {
        self.language_id = Some(language.into());
        self
    }
}

/// The final answer for one request, delivered through its dedicated channel.
pub(crate) type PipelineResult = Result<ServerMessage, BackendError>;

// ---------------------------------------------------------------------------
// Actor command/event protocol
// ---------------------------------------------------------------------------

enum Command {
    /// Serialize + send a request, register its correlation entry.
    Send {
        args: RequestArgs,
        result_tx: oneshot::Sender<PipelineResult>,
        reply: oneshot::Sender<Result<(), BackendError>>,
    },
    /// Targeted cancel of one in-flight request.
    Cancel {
        request_id: String,
        reply: oneshot::Sender<Result<(), BackendError>>,
    },
    /// Graceful shutdown of backend + actor.
    Shutdown {
        reply: oneshot::Sender<Result<(), BackendError>>,
    },
    /// Respawn + re-handshake after breakage.
    Reconnect {
        reply: oneshot::Sender<Result<(), BackendError>>,
    },
}

/// Events flowing from the reader sub-task into the actor.
enum ActorEvent {
    Frame(ServerMessage),
    /// Stdout EOF or I/O error; carries a human-readable reason.
    Died(String),
}

/// Static configuration for one pipeline lifetime (survives reconnects).
struct ActorConfig {
    workspace_root: String,
    script_path: String,
}

/// Everything mutable the actor owns. Single owner ⇒ no locks.
struct ActorState {
    stdin: Option<tokio::process::ChildStdin>,
    reaper: Option<ReapHandle>,
    /// Correlation table: request_id → sender half of the final-outcome pair.
    pending: HashMap<String, oneshot::Sender<PipelineResult>>,
    broken_reason: Option<String>,
}

type PartsTuple = (
    Option<tokio::process::ChildStdin>,
    Option<tokio::io::BufReader<tokio::process::ChildStdout>>,
    ReapHandle,
);

impl ActorState {
    /// Serialize `message` and write it as one JSONL frame (line + `\n` +
    /// flush) to the backend's stdin. On write failure the pipe is treated as
    /// broken (all pending failed) and the I/O error surfaces to the caller.
    async fn send_frame(&mut self, message: &ClientMessage) -> Result<(), BackendError> {
        let line = serde_json::to_string(message)
            .map_err(|e| BackendError::Broken(format!("cannot serialize frame: {e}")))?;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| BackendError::Broken("backend stdin already closed".to_owned()))?;

        if let Err(e) = stdin.write_all(line.as_bytes()).await {
            let reason = e.to_string();
            self.broken_pipe(reason);
            return Err(BackendError::Io(e));
        }
        if let Err(e) = stdin.write_all(b"\n").await {
            let reason = e.to_string();
            self.broken_pipe(reason);
            return Err(BackendError::Io(e));
        }
        if let Err(e) = stdin.flush().await {
            let reason = e.to_string();
            self.broken_pipe(reason);
            return Err(BackendError::Io(e));
        }
        Ok(())
    }

    /// Mark the pipeline broken and fail every pending request with
    /// [`BackendError::Broken`] (best-effort). The actor keeps running so
    /// [`Command::Reconnect`] / [`Command::Shutdown`] stay serviceable.
    fn broken_pipe(&mut self, reason: String) {
        for (_, tx) in self.pending.drain() {
            let _ = tx.send(Err(BackendError::Broken(reason.clone())));
        }
        self.broken_reason = Some(reason);
    }
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Cloneable handle to the running pipeline actor.
///
/// Obtain via [`RequestPipeline::connect_with_args`]; every clone talks to the
/// same backend process and shares the same correlation table.
#[derive(Clone)]
pub struct RequestPipeline {
    cmd_tx: mpsc::Sender<Command>,
    status_tx: broadcast::Sender<StatusUpdate>,
    /// Slash commands the backend advertised in the handshake `ready` frame
    /// (feat_thread_clone consumers). Captured once at connect; a reconnect
    /// spawns the same program, so the set stays valid for the handle's life.
    commands: Vec<String>,
}

impl RequestPipeline {
    /// The script defaults to `chibi` (from PATH) and can be overridden
    /// with the `CHIBI_FAKE_BACKEND` env var (seam for running tests from
    /// other working directories).
    pub async fn connect(
        workspace_root: impl AsRef<Path>,
        status_channel_capacity: usize,
    ) -> Result<Self, BackendError> {
        Self::connect_with_args(workspace_root, status_channel_capacity, &[]).await
    }

    /// [`RequestPipeline::connect`] with extra argv appended after the script
    /// path (e.g. fake-backend mode flags like `--crash-after-running`).
    pub async fn connect_with_args(
        workspace_root: impl AsRef<Path>,
        status_channel_capacity: usize,
        extra_args: &[String],
    ) -> Result<Self, BackendError> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let script_path =
            std::env::var("CHIBI_FAKE_BACKEND").unwrap_or_else(|_| "chibi".to_owned());

        let mut argv: Vec<std::ffi::OsString> = if script_path == "chibi" {
            // Real backend: chibi ide --stdio (no extra CLI flags: workspace_root
            // travels inside request frames, not on the command line).
            vec!["chibi".into(), "ide".into(), "--stdio".into()]
        } else {
            // Fake backend (tests): python3 tests/fake_backend.py --workspace <root>
            vec![
                "python3".into(),
                script_path.clone().into(),
                "--workspace".into(),
                workspace_root.as_os_str().to_owned(),
            ]
        };
        argv.extend(extra_args.iter().map(Into::into));

        let mut client = spawn_argv(&workspace_root.display().to_string(), argv).await?;
        let ready = client.handshake().await?;
        let commands = ready.commands().to_vec();

        let parts = client.into_parts();
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (status_tx, _) = broadcast::channel(status_channel_capacity.max(1));

        tokio::spawn(actor_loop(
            parts,
            cmd_rx,
            status_tx.clone(),
            ActorConfig {
                workspace_root: workspace_root.display().to_string(),
                script_path,
            },
        ));

        Ok(Self {
            cmd_tx,
            status_tx,
            commands,
        })
    }

    /// Slash-command set the backend advertised at handshake (empty when the
    /// `ready` frame carried no capabilities). Feature gates read this
    /// instead of assuming (protocol v1.1 rule: consume via detection).
    pub fn commands(&self) -> &[String] {
        &self.commands
    }

    /// Submit a request; returns immediately with a one-shot receiver for the
    /// FINAL outcome (`result`, or `error` mapped onto [`BackendError`]).
    /// Status updates arrive on the broadcast status channel only — never
    /// here.
    pub async fn send_request(
        &self,
        request: RequestArgs,
    ) -> Result<oneshot::Receiver<PipelineResult>, BackendError> {
        let (result_tx, result_rx) = oneshot::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Send {
                args: request,
                result_tx,
                reply: reply_tx,
            })
            .await
            .map_err(|_| actor_gone())?;
        reply_rx
            .await
            .map_err(|_| actor_gone())?
            .map(|()| result_rx)
    }

    /// Cancel one in-flight request by id. Strictly per-request: unknown ids
    /// fail fast without touching the wire. The backend's
    /// `error { code: cancelled }` answer resolves only that request.
    pub async fn cancel(&self, request_id: &str) -> Result<(), BackendError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Cancel {
                request_id: request_id.to_owned(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| actor_gone())?;
        reply_rx.await.map_err(|_| actor_gone())?
    }

    /// Subscribe to coarse `queued`/`running` progress. Multiple consumers
    /// allowed (broadcast fan-out); a full buffer drops oldest updates rather
    /// than ever blocking the reader loop.
    pub fn subscribe_status(&self) -> broadcast::Receiver<StatusUpdate> {
        self.status_tx.subscribe()
    }

    /// Respawn the backend after a broken pipe / premature exit and restore
    /// handshake state. Pending requests were already failed with
    /// [`BackendError::Broken`] at breakage time.
    pub async fn reconnect(&mut self) -> Result<(), BackendError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Reconnect { reply: reply_tx })
            .await
            .map_err(|_| actor_gone())?;
        reply_rx.await.map_err(|_| actor_gone())?
    }

    /// Graceful shutdown: best-effort `shutdown` frame, wait up to
    /// `SHUTDOWN_BUDGET` for exit 0 (kill on timeout), stop the actor.
    pub async fn shutdown(&self) -> Result<(), BackendError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Shutdown { reply: reply_tx })
            .await
            .map_err(|_| actor_gone())?;
        reply_rx.await.map_err(|_| actor_gone())?
    }
}

/// Error returned when the actor task itself is gone (e.g. handle used after
/// shutdown).
fn actor_gone() -> BackendError {
    BackendError::Broken("pipeline actor stopped".to_owned())
}

// ---------------------------------------------------------------------------
// Actor
// ---------------------------------------------------------------------------

async fn actor_loop(
    initial_parts: PartsTuple,
    mut cmd_rx: mpsc::Receiver<Command>,
    status_tx: broadcast::Sender<StatusUpdate>,
    config: ActorConfig,
) {
    let mut state = ActorState {
        stdin: initial_parts.0,
        reaper: Some(initial_parts.2),
        pending: HashMap::new(),
        broken_reason: None,
    };

    // Reader #1 over the initial stdout.
    let (event_tx, mut event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    if let Some(stdout) = initial_parts.1 {
        spawn_reader(stdout, event_tx.clone());
    }

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    Command::Send { args, result_tx, reply } => {
                        let res = handle_send(&mut state, &config, args, result_tx).await;
                        let _ = reply.send(res);
                    }
                    Command::Cancel { request_id, reply } => {
                        let res = handle_cancel(&mut state, request_id).await;
                        let _ = reply.send(res);
                    }
                    Command::Shutdown { reply } => {
                        let res = handle_shutdown(&mut state).await;
                        let _ = reply.send(res);
                        // Actor stops here; the old reader exits once the dead
                        // child's stdout EOFs (or its event send fails).
                        return;
                    }
                    Command::Reconnect { reply } => {
                        let res = handle_reconnect(&mut state, &config, &event_tx).await;
                        let _ = reply.send(res);
                    }
                }
            }
            event = event_rx.recv() => match event {
                Some(ActorEvent::Frame(msg)) => {
                    dispatch_frame(&mut state, &status_tx, msg).await;
                }
                Some(ActorEvent::Died(reason)) => {
                    // feat_stderr_log_modal: pipe death is a diagnostic lifecycle event too:
                    // one unified stream with stderr.
                    diag::append_tui(format!("pipe closed: {reason}"));
                    state.broken_pipe(reason);
                }
                // The reader is the only event sender: the task that recieves frames
                // from the child. None means it is gone (child death after shutdown):
                // nothing to do.
                None => {}
            },
        }
    }
}

/// `Command::Send`: register the correlation entry and put the request frame
/// on the wire. Broken pipelines resolve the returned receiver immediately;
/// duplicate ids are rejected before any state changes.
async fn handle_send(
    state: &mut ActorState,
    config: &ActorConfig,
    args: RequestArgs,
    result_tx: oneshot::Sender<PipelineResult>,
) -> Result<(), BackendError> {
    if let Some(reason) = &state.broken_reason {
        // Resolve this caller's receiver too, then surface the same failure.
        let _ = result_tx.send(Err(BackendError::Broken(reason.clone())));
        return Err(BackendError::Broken(reason.clone()));
    }
    if state.pending.contains_key(&args.request_id) {
        return Err(BackendError::Broken(format!(
            "duplicate request id: {}",
            args.request_id
        )));
    }

    let message = ClientMessage::Request {
        request_id: args.request_id.clone(),
        thread_id: args.thread_id,
        prompt: args.prompt,
        workspace_root: config.workspace_root.clone(),
        active_file: args.active_file,
        selection: args.selection.map(|boxed| *boxed),
        cursor_position: args.cursor_position,
        language_id: args.language_id,
    };

    match state.send_frame(&message).await {
        Ok(()) => {
            state.pending.insert(args.request_id, result_tx);
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// `Command::Cancel`: strictly per-request; unknown ids fail fast without a
/// round trip.
async fn handle_cancel(state: &mut ActorState, request_id: String) -> Result<(), BackendError> {
    if let Some(reason) = &state.broken_reason {
        return Err(BackendError::Broken(reason.clone()));
    }
    if !state.pending.contains_key(&request_id) {
        return Err(BackendError::UnknownRequest { request_id });
    }
    state
        .send_frame(&ClientMessage::Cancel {
            request_id: request_id.clone(),
        })
        .await?;
    // The backend answers `error { code: cancelled }`; dispatch_frame routes
    // that frame to exactly this request. Nothing else to do here.
    Ok(())
}

/// `Command::Shutdown`: best-effort `shutdown` frame, then reap within
/// [`SHUTDOWN_BUDGET`]; kill on timeout.
async fn handle_shutdown(state: &mut ActorState) -> Result<(), BackendError> {
    // Best effort only: the real verdict comes from the exit status below.
    let _ = state.send_frame(&ClientMessage::Shutdown {}).await;
    match state.reaper.take() {
        Some(mut reaper) => match reaper.graceful_reap(SHUTDOWN_BUDGET).await {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(BackendError::UnexpectedExit {
                status: status.into(),
            }),
            Err(err) => Err(err),
        },
        None => Err(BackendError::Broken(
            "no live backend to shut down".to_owned(),
        )),
    }
}

/// `Command::Reconnect`: tear down whatever is left, respawn + re-handshake a
/// fresh backend, replace pipes, start a new reader.
async fn handle_reconnect(
    state: &mut ActorState,
    config: &ActorConfig,
    event_tx: &mpsc::Sender<ActorEvent>,
) -> Result<(), BackendError> {
    if let Some(reaper) = state.reaper.take() {
        reaper.kill().await;
    }
    state.stdin = None;

    let argv: Vec<std::ffi::OsString> = if config.script_path == "chibi" {
        // Reconnect to real backend: same argv as initial spawn (no --workspace).
        vec!["chibi".into(), "ide".into(), "--stdio".into()]
    } else {
        // Reconnect to fake backend: python3 <script> --workspace <root>.
        vec![
            "python3".into(),
            config.script_path.clone().into(),
            "--workspace".into(),
            config.workspace_root.clone().into(),
        ]
    };

    let spawn_result = async {
        let mut client = spawn_argv(&config.workspace_root, argv).await?;
        client.handshake().await?;
        Ok::<_, BackendError>(client.into_parts())
    }
    .await;

    match spawn_result {
        Ok((stdin, stdout, reaper)) => {
            state.stdin = stdin;
            state.reaper = Some(reaper);
            if let Some(stdout) = stdout {
                spawn_reader(stdout, event_tx.clone());
            }
            state.broken_reason = None;
            diag::append_tui("reconnect ok");
            Ok(())
        }
        Err(err) => {
            state.broken_reason = Some(format!("reconnect failed: {err}"));
            diag::append_tui(format!("reconnect failed: {err}"));
            Err(err)
        }
    }
}

/// Route one parsed server frame to its destination.
async fn dispatch_frame(
    state: &mut ActorState,
    status_tx: &broadcast::Sender<StatusUpdate>,
    msg: ServerMessage,
) {
    match msg {
        ServerMessage::Status {
            request_id,
            state: st,
        } => {
            // BroadcastError just means nobody is subscribed, that is fine.
            let _ = status_tx.send(StatusUpdate {
                request_id: StatusRequestId(request_id),
                state: st,
            });
        }
        ServerMessage::Result {
            request_id,
            content,
            model,
            provider,
            usage,
            thoughts,
        } => {
            if let Some(tx) = state.pending.remove(&request_id) {
                let _ = tx.send(Ok(ServerMessage::Result {
                    request_id,
                    content,
                    model,
                    provider,
                    usage,
                    thoughts,
                }));
            }
        }
        ServerMessage::Error {
            request_id: Some(id),
            code,
            message,
            ..
        } => {
            if let Some(tx) = state.pending.remove(&id) {
                let _ = tx.send(Err(BackendError::RequestFailed {
                    request_id: Some(id),
                    code,
                    message,
                }));
            }
        }
        // Global errors carry no request_id: there is nothing to correlate
        // them with, and handshake-phase artifacts never reach this
        // dispatcher. Intentionally dropped.
        ServerMessage::Error {
            request_id: None, ..
        } => {}
        // A repeated `ready` after the handshake is a no-op for the pipeline.
        ServerMessage::Ready { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Reader sub-task
// ---------------------------------------------------------------------------

/// Parse one JSONL stdout line into a [`ServerMessage`]. Reader hardening:
/// a line that carries an unknown `type` tag (forward protocol growth) is
/// reported to the diagnostics stream instead of vanishing silently; a plain
/// parse failure (garbage line) keeps the old silent-drop behavior so a
/// noisy backend still cannot kill the pipeline. `None` = drop the line.
fn parse_frame(line: &str) -> Option<ServerMessage> {
    match serde_json::from_str::<ServerMessage>(line) {
        Ok(msg) => Some(msg),
        Err(_) => {
            let unknown_tag = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned));
            if let Some(tag) = unknown_tag {
                diag::append_tui(format!("unknown frame type: {tag}"));
            }
            None
        }
    }
}

/// Owns the backend's stdout until it dies. Parses each JSONL line into a
/// [`ServerMessage`] and forwards it to the actor; malformed lines are
/// silently ignored (a noisy backend must not kill the pipeline), unknown
/// frame types leave a diagnostics trace via [`parse_frame`].
fn spawn_reader(
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    event_tx: mpsc::Sender<ActorEvent>,
) {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut stdout = stdout;
        loop {
            let mut buf = String::new();
            match stdout.read_line(&mut buf).await {
                Ok(0) => {
                    let _ = event_tx
                        .send(ActorEvent::Died("backend closed stdout (EOF)".to_owned()))
                        .await;
                    return;
                }
                Ok(_) => {
                    let line = buf.trim_end_matches(['\n', '\r']);
                    if let Some(msg) = parse_frame(line) {
                        if event_tx.send(ActorEvent::Frame(msg)).await.is_err() {
                            // Actor gone (shutdown): nothing left to feed.
                            return;
                        }
                    }
                }
                Err(e) => {
                    let _ = event_tx.send(ActorEvent::Died(e.to_string())).await;
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reader hardening: a frame with an unknown `type` tag must not vanish
    /// silently — a `[tui]` diagnostics line names the tag, and the reader
    /// (parse_frame) keeps the loop alive by yielding `None` instead of an
    /// error. The global diag buffer is process state, so the assertion is
    /// delta-based (monotonic total) plus a marker scan, tolerant of
    /// parallel appends.
    #[test]
    fn unknown_frame_type_leaves_diag_trace_and_loop_continues() {
        let (_, before_total) = crate::diag::view();
        let line = r#"{"type":"holo_deck","request_id":"r1"}"#;

        assert!(
            parse_frame(line).is_none(),
            "unknown type must not become a frame"
        );
        assert!(
            parse_frame(r#"{"type":"result","request_id":"r2","content":"ok"}"#).is_some(),
            "known types still parse after an unknown one"
        );

        let (lines, after_total) = crate::diag::view();
        assert_eq!(
            after_total,
            before_total + 1,
            "exactly one diag line for the unknown type"
        );
        let marker = "unknown frame type: holo_deck";
        assert!(
            lines.iter().any(|l| l.contains(marker)),
            "diag line must name the unknown tag; got {lines:?}"
        );
    }

    /// A garbage line (no `type` at all) keeps the pre-hardening behavior:
    /// dropped silently, no diag noise.
    #[test]
    fn garbage_line_still_dropped_silently() {
        let (_, before_total) = crate::diag::view();
        assert!(parse_frame("not json at all").is_none());
        let (_, after_total) = crate::diag::view();
        assert_eq!(after_total, before_total, "no diag line for plain garbage");
    }
}
