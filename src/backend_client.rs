//! Backend process lifecycle for `chibi ide --stdio` (protocol v1).
//!
//! [`BackendClient`] owns the child process: it spawns the backend binary,
//! pipes JSONL frames over stdin/stdout, performs the mandatory
//! `initialize` → `ready` handshake and shuts the process down gracefully.
//!
//! Scope of this module (task 3a — process_lifecycle):
//! spawn / handshake / `send_raw` / `read_line` / graceful shutdown.
//!
//! Out of scope here — task 3b lives in [`crate::request_pipeline`]:
//! request_id correlation, cancel, reconnect, request-level timeouts.
//!
//! Errors are reported via [`BackendError`]. A non-zero child exit is only an
//! error where the protocol says so (handshake, shutdown): plain
//! `read_line`/`send_raw` surface pure I/O state so a future pipeline can
//! interpret it itself.
//!
//! Spec: `.project/ide_integration/architecture_specification.md` §4.
//! Fixtures: `tests/fixtures/ide_protocol/`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time;

use crate::diag;
use crate::protocol::{
    ClientCapabilities, ClientInfo, ClientMessage, ErrorCode, ProtocolVersion, ServerInfo,
    ServerMessage,
};

/// Client identification sent with the `initialize` frame.
pub const CLIENT_NAME: &str = "chibi-tui";

/// How long [`BackendClient::shutdown`] waits for the child to exit before
/// escalating to `kill`. Generous: the backend flushes state on exit.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Grace period for the child to actually die after its stdout hit EOF
/// (reaping races with pipe teardown).
const SPAWN_DEATH_GRACE: Duration = Duration::from_millis(500);

/// Everything that can go wrong while driving the backend process or its
/// request pipeline.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// Failed to spawn the backend binary (not found, permissions…).
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    /// Handshake failed before or during `initialize` → `ready`
    /// (malformed frame, unexpected frame kind, clean premature close…).
    #[error("handshake failed: {0}")]
    Handshake(String),

    /// The backend refused our protocol version
    /// (`error` with code `unsupported_protocol_version`).
    #[error("unsupported protocol version (server supports {server_protocol_version}): {message}")]
    UnsupportedProtocol {
        server_protocol_version: u64,
        message: String,
    },

    /// The backend answered with a global error other than
    /// `unsupported_protocol_version` during the handshake.
    #[error("backend rejected handshake ({code:?}): {message}")]
    HandshakeRejected { code: ErrorCode, message: String },

    /// Underlying stdin/stdout I/O failure.
    #[error("I/O error talking to backend: {0}")]
    Io(#[from] std::io::Error),

    /// The child process exited with a non-zero status where exit code 0 was
    /// required (handshake refusal per spec §10, unclean shutdown).
    #[error("backend exited unexpectedly with status {status}")]
    UnexpectedExit { status: ExitStatusDisplay },

    /// A `cancel` frame named a request that is no longer in flight (already
    /// answered or never issued).
    #[error("unknown request id: {request_id}")]
    UnknownRequest { request_id: String },

    /// The pipeline is broken (backend died) and must be
    /// re-established (see [`crate::RequestPipeline::reconnect`]) before further use.
    #[error("pipeline broken: {0}")]
    Broken(String),

    /// A request-scoped protocol error frame (`error` with a request_id)
    /// routed to that request.
    #[error("request failed ({code:?}): {message}")]
    RequestFailed {
        request_id: Option<String>,
        code: ErrorCode,
        message: String,
    },
}

/// `std::process::ExitStatus` has no stable `Display` across platforms, so we
/// normalize it to the raw exit code (`-9` after a kill, `-1` on signal death).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatusDisplay(pub i32);

impl From<std::process::ExitStatus> for ExitStatusDisplay {
    fn from(status: std::process::ExitStatus) -> Self {
        Self(status.code().unwrap_or(-1))
    }
}

