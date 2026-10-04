//! Backend installer engine for the missing-backend onboarding flow.
//!
//! Discovers a usable Python interpreter (>=3.11, <4) **by execution** —
//! never by binary name, since stock macOS `/usr/bin/python3` is 3.9.6 —
//! detects pipx by execution (pyenv shims without backing installs fail),
//! picks a rung from the owner-approved installer ladder, and runs the
//! install with streamed, piped output. Installer children are always piped:
//! inheriting the TUI's stdout would corrupt the ratatui canvas and, in live
//! mode, the protocol channel.
//!
//! All probes are time-limited. The pure decision logic (rung ladder,
//! version gate, candidate enumeration) is separated from execution so it
//! can be unit-tested offline with a mock [`ProbeExec`] — no network, no
//! real interpreters.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::setup_screen::TargetOs;

/// Installer ladder rung, in authoritative order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rung {
    /// `pipx install --python <probed> chibi-bot`
    Pipx,
    /// TUI-owned venv under the storage root + pip inside.
    Venv,
    /// `pip install --user` (only when the interpreter is not
    /// externally-managed, e.g. pyenv).
    PipUser,
    /// `pip install --user --break-system-packages` — LAST resort, explicit
    /// warning, never silent.
    BreakSystemPackages,
}

impl Rung {
    pub fn label(self) -> &'static str {
        match self {
            Rung::Pipx => "pipx",
            Rung::Venv => "TUI venv",
            Rung::PipUser => "pip --user",
            Rung::BreakSystemPackages => "pip --user --break-system-packages",
        }
    }
}

/// Package installed by every rung of the ladder. `chibi` on PyPI is an
/// unrelated package; only `chibi-bot` provides the console script.
pub const PACKAGE: &str = "chibi-bot";

/// Minimum required minor version of Python 3 (backend `requires_python`).
pub const MIN_PYTHON_MINOR: u32 = 11;

/// Default timeout for a single probe command.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of one probe execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeOutput {
    pub success: bool,
    pub stdout: String,
}

/// Injectable probe seam: run a program with args, return its result, or
/// `None` on timeout/spawn failure. Real implementation shells out via
/// tokio; tests substitute canned responses.
pub trait ProbeExec: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> BoxFuture<'static, Option<ProbeOutput>>;
}

/// Real executor: tokio child with piped output and a hard timeout.
pub struct SysProbe {
    pub timeout: Duration,
}

impl Default for SysProbe {
    fn default() -> Self {
        Self {
            timeout: PROBE_TIMEOUT,
        }
    }
}

impl ProbeExec for SysProbe {
    fn run(&self, program: &str, args: &[String]) -> BoxFuture<'static, Option<ProbeOutput>> {
        let program = program.to_owned();
        let args = args.to_vec();
        let timeout = self.timeout;
        Box::pin(async move {
            let mut cmd = Command::new(&program);
            cmd.args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let fut = async {
                let out = cmd.output().await.ok()?;
                Some(ProbeOutput {
                    success: out.status.success(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                })
            };
            tokio::time::timeout(timeout, fut).await.unwrap_or_default()
        })
    }
}

// ---------------------------------------------------------------------------
// Python discovery
// ---------------------------------------------------------------------------

/// A Python interpreter that passed the version gate by execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PythonInfo {
    /// Program to invoke (full candidate path, or `py` for the launcher).
    pub program: String,
    /// Extra fixed args before user args (e.g. `["-3"]` for the `py` launcher).
    pub prefix_args: Vec<String>,
    /// Probed version string, e.g. "3.12.13".
    pub version: String,
}

/// A discovery candidate: a concrete interpreter path, or the Windows `py` launcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Candidate {
    Direct(PathBuf),
    PyLauncher,
}

/// Versioned names first, then the generic name. A bare `python3` may resolve
/// to a too-old interpreter, so versioned names get priority inside each dir.
pub fn python_names(os: TargetOs) -> Vec<String> {
    let mut names = vec![
        "python3.14",
        "python3.13",
        "python3.12",
        "python3.11",
        "python3",
    ];
    if os == TargetOs::Windows {
        names.push("python");
    }
    names
        .into_iter()
        .map(|n| {
            if os == TargetOs::Windows && !n.ends_with(".exe") {
                format!("{n}.exe")
            } else {
                n.to_owned()
            }
        })
        .collect()
}

