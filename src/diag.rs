//! Diagnostics log (feat_stderr_log_modal): one unified, in-memory diagnostic
//! stream built from two sources:
//!
//! * the backend's **stderr** — pumped line by line by
//!   [`crate::backend_client`] (timestamps come from the backend itself, so
//!   lines are appended verbatim);
//! * **TUI-side lifecycle events** — spawn, handshake ok/fail, reconnect,
//!   pipe closed — appended with a `[tui]` prefix by whichever layer owns the
//!   moment ([`crate::backend_client`], [`crate::request_pipeline`]).
//!
//! Storage is a fixed-capacity ring buffer ([`LOG_CAPACITY`] lines, oldest
//! evicted) behind a process-global mutex. Appends never block the event loop
//! or the protocol path: the capture task takes the lock only for the instant
//! it takes to push one line, and the log viewer modal reads a snapshot copy
//! (never a live reference).
//!
//! Optional file sink: when the `CHIBI_TUI_LOG` env var is set to a path,
//! every appended line is mirrored to that file (append mode, parent
//! directories created on first use). Unset (the default) = memory only. A
//! sink that cannot be opened is silently disabled — diagnostics must never
//! break the app.
//!
//! Honest limitation: only stderr reaches this buffer. The backend's loguru
//! logging currently writes to its **stdout** (the protocol channel), so
//! backend log lines do not land here; the backend-side sink fix is a
//! separate backend task.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Ring-buffer capacity in lines; the oldest line is evicted beyond this.
/// Configurable const: diagnostics at this rate stay cheap, and 512 lines is
/// comfortably more than one viewer screen while bounding memory.
pub const LOG_CAPACITY: usize = 512;

/// Env var that turns on the file mirror. Value = path of the log file
/// (append mode). Unset or empty = memory only (default).
pub const FILE_SINK_ENV: &str = "CHIBI_TUI_LOG";

/// Prefix stamped on TUI-side lifecycle events (vs. verbatim backend stderr).
pub const TUI_EVENT_PREFIX: &str = "[tui]";

// ---------------------------------------------------------------------------
// Core (testable) implementation — a plain struct, no globals
// ---------------------------------------------------------------------------

/// Optional append-mode mirror of the log stream.
enum Sink {
    /// No file mirror (default).
    None,
    /// Mirror attempted but unusable (open/create failed): disabled for good,
    /// retries would just spam the same failure.
    Failed,
    Open(File),
}

/// The diagnostics stream: bounded line ring + monotonic counter + optional
/// file mirror. Use the process-global accessors below in production code;
/// construct instances directly in tests.
///
/// "Unseen lines" is tracked by the CONSUMER, not here: [`App`][crate::app::App]
/// keeps `log_seen_total` (the [`DiagLog::total`] value at its last full
/// view) and the `log*` status marker fires while `total() > log_seen_total`.
/// A per-consumer counter over a monotonic producer total is race-free by
/// construction (totals only ever grow) and survives multiple open/close
/// cycles without extra global state.
pub struct DiagLog {
    lines: VecDeque<String>,
    /// Monotonic count of ALL lines ever appended (survives eviction) — the
    /// baseline both the viewer's `+K new lines` hint and the consumer-side
    /// unseen tracking are computed against.
    total: u64,
    sink: Sink,
}

impl DiagLog {
    /// Memory-only log (no file mirror).
    pub fn new() -> Self {
        Self::with_sink_path(None)
    }

    /// Log with an optional file mirror at `path` (append mode; parent
    /// directories created on first use). A path that cannot be opened
    /// degrades silently to memory-only.
    pub fn with_sink_path(path: Option<&Path>) -> Self {
        let sink = match path {
            None => Sink::None,
            Some(path) => match open_sink(path) {
                Ok(file) => Sink::Open(file),
                Err(_) => Sink::Failed,
            },
        };
        Self {
            lines: VecDeque::with_capacity(LOG_CAPACITY),
            total: 0,
            sink,
        }
    }