impl std::fmt::Display for ExitStatusDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Owns the `chibi ide --stdio` child process and its JSONL pipes.
///
/// Construct via [`BackendClient::spawn`], then call
/// [`BackendClient::handshake`] before session traffic; end with
/// [`BackendClient::shutdown`] or [`BackendClient::kill`] — drop is the last
/// resort and kills the child (`kill_on_drop(true)`).
pub struct BackendClient {
    workspace_root: PathBuf,
    program: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl BackendClient {
    /// Spawn `chibi ide --stdio --workspace <root>` with piped stdio/stderr.
    ///
    /// `workspace_root` is stored for later `request` frames; this task only
    /// carries it. Stderr is drained by a background task so a chatty backend
    /// can never deadlock on a full pipe.
    ///
    /// The binary is resolved from `CHIBI_BACKEND_BIN` (override seam),
    /// falling back to `"chibi"` on `$PATH`.
    pub async fn spawn(workspace_root: impl AsRef<Path>) -> Result<Self, BackendError> {
        Self::spawn_command(workspace_root, &default_argv()).await
    }

    /// Shared spawn path. `argv` is either the production command line or
    /// (in tests) `/bin/sh -c <script>` / `python3 fake_backend.py`.
    async fn spawn_command(
        workspace_root: impl AsRef<Path>,
        argv: &[std::ffi::OsString],
    ) -> Result<Self, BackendError> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let (program, args) = argv.split_first().ok_or_else(|| BackendError::Spawn {
            program: String::new(),
            source: std::io::Error::other("empty argv"),
        })?;
        let program_display = program.to_string_lossy().to_string();

        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Never leave orphan processes behind if we are dropped in the middle of the protocol.
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| BackendError::Spawn {
            program: program_display.clone(),
            source,
        })?;

        let stdin = child.stdin.take().ok_or_else(|| BackendError::Spawn {
            program: program_display.clone(),
            source: std::io::Error::other("child stdin not captured"),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| BackendError::Spawn {
            program: program_display.clone(),
            source: std::io::Error::other("child stdout not captured"),
        })?;

        // feat_stderr_log_modal: stderr is pumped into the diagnostics ring buffer
        // (verbatim lines + optional CHIBI_TUI_LOG file mirror) instead of being
        // discarded. Still a fire-and-forget background task: a chatty backend
        // can never deadlock on a full stderr pipe, and the append only
        // holds the diag mutex for the instant of one push.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(capture_stderr(stderr));
        }
        diag::append_tui(format!(
            "spawn `{program_display}` (pid {})",
            child.id().unwrap_or(0)
        ));