/// Search dirs for interpreter candidates: PATH entries in order, plus
/// well-known absolute locations as a suffix (GUI-launched TUIs on macOS get
/// a minimal PATH where Homebrew/pyenv/user installs are invisible).
pub fn search_dirs(os: TargetOs, path_var: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match path_var {
        Some(p) => std::env::split_paths(p)
            .filter(|d| !d.as_os_str().is_empty())
            .collect(),
        None => Vec::new(),
    };
    if os == TargetOs::Unix {
        for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
            let p = PathBuf::from(extra);
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            for extra in ["~/.local/bin", "~/.pyenv/shims"] {
                let expanded =
                    std::path::PathBuf::from(home.clone()).join(extra.trim_start_matches("~/"));
                if !dirs.contains(&expanded) {
                    dirs.push(expanded);
                }
            }
        }
    }
    dirs
}

/// Candidate enumeration: inside each dir try versioned names first, then the
/// generic name; the Windows `py` launcher is appended last.
pub fn python_candidates(os: TargetOs, path_var: Option<&std::ffi::OsStr>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for dir in search_dirs(os, path_var) {
        for name in python_names(os) {
            let full = dir.join(&name);
            if let Some(parent) = full.parent() {
                let _ = parent;
            }
            out.push(Candidate::Direct(full));
        }
    }
    if os == TargetOs::Windows {
        out.push(Candidate::PyLauncher);
    }
    out
}

/// Version gate: major 3, minor >= 11 (mirrors backend `requires_python`).
pub fn version_ok(version: &str) -> bool {
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let minor = parts.next().and_then(|p| p.parse::<u32>().ok());
    match (major, minor) {
        (Some(3), Some(minor)) => minor >= MIN_PYTHON_MINOR,
        _ => false,
    }
}

const VERSION_CODE: &str = "import sys;print('%d.%d.%d' % sys.version_info[:3])";
const EXTERNALLY_MANAGED_CODE: &str = "import os,sysconfig;print(os.path.exists(os.path.join(sysconfig.get_path('stdlib'),'EXTERNALLY-MANAGED')))";

/// Probe one candidate interpreter by execution and apply the version gate.
pub async fn probe_python(exec: &dyn ProbeExec, candidate: &Candidate) -> Option<PythonInfo> {
    let (program, prefix_args) = match candidate {
        Candidate::Direct(p) => (p.to_string_lossy().into_owned(), Vec::new()),
        Candidate::PyLauncher => ("py".to_owned(), vec!["-3".to_owned()]),
    };
    let mut args = prefix_args.clone();
    args.push("-c".to_owned());
    args.push(VERSION_CODE.to_owned());
    let out = exec.run(&program, &args).await?;
    if !out.success {
        return None;
    }
    let version = out.stdout.trim().to_owned();
    if !version_ok(&version) {
        return None;
    }
    Some(PythonInfo {
        program,
        prefix_args,
        version,
    })
}

// ---------------------------------------------------------------------------
// Facts + ladder decision
// ---------------------------------------------------------------------------

/// Probed environment facts driving the ladder decision.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub python: Option<PythonInfo>,
    pub pipx_ok: bool,
    pub pipx_bin_dir: Option<PathBuf>,
    pub externally_managed: bool,
}

/// Full discovery: interpreter probe, pipx probe (by execution — a pyenv shim
/// without a backing install fails here), PEP 668 marker probe.
pub async fn discover(exec: &dyn ProbeExec, os: TargetOs) -> Facts {
    let mut facts = Facts::default();

    for candidate in python_candidates(os, std::env::var_os("PATH").as_deref()) {
        if let Some(py) = probe_python(exec, &candidate).await {
            facts.python = Some(py);
            break;
        }
    }

    if let Some(out) = exec.run("pipx", &["--version".to_owned()]).await {
        facts.pipx_ok = out.success;
        if out.success {
            if let Some(bin) = exec
                .run(
                    "pipx",
                    &[
                        "environment".to_owned(),
                        "--value".to_owned(),
                        "PIPX_BIN_DIR".to_owned(),
                    ],
                )
                .await
            {
                if bin.success {
                    let dir = bin.stdout.trim();
                    if !dir.is_empty() {
                        facts.pipx_bin_dir = Some(PathBuf::from(dir));
                    }
                }
            }
        }
    }

    if let Some(py) = &facts.python {
        let mut args = py.prefix_args.clone();
        args.push("-c".to_owned());
        args.push(EXTERNALLY_MANAGED_CODE.to_owned());
        if let Some(out) = exec.run(&py.program, &args).await {
            facts.externally_managed = out.success && out.stdout.trim() == "True";
        }
    }

    facts
}