    /// Append ONE line (already line-split by the caller). Evicts the oldest
    /// line beyond [`LOG_CAPACITY`] and mirrors to the file sink when one is
    /// configured.
    pub fn push(&mut self, line: impl Into<String>) {
        let line = line.into();
        if self.lines.len() >= LOG_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line.clone());
        self.total = self.total.saturating_add(1);
        if let Sink::Open(file) = &mut self.sink {
            // Best-effort mirror: a write failure (disk full, file removed)
            // must never panic or surface — diagnostics stay memory-only.
            let _ = writeln!(file, "{line}");
        }
    }

    /// Snapshot copy of the buffered lines (oldest → newest). The viewer
    /// modal reads a copy so the ring can keep appending freely while the
    /// modal holds and renders its view.
    pub fn snapshot(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    /// Current buffered line count (≤ [`LOG_CAPACITY`]).
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True when nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Monotonic count of all lines ever appended (never reset, survives
    /// eviction). Monotonicity is what makes the viewer's `+K new lines`
    /// arithmetic race-free: `total_now - total_at_snapshot` is exact for a
    /// single reader, and unseen tracking (`total() > seen_total`) needs no
    /// reset coordination at all.
    pub fn total(&self) -> u64 {
        self.total
    }
}

impl Default for DiagLog {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DiagLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiagLog")
            .field("buffered", &self.lines.len())
            .field("total", &self.total)
            .finish_non_exhaustive()
    }
}