        Ok(Self {
            workspace_root,
            program: program_display,
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Workspace root this client was spawned with.
    /// Test-only seam: spawn `/bin/sh -c <script>` as a deterministic stand-in
    /// backend so lifecycle behaviour can be exercised without a real
    /// `chibi` install.
    #[cfg(test)]
    async fn spawn_script(script: &str) -> Result<Self, BackendError> {
        Self::spawn_command(".", &["/bin/sh".into(), "-c".into(), script.into()]).await
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Program (binary name or path) backing this client.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Mandatory first exchange: send `initialize` (protocol v1), wait for
    /// `ready`.
    ///
    /// Mapping per spec §4/§10:
    /// - `ready` → success;
    /// - `error {code: unsupported_protocol_version}` → [`BackendError::UnsupportedProtocol`];
    /// - any other global error → [`BackendError::HandshakeRejected`];
    /// - premature exit: non-zero status → [`BackendError::UnexpectedExit`],
    ///   zero status → [`BackendError::Handshake`];
    /// - any other frame kind → [`BackendError::Handshake`].
    ///
    /// feat_stderr_log_modal: the outcome is stamped into the diagnostics
    /// stream as a `[tui] handshake ok/failed` lifecycle event.
    pub async fn handshake(&mut self) -> Result<Ready, BackendError> {
        let result = self.handshake_inner().await;
        match &result {
            Ok(ready) => {
                let server = ready
                    .server
                    .as_ref()
                    .map(|s| format!(" (server {} {})", s.name, s.version))
                    .unwrap_or_default();
                diag::append_tui(format!("handshake ok (protocol v1){server}"));
            }
            Err(e) => diag::append_tui(format!("handshake failed: {e}")),
        }
        result
    }

    /// The handshake proper (logging wrapper above).
    async fn handshake_inner(&mut self) -> Result<Ready, BackendError> {
        let initialize = ClientMessage::Initialize {
            protocol_version: ProtocolVersion::CURRENT,
            client: Some(ClientInfo {
                name: CLIENT_NAME.to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            }),
            capabilities: Some(ClientCapabilities {
                thoughts: true,
                subagents: true,
            }),
        };
        let line = serde_json::to_string(&initialize)
            .map_err(|e| BackendError::Handshake(format!("cannot serialize initialize: {e}")))?;
        self.send_raw(&line).await?;

        let raw = match self.read_line().await? {
            Some(line) => line,
            None => {
                // Stdout closed: distinguish "died with a refusal"
                // (non-zero exit) from a clean premature close. The child may
                // not be reaped at EOF yet, so we give it a short grace period.
                match time::timeout(SPAWN_DEATH_GRACE, self.child.wait()).await {
                    Ok(Ok(status)) if !status.success() => {
                        return Err(BackendError::UnexpectedExit {
                            status: status.into(),
                        });
                    }
                    _ => {}
                }
                return Err(BackendError::Handshake(
                    "backend closed stdout during handshake".to_owned(),
                ));
            }
        };

        // Stdout carries protocol frames only when --stdio mode is up;
        // anything else is a handshake failure by definition.
        let msg: ServerMessage = serde_json::from_str(&raw).map_err(|e| {
            BackendError::Handshake(format!("malformed frame during handshake: {e}"))
        })?;

        match msg {
            ServerMessage::Ready {
                protocol_version,
                server,
                capabilities,
            } => Ok(Ready {
                protocol_version,
                server,
                capabilities,
            }),
            ServerMessage::Error {
                code,
                server_protocol_version,
                message,
                ..
            } => match code {
                ErrorCode::UnsupportedProtocolVersion => Err(BackendError::UnsupportedProtocol {
                    server_protocol_version: server_protocol_version.unwrap_or(1),
                    message,
                }),
                other => {
                    self.kill().await;
                    Err(BackendError::HandshakeRejected {
                        code: other,
                        message,
                    })
                }
            },
            other => Err(BackendError::Handshake(format!(
                "unexpected frame during handshake: {other:?}"
            ))),
        }
    }

    /// Write one JSONL frame to the child's stdin followed by `\n`, then flush.
    ///
    /// `line` must be a single-line JSON document (caller's responsibility:
    /// protocol frames are one object per line). A closed stdin surfaces as
    /// [`BackendError::Io`] (`BrokenPipe`).
    pub async fn send_raw(&mut self, line: &str) -> Result<(), BackendError> {
        assert!(!line.contains('\n'), "protocol frames are single-line");
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read the next JSONL line from the child's stdout.
    ///
    /// Returns `Ok(None)` on clean EOF (backend closed stdout). Partial data
    /// without a trailing newline at EOF is returned as the final line,
    /// matching [`AsyncBufReadExt::read_line`] semantics.
    pub async fn read_line(&mut self) -> Result<Option<String>, BackendError> {
        let mut buf = String::new();
        let n = self.stdout.read_line(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(buf.trim_end_matches(['\n', '\r']).to_owned()))
    }

    /// Graceful shutdown: send the `shutdown` frame and wait up to
    /// [`SHUTDOWN_TIMEOUT`] for exit code 0. On timeout the child is killed
    /// and [`BackendError::UnexpectedExit`] is returned.
    ///
    /// A broken pipe while writing the frame means the child already died;
    /// we then skip straight to reaping and report its exit status.
    pub async fn shutdown(&mut self) -> Result<(), BackendError> {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT).await
    }

    /// [`BackendClient::shutdown`] with an explicit wait budget (used by tests
    /// to keep the timeout-kill scenario fast).
    pub async fn shutdown_with_timeout(&mut self, timeout: Duration) -> Result<(), BackendError> {
        let shutdown_line = serde_json::to_string(&ClientMessage::Shutdown {})
            .map_err(|e| BackendError::Io(std::io::Error::other(e)))?;

        // Best effort only: the real verdict comes from the exit status below.
        if let Err(err) = self.send_raw(&shutdown_line).await {
            let broken = matches!(&err,
                BackendError::Io(e) if e.kind() == std::io::ErrorKind::BrokenPipe);
            if !broken {
                return Err(err);
            }
        }

        match time::timeout(timeout, self.child.wait()).await {
            Err(_elapsed) => {
                // Timed out: escalate to kill so that nothing can hang forever.
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
                Err(BackendError::UnexpectedExit {
                    status: ExitStatusDisplay(-9),
                })
            }
            Ok(Err(e)) => Err(e.into()),
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(BackendError::UnexpectedExit {
                status: status.into(),
            }),
        }
    }

    /// Kill the child immediately (no grace period). Used internally on
    /// unrecoverable protocol violations so no orphan survives the session.
    pub async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Split the client into its pipe halves plus a reaper handle.
    ///
    /// The child keeps running: [`ReapHandle`] is the only way to stop it
    /// afterwards. This is the hand-off point for the reader actor in
    /// [`crate::request_pipeline`], which owns both directions of the JSONL
    /// pipe while exit control stays with the pipeline.
    pub(crate) fn into_parts(
        self,
    ) -> (
        Option<ChildStdin>,
        Option<BufReader<ChildStdout>>,
        ReapHandle,
    ) {
        // IMPORTANT: `stdin`/`stdout` were already moved OUT of `child` in
        // `spawn_command`; `child.stdin.take()` here would silently yield
        // `None`. Hand over the REAL pipes here. The `BufReader` moves intact, so
        // bytes buffered during the handshake (e.g. an early garbage line)
        // are preserved for the reader of the pipeline.
        let Self {
            child,
            stdin,
            stdout,
            ..
        } = self;
        (Some(stdin), Some(stdout), ReapHandle { child })
    }
}

/// Controlled spawn seam: run an exact argv instead of the production command
/// line. The request pipeline (task 3b) drives `python3 tests/fake_backend.py`
/// through it; the integration tests rely on the same path.
pub(crate) async fn spawn_argv(
    workspace_root: &str,
    argv: Vec<std::ffi::OsString>,
) -> Result<BackendClient, BackendError> {
    BackendClient::spawn_command(workspace_root, &argv).await
}

// ---------------------------------------------------------------------------
// feat_stderr_log_modal: stderr → diagnostics ring buffer
// ---------------------------------------------------------------------------

/// Pump the backend's stderr into the diagnostics log, line by line.
///
/// Reads run in fixed-size chunks (stderr lines may be partial/chunky —
/// a single `read` never corresponds to a line). Complete lines are appended
/// verbatim as they complete; a trailing partial line with no newline is
/// flushed as one final line at EOF. The task never fails and never touches
/// the protocol path: it holds the diag mutex only for the instant of one
/// push, so it can never block the event loop or request handling.
async fn capture_stderr<R>(mut stderr: R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                for line in drain_complete_lines(&mut pending) {
                    diag::append(line);
                }
            }
        }
    }
    // EOF: a partial line without its newline is still a line (flush as the
    // final entry, matching the AsyncBufReadExt::read_line semantics).
    if !pending.is_empty() {
        diag::append(clean_line(&pending));
    }
}