/// Installer ladder, authoritative order. Applicability per rung:
/// 1. pipx — requires a working pipx probe
/// 2. TUI-owned venv — always available once a python exists
/// 3. `pip --user` — ONLY when the interpreter is not externally managed
/// 4. `--break-system-packages` — last resort, never silent
pub fn applicable_rungs(facts: &Facts) -> Vec<Rung> {
    if facts.python.is_none() {
        return Vec::new();
    }
    let mut rungs = Vec::new();
    if facts.pipx_ok {
        rungs.push(Rung::Pipx);
    }
    rungs.push(Rung::Venv);
    if !facts.externally_managed {
        rungs.push(Rung::PipUser);
    }
    rungs.push(Rung::BreakSystemPackages);
    rungs
}

/// First rung to offer to the user.
pub fn first_applicable_rung(facts: &Facts) -> Option<Rung> {
    applicable_rungs(facts).first().copied()
}

/// Next applicable rung after a failure, skipping inapplicable ones.
pub fn next_applicable_rung(current: Rung, facts: &Facts) -> Option<Rung> {
    let rungs = applicable_rungs(facts);
    let idx = rungs.iter().position(|r| *r == current)?;
    rungs.into_iter().nth(idx + 1)
}

// ---------------------------------------------------------------------------
// Installer commands + binary resolution
// ---------------------------------------------------------------------------

/// One install command to execute and stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// Venv location: TUI-owned directory under the storage root.
pub fn venv_dir_for(root: &Path) -> PathBuf {
    root.join("venv")
}

fn venv_python(venv_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        venv_dir.join("Scripts").join("python.exe")
    } else {
        venv_dir.join("bin").join("python")
    }
}

/// Build the commands for a rung. Package name is always `chibi-bot`.
pub fn rung_commands(rung: Rung, python: &PythonInfo, venv_dir: &Path) -> Vec<CommandSpec> {
    match rung {
        Rung::Pipx => vec![CommandSpec {
            program: "pipx".to_owned(),
            args: vec![
                "install".to_owned(),
                "--python".to_owned(),
                python.program.clone(),
                PACKAGE.to_owned(),
            ],
        }],
        Rung::Venv => vec![
            CommandSpec {
                program: python.program.clone(),
                args: {
                    let mut a = python.prefix_args.clone();
                    a.extend([
                        "-m".to_owned(),
                        "venv".to_owned(),
                        venv_dir.display().to_string(),
                    ]);
                    a
                },
            },
            CommandSpec {
                program: venv_python(venv_dir).display().to_string(),
                args: vec![
                    "-m".to_owned(),
                    "pip".to_owned(),
                    "install".to_owned(),
                    PACKAGE.to_owned(),
                ],
            },
        ],
        Rung::PipUser => vec![CommandSpec {
            program: python.program.clone(),
            args: {
                let mut a = python.prefix_args.clone();
                a.extend([
                    "-m".to_owned(),
                    "pip".to_owned(),
                    "install".to_owned(),
                    "--user".to_owned(),
                    PACKAGE.to_owned(),
                ]);
                a
            },
        }],
        Rung::BreakSystemPackages => vec![CommandSpec {
            program: python.program.clone(),
            args: {
                let mut a = python.prefix_args.clone();
                a.extend([
                    "-m".to_owned(),
                    "pip".to_owned(),
                    "install".to_owned(),
                    "--user".to_owned(),
                    "--break-system-packages".to_owned(),
                    PACKAGE.to_owned(),
                ]);
                a
            },
        }],
    }
}