/// Open the file mirror: append mode, create if missing, create parent dirs.
fn open_sink(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Parse the `CHIBI_TUI_LOG` value into a sink path: unset or EMPTY means
/// memory-only (a stray empty env var must not create a file named "").
fn parse_sink_path(raw: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let raw = raw?;
    if raw.is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

/// Format one TUI-side lifecycle event: `[tui] <event>` on a single line
/// (embedded newlines collapse to spaces so the viewer never renders a torn
/// row).
pub fn format_tui_event(event: impl fmt::Display) -> String {
    format!(
        "{TUI_EVENT_PREFIX} {}",
        event.to_string().replace('\n', " ")
    )
}

// ---------------------------------------------------------------------------
// Process-global accessors (production surface)
// ---------------------------------------------------------------------------

static LOG: OnceLock<Mutex<DiagLog>> = OnceLock::new();

fn cell() -> &'static Mutex<DiagLog> {
    LOG.get_or_init(|| {
        let sink = parse_sink_path(std::env::var_os(FILE_SINK_ENV).as_deref());
        Mutex::new(DiagLog::with_sink_path(sink.as_deref()))
    })
}

/// Lock helper that tolerates poisoning (a panicking writer must not turn
/// every later diagnostic append into a panic — recover the guarded data).
fn lock() -> std::sync::MutexGuard<'static, DiagLog> {
    cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Append one line to the global log (verbatim — the backend's stderr comes
/// pre-split by [`crate::backend_client`]). Never fails, never blocks for
/// more than a push.
pub fn append(line: impl Into<String>) {
    lock().push(line);
}

/// Append one TUI-side lifecycle event (`[tui]`-prefixed) to the global log.
pub fn append_tui(event: impl fmt::Display) {
    lock().push(format_tui_event(event));
}

/// Snapshot copy of the global buffer (oldest → newest) for the viewer modal.
pub fn snapshot() -> Vec<String> {
    lock().snapshot()
}

/// Monotonic total of lines ever appended to the global log.
pub fn total_appended() -> u64 {
    lock().total()
}

/// Atomically snapshot the stream and return `(lines, total)` in ONE lock
/// acquisition. The log viewer uses this when it opens and when it re-arms
/// live-tail at the bottom, so the `+K new lines` baseline is always
/// consistent with the snapshot it came with (no interleaved append can slip
/// between the two reads).
pub fn view() -> (Vec<String>, u64) {
    let log = lock();
    (log.snapshot(), log.total())
}

// ---------------------------------------------------------------------------
// Tests — all on LOCAL instances (the global is process state; parallel test
// threads appending through it would make exact assertions racy)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp dir per call (no tempfile dependency — a process-unique
    /// name + an atomic counter is enough for these tests).
    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "chibi_tui_diag_{}_{}_{}",
            std::process::id(),
            tag,
            n
        ));
        std::fs::create_dir_all(&dir).expect("temp dir created");
        dir
    }

    // ---- ring eviction -----------------------------------------------------

    #[test]
    fn ring_respects_capacity_and_evicts_oldest() {
        let mut log = DiagLog::new();
        let extra = 37usize;
        for i in 0..(LOG_CAPACITY + extra) {
            log.push(format!("line-{i}"));
        }
        assert_eq!(log.len(), LOG_CAPACITY, "ring never grows past the cap");
        // The oldest `extra` lines were evicted; the first survivor is #extra.
        let snap = log.snapshot();
        assert_eq!(snap.first().map(String::as_str), Some("line-37"));
        assert_eq!(
            snap.last().map(String::as_str),
            Some(format!("line-{}", LOG_CAPACITY + extra - 1).as_str())
        );
        // Snapshot is a COPY: mutating it does not touch the ring.
        let mut copy = log.snapshot();
        copy.clear();
        assert_eq!(log.len(), LOG_CAPACITY, "snapshot must be a copy");
    }

    #[test]
    fn total_is_monotonic_across_eviction() {
        let mut log = DiagLog::new();
        for i in 0..(LOG_CAPACITY * 2) {
            log.push(format!("line-{i}"));
        }
        assert_eq!(log.total(), (LOG_CAPACITY * 2) as u64);
        assert_eq!(log.len(), LOG_CAPACITY);
        let before = log.total();
        log.push("one more".to_owned());
        assert_eq!(log.total(), before + 1);
    }

    // ---- monotonic total (unseen tracking baseline) ------------------------

    #[test]
    fn total_only_grows_and_survives_eviction() {
        let mut log = DiagLog::new();
        for i in 0..(LOG_CAPACITY + 10) {
            log.push(format!("line-{i}"));
        }
        assert_eq!(log.total(), (LOG_CAPACITY + 10) as u64);
        assert_eq!(log.len(), LOG_CAPACITY, "ring capped, total keeps counting");

        let before = log.total();
        log.push("one more");
        assert_eq!(log.total(), before + 1);
    }

    // ---- line splitting / partial lines ------------------------------------

    #[test]
    fn push_is_one_line_verbatim() {
        let mut log = DiagLog::new();
        log.push("plain line");
        log.push(""); // empty stderr lines stay verbatim
        assert_eq!(log.len(), 2);
        let snap = log.snapshot();
        assert_eq!(snap[0], "plain line");
        assert_eq!(snap[1], "");
    }

    // ---- file sink -----------------------------------------------------------

    #[test]
    fn file_sink_mirrors_lines_and_creates_dirs() {
        let dir = temp_dir("sink");
        // Nested path proves create_dir_all on first use.
        let path = dir.join("logs").join("nested").join("tui.log");
        let mut log = DiagLog::with_sink_path(Some(&path));
        log.push("first");
        log.push("second");

        let content = std::fs::read_to_string(&path).expect("sink file exists");
        assert_eq!(content, "first\nsecond\n", "mirror is append-mode verbatim");

        // Re-opening the same path appends instead of truncating.
        let mut log2 = DiagLog::with_sink_path(Some(&path));
        log2.push("third");
        let content = std::fs::read_to_string(&path).expect("sink file exists");
        assert_eq!(content, "first\nsecond\nthird\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_sink_degrades_to_memory_only() {
        // A path whose "parent" is a FILE: create_dir_all must fail there,
        // and the log must keep working in memory.
        let dir = temp_dir("badsink");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a dir").expect("blocker written");
        let path = blocker.join("impossible").join("tui.log");

        let mut log = DiagLog::with_sink_path(Some(&path));
        log.push("still buffered");
        assert_eq!(log.len(), 1, "sink failure must not lose lines");
        assert_eq!(log.snapshot(), ["still buffered".to_owned()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- env parsing / event formatting --------------------------------------

    #[test]
    fn sink_path_parsing_rejects_empty() {
        use std::ffi::OsStr;
        assert_eq!(parse_sink_path(None), None, "unset env ⇒ memory only");
        assert_eq!(
            parse_sink_path(Some(OsStr::new(""))),
            None,
            "empty env ⇒ memory only (no empty-named file)"
        );
        assert_eq!(
            parse_sink_path(Some(OsStr::new("/tmp/tui.log"))),
            Some(PathBuf::from("/tmp/tui.log"))
        );
    }

    #[test]
    fn tui_events_carry_prefix_and_single_line() {
        assert_eq!(format_tui_event("spawn `chibi`"), "[tui] spawn `chibi`");
        assert_eq!(
            format_tui_event("pipe closed: a\nb"),
            "[tui] pipe closed: a b",
            "embedded newlines collapse so rows stay intact"
        );
    }
}