/// Split and remove all complete (`\n`-terminated) lines out of `pending`,
/// returning them newline-stripped. Incomplete trailing bytes stay buffered
/// for the next chunk. Pure (sync) so the splitting rules are unit-testable
/// without a pipe.
fn drain_complete_lines(pending: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = pending.drain(..=pos).collect();
        lines.push(clean_line(&line));
    }
    lines
}

/// Decode one raw line: lossy UTF-8 (a broken byte must not kill the stream),
/// trailing `\n` and `\r` stripped (CRLF piping noise). Empty lines stay
/// verbatim — the buffer is a faithful mirror of what the backend printed.
fn clean_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_owned()
}

/// Ownership of the still-running child process after
/// [`BackendClient::into_parts`]; the only remaining way to kill/reap it.
pub(crate) struct ReapHandle {
    child: Child,
}

impl ReapHandle {
    /// Kill the child immediately and reap it. Idempotent.
    pub(crate) async fn kill(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Wait up to `budget` for the child to exit on its own; on timeout,
    /// escalate to SIGKILL and report status `-9`. Otherwise return the real
    /// exit status for the caller to check against "exit 0 expected".
    pub(crate) async fn graceful_reap(
        &mut self,
        budget: Duration,
    ) -> Result<std::process::ExitStatus, BackendError> {
        match time::timeout(budget, self.child.wait()).await {
            Err(_elapsed) => {
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
                Err(BackendError::UnexpectedExit {
                    status: ExitStatusDisplay(-9),
                })
            }
            Ok(wait_result) => wait_result.map_err(BackendError::from),
        }
    }
}

/// Protocol result of a successful handshake.
#[derive(Clone, Debug)]
pub struct Ready {
    pub protocol_version: ProtocolVersion,
    pub server: Option<ServerInfo>,
    pub capabilities: Option<crate::protocol::Capabilities>,
}

impl Ready {
    /// Slash-command set advertised by the backend (empty when absent).
    pub fn commands(&self) -> &[String] {
        static EMPTY: [String; 0] = [];
        self.capabilities
            .as_ref()
            .map(|c| c.commands.as_slice())
            .unwrap_or(&EMPTY)
    }
}

/// Production spawn argv: `<program> ide --stdio --workspace <root>`, where
/// `<program>` comes from `CHIBI_BACKEND_BIN` (fallback `"chibi"`).
fn default_argv() -> Vec<std::ffi::OsString> {
    vec![
        backend_program().into(),
        "ide".into(),
        "--stdio".into(),
        "--workspace".into(),
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .into_os_string(),
    ]
}

/// Default backend binary: overridable via `CHIBI_BACKEND_BIN`.
fn backend_program() -> String {
    std::env::var("CHIBI_BACKEND_BIN").unwrap_or_else(|_| "chibi".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- spawn ---------------------------------------------------------------

    #[tokio::test]
    async fn spawn_starts_process_with_pipes() {
        let mut client = BackendClient::spawn_script("read line")
            .await
            .expect("spawn");
        assert_eq!(client.program(), "/bin/sh");
        client.kill().await;
    }

    #[tokio::test]
    async fn spawn_missing_binary_reports_spawn_error() {
        let res = BackendClient::spawn_command(
            ".",
            &[
                "/nonexistent/chibi-binary-xyz".into(),
                "ide".into(),
                "--stdio".into(),
            ],
        )
        .await;
        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("spawn of a missing binary must fail"),
        };
        assert!(matches!(err, BackendError::Spawn { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn existing_but_failing_binary_is_not_a_spawn_error() {
        // A shell that exits 1 right away execs fine: the failure surfaces
        // as an exit status, never as `Spawn`. The setup screen keys off
        // `Spawn` only, so it stays away from this case.
        let mut client = match BackendClient::spawn_script("exit 1").await {
            Ok(c) => c,
            Err(e) => panic!("`sh -c 'exit 1'` must spawn, got: {e:?}"),
        };
        let err = match client.handshake().await {
            Err(e) => e,
            Ok(_) => panic!("`sh -c 'exit 1'` must fail the handshake"),
        };
        assert!(!matches!(err, BackendError::Spawn { .. }), "got: {err:?}");
    }

    // --- handshake -----------------------------------------------------------

    #[tokio::test]
    async fn handshake_success_against_mock() {
        // Mock echoes a fixture-shaped `ready` after reading any line.
        let script = r#"
read line
echo '{"type":"ready","protocol_version":1,"server":{"name":"chibi","version":"1.0.0"},"capabilities":{"commands":["/help"]}}'
"#;
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        let ready = client.handshake().await.expect("handshake must pass");
        assert_eq!(ready.protocol_version, ProtocolVersion::CURRENT);
        assert_eq!(ready.server.as_ref().unwrap().name, "chibi");
        assert_eq!(ready.commands(), ["/help".to_owned()]);
        client.kill().await;
    }

    #[tokio::test]
    async fn handshake_unsupported_protocol_version() {
        // Mock answers with the canonical refusal frame from invalid_cases.jsonl
        // and exits non-zero, exactly like the real backend.
        let script = r#"
read line
echo '{"type":"error","request_id":null,"code":"unsupported_protocol_version","server_protocol_version":1,"message":"Unsupported protocol version: 99."}'
exit 42
"#;
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        let err = match client.handshake().await {
            Err(e) => e,
            Ok(_) => panic!("handshake must fail"),
        };
        match err {
            BackendError::UnsupportedProtocol {
                server_protocol_version,
                ..
            } => assert_eq!(server_protocol_version, 1),
            other => panic!("expected UnsupportedProtocol, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_nonzero_exit_without_answer_is_unexpected_exit() {
        let script = "read line; exit 7";
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        let err = client.handshake().await.expect_err("must fail");
        assert!(
            matches!(err, BackendError::UnexpectedExit { .. }),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn handshake_malformed_frame_fails_cleanly() {
        let script = r#"
read line
echo 'this is not json'
"#;
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        let err = client.handshake().await.expect_err("must fail");
        assert!(matches!(err, BackendError::Handshake(_)), "got: {err:?}");
        client.kill().await;
    }

    // --- send/read framing -----------------------------------------------------

    #[tokio::test]
    async fn send_raw_and_read_line_roundtrip_multiple_frames() {
        // Mock echoes every line back prefixed so framing is observable.
        let script = r#"
while read line; do
  echo "ack:$line"
done
"#;
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        client.send_raw("frame-one").await.unwrap();
        client.send_raw("frame-two").await.unwrap();

        assert_eq!(
            client.read_line().await.unwrap().as_deref(),
            Some("ack:frame-one")
        );
        assert_eq!(
            client.read_line().await.unwrap().as_deref(),
            Some("ack:frame-two")
        );

        // EOF follows once the mock's stdin closes; `client` drops here and
        // kill_on_drop reaps the child.
    }

    #[tokio::test]
    async fn read_line_returns_none_on_clean_eof() {
        // Mock prints nothing and exits as soon as its stdin closes.
        let script = "exit 0";
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        // Give the mock a beat to exit, then read: pipe is open but writer is
        // gone -> clean EOF -> None.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(client.read_line().await.unwrap(), None);
        // `client` drops here; kill_on_drop reaps the exited child.
    }

    // --- shutdown ------------------------------------------------------------

    #[tokio::test]
    async fn shutdown_graceful_exit_zero() {
        // Mock waits for one line then exits 0, like the real backend on
        // a `shutdown` frame.
        let script = "read line; exit 0";
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        client.shutdown().await.expect("graceful shutdown");
    }

    #[tokio::test]
    async fn shutdown_timeout_kills_stubborn_child() {
        // Mock ignores stdin entirely and never exits on its own.
        let script = r#"
read line
while true; do sleep 1; done
"#;
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        let err = client
            .shutdown_with_timeout(Duration::from_millis(300))
            .await
            .expect_err("stubborn child must be killed and reported");
        assert!(
            matches!(err, BackendError::UnexpectedExit { status } if status.0 == -9),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_after_child_death_surfaces_exit_status() {
        // Mock exits 3 on the first line: broken-pipe path + unclean status.
        let script = "read line; exit 3";
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        let err = client.shutdown().await.expect_err("exit code 3 is unclean");
        assert!(
            matches!(err, BackendError::UnexpectedExit { status } if status.0 == 3),
            "got: {err:?}"
        );
    }

    // ---- feat_stderr_log_modal: stderr capture ------------------------------

    // Pure splitting rules (no pipe needed).

    #[test]
    fn drain_complete_lines_splits_and_keeps_partial() {
        let mut buf = b"one\ntwo\nthr".to_vec();
        let lines = drain_complete_lines(&mut buf);
        assert_eq!(lines, ["one".to_owned(), "two".to_owned()]);
        assert_eq!(buf, b"thr", "partial line stays buffered");

        // The remainder completes on the NEXT chunk (chunky reads):
        buf.extend_from_slice(b"ee\nfour");
        let lines = drain_complete_lines(&mut buf);
        assert_eq!(lines, ["three".to_owned()]);
        assert_eq!(buf, b"four");
    }

    #[test]
    fn clean_line_strips_crlf_and_handles_empty_and_bad_utf8() {
        assert_eq!(clean_line(b"abc\r\n"), "abc");
        assert_eq!(clean_line(b"abc\n"), "abc");
        assert_eq!(clean_line(b"abc"), "abc");
        assert_eq!(clean_line(b"\r\n"), "", "empty lines stay verbatim");
        assert_eq!(clean_line(&[0xff, b'a']), "\u{fffd}a", "lossy UTF-8");
    }

    /// End-to-end through a real spawned process: stderr lands in the diag
    /// ring verbatim (complete line + EOF-flushed partial) and the `[tui]`
    /// spawn lifecycle event is stamped. Assertions are CONTAINS-based — the
    /// global buffer is process state shared with other concurrent tests.
    #[tokio::test]
    async fn stderr_lines_land_in_diag_ring_buffer() {
        let marker = format!(
            "diag-marker-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        // The script writes to stderr, then BLOCKS on `read` so the child
        // cannot die (and close the pipe) before its writes land.
        let script =
            format!("echo out-stdout; echo '{marker}' >&2; printf 'partial-tail' >&2; read line");
        let mut client = BackendClient::spawn_script(&script).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await; // writes settle
        client.kill().await; // stderr EOF → capture task flushes and exits
                             // Give the background capture task a beat to drain and append.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let snap = diag::snapshot();
        assert!(
            snap.iter().any(|l| l.text == *marker),
            "stderr line captured verbatim"
        );
        assert!(
            snap.iter().any(|l| l.text == "partial-tail"),
            "trailing partial line flushed as a final line at EOF"
        );
        assert!(
            snap.iter()
                .any(|l| l.text.starts_with("[tui] spawn `/bin/sh`")),
            "spawn lifecycle event stamped with the [tui] prefix"
        );
    }

    #[tokio::test]
    async fn handshake_outcome_lands_in_diag_stream() {
        // Success path.
        let script = r#"
read line
echo '{"type":"ready","protocol_version":1,"server":{"name":"chibi","version":"1.0.0"}}'
"#;
        let mut client = BackendClient::spawn_script(script).await.unwrap();
        client.handshake().await.expect("handshake ok");
        client.kill().await;
        let snap = diag::snapshot();
        assert!(
            snap.iter()
                .any(|l| l.text.starts_with("[tui] handshake ok (protocol v1)")),
            "ok handshake stamped: {:?}",
            snap.last()
        );

        // Failure path.
        let mut client = BackendClient::spawn_script("read line; exit 7")
            .await
            .unwrap();
        assert!(client.handshake().await.is_err());
        let snap = diag::snapshot();
        assert!(
            snap.iter()
                .any(|l| l.text.starts_with("[tui] handshake failed")),
            "failed handshake stamped"
        );
    }
}