/// Resolve the installed `chibi` binary from the interpreter/pipx at runtime.
/// Never hardcodes bin-dir literals beyond the venv fallback.
pub async fn resolve_installed_binary(
    exec: &dyn ProbeExec,
    rung: Rung,
    facts: &Facts,
    venv_dir: &Path,
) -> Option<PathBuf> {
    let binary_name = if cfg!(windows) { "chibi.exe" } else { "chibi" };
    match rung {
        Rung::Pipx => {
            let dir = facts.pipx_bin_dir.clone().or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin"))
            })?;
            Some(dir.join(binary_name))
        }
        Rung::Venv => {
            let vp = venv_python(venv_dir);
            let args = vec!["-c".to_owned(), scripts_code(false)];
            if let Some(out) = exec.run(&vp.display().to_string(), &args).await {
                if out.success {
                    let dir = PathBuf::from(out.stdout.trim());
                    if dir.is_absolute() {
                        return Some(dir.join(binary_name));
                    }
                }
            }
            Some(if cfg!(windows) {
                venv_dir.join("Scripts").join(binary_name)
            } else {
                venv_dir.join("bin").join(binary_name)
            })
        }
        Rung::PipUser | Rung::BreakSystemPackages => {
            let py = facts.python.as_ref()?;
            let mut args = py.prefix_args.clone();
            args.push("-c".to_owned());
            args.push(scripts_code(true));
            if let Some(out) = exec.run(&py.program, &args).await {
                if out.success {
                    let dir = PathBuf::from(out.stdout.trim());
                    if dir.is_absolute() {
                        return Some(dir.join(binary_name));
                    }
                }
            }
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".local/bin"))
                .map(|d| d.join(binary_name))
        }
    }
}

fn scripts_code(posix_user: bool) -> String {
    if posix_user {
        "import sysconfig;print(sysconfig.get_path('scripts','posix_user'))".to_owned()
    } else {
        "import sysconfig;print(sysconfig.get_path('scripts'))".to_owned()
    }
}

/// Success criterion: the resolved binary runs `stdio --help` and exits 0.
/// Exit code of the install child alone is never sufficient.
pub async fn verify_binary(exec: &dyn ProbeExec, binary: &Path) -> bool {
    match exec
        .run(
            &binary.display().to_string(),
            &["stdio".to_owned(), "--help".to_owned()],
        )
        .await
    {
        Some(out) => out.success,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Streaming install runner
// ---------------------------------------------------------------------------

/// Bounded tail of streamed installer output (ANSI-stripped, CR-split).
#[derive(Debug)]
pub struct LogTail {
    lines: VecDeque<String>,
    cap: usize,
}

impl LogTail {
    pub fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, raw: &str) {
        for line in raw.replace('\r', "\n").split('\n') {
            let clean = strip_ansi(line).trim().to_owned();
            if clean.is_empty() {
                continue;
            }
            if self.lines.len() == self.cap {
                self.lines.pop_front();
            }
            self.lines.push_back(clean);
        }
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        self.lines.iter().rev().take(n).rev().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Strip ANSI escape sequences (CSI: ESC [ params letter) for readable tail.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Outcome of a full install attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Binary resolved and verified via `stdio --help` exit 0.
    Installed { binary: PathBuf, bin_dir: PathBuf },
    /// A command failed (or could not spawn). Includes last exit code.
    Failed {
        rung: Rung,
        reason: String,
        code: Option<i32>,
    },
    /// User cancelled (Esc).
    Cancelled,
}

/// Handle over a running install: shared log tail, cancel flag, completion.
pub struct InstallHandle {
    pub log: Arc<Mutex<LogTail>>,
    cancel_flag: Arc<AtomicBool>,
    done: oneshot::Receiver<InstallOutcome>,
}

impl InstallHandle {
    /// Request cancellation. The runner kills the child within one tick.
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    /// Await completion (call from a select arm with `&mut`).
    pub async fn finished(&mut self) -> InstallOutcome {
        match (&mut self.done).await {
            Ok(outcome) => outcome,
            Err(_) => InstallOutcome::Failed {
                rung: Rung::Venv,
                reason: "installer task ended unexpectedly".to_owned(),
                code: None,
            },
        }
    }
}

/// Spawn the installer for a rung as a detached task that streams output
/// into the shared log tail and reports one final outcome. Children are
/// always piped — never inheriting the TUI's stdio.
pub fn spawn_install(facts: &Facts, rung: Rung, venv_dir: PathBuf) -> InstallHandle {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let log = Arc::new(Mutex::new(LogTail::new(400)));
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let python = facts
        .python
        .clone()
        .expect("spawn_install requires a probed python");
    let em = facts.externally_managed;
    let pipx_bin_dir = facts.pipx_bin_dir.clone();

    let log_task = Arc::clone(&log);
    let cancel_task = Arc::clone(&cancel_flag);

    tokio::spawn(async move {
        let exec = SysProbe::default();
        let owned_facts = Facts {
            python: Some(python),
            pipx_ok: true,
            pipx_bin_dir,
            externally_managed: em,
        };
        let outcome = run_rung(
            &exec,
            rung,
            owned_facts.python.as_ref().expect("python present"),
            &venv_dir,
            &owned_facts,
            log_task,
            cancel_task,
        )
        .await;
        let _ = done_tx.send(outcome);
    });

    InstallHandle {
        log,
        cancel_flag,
        done: done_rx,
    }
}

async fn run_rung(
    exec: &SysProbe,
    rung: Rung,
    python: &PythonInfo,
    venv_dir: &Path,
    facts: &Facts,
    log: Arc<Mutex<LogTail>>,
    cancel: Arc<AtomicBool>,
) -> InstallOutcome {
    let commands = rung_commands(rung, python, venv_dir);
    for cmd in &commands {
        if cancel.load(Ordering::SeqCst) {
            return InstallOutcome::Cancelled;
        }
        let mut child = match Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return InstallOutcome::Failed {
                    rung,
                    reason: format!("could not start {}: {e}", cmd.program),
                    code: None,
                }
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut readers = Vec::new();
        if let Some(s) = stdout {
            readers.push(tokio::spawn(pump_lines(s, Arc::clone(&log))));
        }
        if let Some(s) = stderr {
            readers.push(tokio::spawn(pump_lines(s, Arc::clone(&log))));
        }

        let status = tokio::select! {
            st = child.wait() => st.ok(),
            _ = cancel_poll(Arc::clone(&cancel)) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                for r in readers {
                    let _ = r.await;
                }
                return InstallOutcome::Cancelled;
            }
        };

        for r in readers {
            let _ = r.await;
        }

        match status {
            Some(st) if st.success() => {}
            Some(st) => {
                let tail = log
                    .lock()
                    .map(|l| l.tail(3).join(" | "))
                    .unwrap_or_default();
                return InstallOutcome::Failed {
                    rung,
                    reason: if tail.is_empty() {
                        format!("{} exited with failure", cmd.program)
                    } else {
                        tail
                    },
                    code: st.code(),
                };
            }
            None => {
                return InstallOutcome::Failed {
                    rung,
                    reason: format!("{} terminated abnormally", cmd.program),
                    code: None,
                }
            }
        }
    }

    if cancel.load(Ordering::SeqCst) {
        return InstallOutcome::Cancelled;
    }

    let binary = match resolve_installed_binary(exec, rung, facts, venv_dir).await {
        Some(b) => b,
        None => {
            return InstallOutcome::Failed {
                rung,
                reason: "could not resolve installed binary location".to_owned(),
                code: None,
            }
        }
    };

    if verify_binary(exec, &binary).await {
        let bin_dir = binary
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        InstallOutcome::Installed { binary, bin_dir }
    } else {
        InstallOutcome::Failed {
            rung,
            reason: "installed binary failed the `stdio --help` verification".to_owned(),
            code: None,
        }
    }
}

async fn cancel_poll(cancel: Arc<AtomicBool>) {
    loop {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn pump_lines<R>(mut stream: R, log: Arc<Mutex<LogTail>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0u8; 4096];
    let mut pending = String::new();
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pending.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if let Some(last_nl) = pending.rfind('\n') {
                    let complete: String = pending.drain(..=last_nl).collect();
                    if let Ok(mut l) = log.lock() {
                        l.push(&complete);
                    }
                }
            }
        }
    }
    if !pending.trim().is_empty() {
        if let Ok(mut l) = log.lock() {
            l.push(&pending);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MockProbe {
        responses: HashMap<String, ProbeOutput>,
    }

    fn key(program: &str, args: &[&str]) -> String {
        format!("{program} {}", args.join(" "))
    }

    impl ProbeExec for MockProbe {
        fn run(&self, program: &str, args: &[String]) -> BoxFuture<'static, Option<ProbeOutput>> {
            let k = key(
                program,
                &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            );
            let out = self.responses.get(&k).cloned();
            Box::pin(async move { out })
        }
    }

    fn mock_with(responses: Vec<(&str, Vec<&str>, bool, &str)>) -> MockProbe {
        let mut map = HashMap::new();
        for (program, args, success, stdout) in responses {
            map.insert(
                key(program, &args),
                ProbeOutput {
                    success,
                    stdout: stdout.to_owned(),
                },
            );
        }
        MockProbe { responses: map }
    }

    #[test]
    fn version_gate_rejects_39_and_4_x_accepts_311_plus() {
        assert!(!version_ok("3.9.6"), "stock macOS python3 must be rejected");
        assert!(version_ok("3.11.9"));
        assert!(version_ok("3.12.13"));
        assert!(!version_ok("4.0.1"));
        assert!(!version_ok("2.7.18"));
        assert!(!version_ok("garbage"));
    }

    #[test]
    fn candidate_enumeration_orders_versioned_names_first() {
        let dirs = std::env::join_paths([
            PathBuf::from("/usr/bin"),
            PathBuf::from("/opt/homebrew/bin"),
        ])
        .unwrap();
        let cands = python_candidates(TargetOs::Unix, Some(&dirs));
        let names: Vec<String> = cands
            .iter()
            .filter_map(|c| match c {
                Candidate::Direct(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(&names[0], "python3.14");
        assert_eq!(&names[3], "python3.11");
        assert_eq!(&names[4], "python3");
        assert!(!names.contains(&"chibi".to_owned()));
    }

    #[test]
    fn windows_candidates_end_with_py_launcher() {
        let cands = python_candidates(TargetOs::Windows, Some(&std::ffi::OsString::from("")));
        assert!(matches!(cands.last(), Some(Candidate::PyLauncher)));
    }

    fn facts(python: bool, pipx: bool, em: bool) -> Facts {
        Facts {
            python: python.then(|| PythonInfo {
                program: "/usr/local/bin/python3.12".to_owned(),
                prefix_args: vec![],
                version: "3.12.13".to_owned(),
            }),
            pipx_ok: pipx,
            pipx_bin_dir: None,
            externally_managed: em,
        }
    }

    #[test]
    fn ladder_prefers_pipx_then_venv() {
        assert_eq!(
            first_applicable_rung(&facts(true, true, true)),
            Some(Rung::Pipx)
        );
        assert_eq!(
            first_applicable_rung(&facts(true, false, true)),
            Some(Rung::Venv)
        );
    }

    #[test]
    fn ladder_requires_python() {
        assert_eq!(first_applicable_rung(&Facts::default()), None);
        assert!(applicable_rungs(&Facts::default()).is_empty());
    }

    #[test]
    fn pip_user_only_when_not_externally_managed() {
        let em = facts(true, false, true);
        assert!(!applicable_rungs(&em).contains(&Rung::PipUser));
        let not_em = facts(true, false, false);
        assert!(applicable_rungs(&not_em).contains(&Rung::PipUser));
    }

    #[test]
    fn break_system_packages_is_always_last() {
        let f = facts(true, false, false);
        let rungs = applicable_rungs(&f);
        assert_eq!(rungs.last(), Some(&Rung::BreakSystemPackages));
        assert_eq!(
            next_applicable_rung(Rung::PipUser, &f),
            Some(Rung::BreakSystemPackages)
        );
    }

    #[test]
    fn fallback_chain_skips_inapplicable_rungs() {
        let with_pipx = facts(true, true, true);
        assert_eq!(
            next_applicable_rung(Rung::Pipx, &with_pipx),
            Some(Rung::Venv)
        );
        let em = facts(true, false, true);
        assert_eq!(
            next_applicable_rung(Rung::Venv, &em),
            Some(Rung::BreakSystemPackages),
            "externally-managed: pip --user is skipped"
        );
        let not_em = facts(true, false, false);
        assert_eq!(
            next_applicable_rung(Rung::Venv, &not_em),
            Some(Rung::PipUser)
        );
    }

    #[test]
    fn rung_commands_use_package_chibi_bot() {
        let py = PythonInfo {
            program: "/usr/local/bin/python3.12".to_owned(),
            prefix_args: vec![],
            version: "3.12.13".to_owned(),
        };
        let venv = PathBuf::from("/data/chibi-tui/venv");
        let pipx = rung_commands(Rung::Pipx, &py, &venv);
        assert!(pipx[0].args.contains(&PACKAGE.to_owned()));
        assert_eq!(pipx[0].program, "pipx");
        let venv_cmds = rung_commands(Rung::Venv, &py, &venv);
        assert_eq!(
            venv_cmds.len(),
            2,
            "venv rung creates venv then pips inside"
        );
        assert!(venv_cmds[0].args.contains(&venv.display().to_string()));
        assert!(venv_cmds[1].program.starts_with("/data/chibi-tui/venv"));
        assert!(venv_cmds[1].args.contains(&PACKAGE.to_owned()));
        let user = rung_commands(Rung::PipUser, &py, &venv);
        assert!(user[0].args.contains(&"--user".to_owned()));
        assert!(!user[0].args.contains(&"--break-system-packages".to_owned()));
        let bsp = rung_commands(Rung::BreakSystemPackages, &py, &venv);
        assert!(bsp[0].args.contains(&"--break-system-packages".to_owned()));
        assert!(bsp[0].args.contains(&PACKAGE.to_owned()));
    }

    #[test]
    fn log_tail_strips_ansi_and_splits_carriage_returns() {
        let mut tail = LogTail::new(10);
        tail.push("\u{1b}[31mCollecting chibi-bot\u{1b}[0m\r  Downloading pkg-1.0.whl (2 MB)\r");
        let lines = tail.tail(10);
        assert!(lines.iter().all(|l| !l.contains('\u{1b}')));
        assert!(lines.iter().any(|l| l.contains("Collecting chibi-bot")));
        assert!(lines.iter().any(|l| l.contains("Downloading pkg-1.0.whl")));
    }

    #[test]
    fn log_tail_is_bounded() {
        let mut tail = LogTail::new(5);
        for i in 0..20 {
            tail.push(&format!("line {i}"));
        }
        assert_eq!(tail.tail(100).len(), 5);
        assert!(tail.tail(1)[0].contains("line 19"));
    }

    #[tokio::test]
    async fn mock_discovery_finds_pipx_and_rejects_old_python() {
        let exec = mock_with(vec![
            (
                "/usr/bin/python3",
                vec!["-c", VERSION_CODE],
                true,
                "3.9.6\n",
            ),
            (
                "/opt/homebrew/bin/python3.12",
                vec!["-c", VERSION_CODE],
                true,
                "3.12.13\n",
            ),
            ("pipx", vec!["--version"], true, "1.6.0\n"),
            (
                "pipx",
                vec!["environment", "--value", "PIPX_BIN_DIR"],
                true,
                "/Users/u/.local/bin\n",
            ),
            (
                "/opt/homebrew/bin/python3.12",
                vec!["-c", EXTERNALLY_MANAGED_CODE],
                true,
                "True\n",
            ),
        ]);
        let f = discover(&exec, TargetOs::Unix).await;
        let py = f.python.clone().expect("3.12 candidate must be found");
        assert_eq!(py.version, "3.12.13");
        assert!(f.pipx_ok);
        assert!(f.externally_managed);
        assert_eq!(first_applicable_rung(&f), Some(Rung::Pipx));
    }

    #[tokio::test]
    async fn mock_discovery_reports_no_python_when_all_fail() {
        let exec = mock_with(vec![]);
        let f = discover(&exec, TargetOs::Unix).await;
        assert!(f.python.is_none());
        assert_eq!(first_applicable_rung(&f), None);
    }
}
