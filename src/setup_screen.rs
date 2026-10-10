//! Startup screen reworked into a full missing-backend onboarding flow.
//!
//! Live mode spawns the `chibi` backend. When that binary is not there (a
//! fresh install without the backend), this screen walks the user through
//! detection → offer → install → verify → hand off to the backend config
//! screen:
//!
//! `Intro → Probing → Offer → NoPython → Installing{streamed tail} → Failed
//! → Installed`.
//!
//! Discovery is BY EXECUTION only: interpreter binaries are probed with a
//! `-c` version one-liner (never trusted by name — stock macOS
//! `/usr/bin/python3` is 3.9.6), pipx is probed with `pipx --version` (a
//! pyenv shim without a backing install fails), and PEP 668
//! `EXTERNALLY-MANAGED` markers decide whether `pip --user` is ever legal.
//! The install itself uses the owner-approved ladder in
//! [`crate::backend_install`] with a streamed output tail.
//!
//! The screen runs before the main event loop starts, so `r` needs no extra
//! wiring: the caller just re-enters its connect attempt. After a verified
//! install the caller hands off to `config_screen::run` (BYOK: configure
//! even when keys already exist) and then respawns the backend.
//!
//! # Key grammar
//!
//! * `i` — start the install (Offer state)
//! * `n` — after a failed rung, try the next ladder option (an explicit,
//!   warned opt-in for `--break-system-packages`)
//! * `p` — re-probe (NoPython state)
//! * `r` — retry the backend spawn
//! * `q`/Esc — quit confirmation (shared grammar), EXCEPT while installing,
//!   where Esc cancels the install and Ctrl+C quits (killing the child)
//! * Ctrl+C — quit confirmation elsewhere; quit-anyway while installing

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::oneshot;

use crate::app::{quit_decision, QuitDecision};
use crate::backend_client::BackendError;
use crate::backend_install::{
    first_applicable_rung, next_applicable_rung, spawn_install, venv_dir_for, Facts, InstallHandle,
    InstallOutcome, LogTail, Rung,
};
use crate::backend_store;
use crate::theme::Theme;

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

/// OS family used to pick the command variants on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetOs {
    Unix,
    Windows,
}

impl TargetOs {
    /// Host OS, resolved at compile time (single-binary builds).
    pub fn current() -> Self {
        if cfg!(windows) {
            TargetOs::Windows
        } else {
            TargetOs::Unix
        }
    }
}

/// When the setup screen applies: the default binary could not be spawned
/// and the user did not point at one via `CHIBI_BACKEND_BIN`. Every other
/// startup failure (existing but broken binary, handshake problems) keeps
/// the regular error popup, because there the error text itself is the
/// useful part.
pub fn applies(err: &BackendError, backend_bin_env: Option<&OsStr>) -> bool {
    matches!(err, BackendError::Spawn { .. }) && backend_bin_env.is_none()
}

/// The effective binary override for the spawn/gate decision: an explicit
/// `CHIBI_BACKEND_BIN` env value, else the installer's persisted path — but
/// ONLY while that binary still exists. A persisted path whose binary has
/// vanished must NOT masquerade as an override: the gate rule requires the
/// setup screen to still apply so the user can reinstall. Pure helper takes
/// both inputs explicitly (unit-testable without touching process env).
pub fn backend_override_with(env: Option<&OsStr>, persisted: Option<&Path>) -> Option<OsString> {
    if let Some(v) = env {
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    match persisted {
        Some(p) if p.exists() => Some(p.as_os_str().to_owned()),
        _ => None,
    }
}

/// [`backend_override_with`] against the live process env + persisted store.
pub fn backend_override() -> Option<OsString> {
    let env = std::env::var_os("CHIBI_BACKEND_BIN");
    let persisted = backend_store::load_backend_path();
    backend_override_with(env.as_deref(), persisted.as_deref())
}

/// Outcome of the screen loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// `r`: re-attempt the backend spawn.
    Retry,
    /// `q`/Esc/Ctrl+C, confirmed via the shared quit confirmation: leave
    /// the app.
    Quit,
    /// A verified install completed: the caller opens the backend config
    /// screen, then re-attempts the spawn.
    Installed,
}

/// Internal screen states of the onboarding flow.
#[derive(Debug)]
enum Screen {
    /// Static intro content (corrected manual hints) shown while probing
    /// starts.
    Intro,
    /// Python >=3.11,<4 + pipx probes running by execution.
    Probing,
    /// Interpreter found: offer the first applicable ladder rung.
    Offer {
        rung: Rung,
        note: Option<&'static str>,
    },
    /// No usable Python found: warn + platform hints + re-probe.
    NoPython,
    /// Install running with a streamed output tail.
    Installing { rung: Rung },
    /// A rung failed: show reason, tail, and the next ladder option.
    Failed {
        rung: Rung,
        reason: String,
        next: Option<Rung>,
    },
    /// Verified install; rendered for one frame, then handed off.
    Installed { binary: std::path::PathBuf },
}

/// Full static screen content for `os`, styled with the app theme. Command
/// lines stay on their own lines so a terminal copy-paste grabs them cleanly.
pub fn content_lines(os: TargetOs, theme: &Theme) -> Vec<Line<'static>> {
    let bold_fg = Style::new().fg(theme.fg).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(theme.dim);
    let cmd = Style::new().fg(theme.green);
    let head = Style::new().fg(theme.cyan).add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            "The backend binary `chibi` was not found on PATH.",
            bold_fg,
        )),
        Line::from(Span::styled(
            "chibi-tui spawns it to reach the AI. Install it with one",
            dim,
        )),
        Line::from(Span::styled(
            "of the options below, then press r to retry.",
            dim,
        )),
        Line::from(String::new()),
        Line::from(Span::styled("pipx (recommended)", head)),
        Line::from(Span::styled("  pipx install chibi-bot", cmd)),
        Line::from(String::new()),
        Line::from(Span::styled("venv (manual)", head)),
    ];
    match os {
        TargetOs::Unix => {
            lines.push(Line::from(Span::styled(
                "  python3 -m venv ~/.local/share/chibi-tui/venv",
                cmd,
            )));
            lines.push(Line::from(Span::styled(
                "  ~/.local/share/chibi-tui/venv/bin/pip install chibi-bot",
                cmd,
            )));
            lines.push(Line::from(Span::styled(
                "  export CHIBI_BACKEND_BIN=~/.local/share/chibi-tui/venv/bin/chibi",
                cmd,
            )));
        }
        TargetOs::Windows => {
            lines.push(Line::from(Span::styled(
                r"  py -3 -m venv %USERPROFILE%\.local\share\chibi-tui\venv",
                cmd,
            )));
            lines.push(Line::from(Span::styled(
                r"  %USERPROFILE%\.local\share\chibi-tui\venv\Scripts\pip install chibi-bot",
                cmd,
            )));
            lines.push(Line::from(Span::styled(
                r"  set CHIBI_BACKEND_BIN=%USERPROFILE%\.local\share\chibi-tui\venv\Scripts\chibi.exe",
                cmd,
            )));
        }
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "Already have a backend? Point the TUI at it:",
        head,
    )));
    match os {
        TargetOs::Unix => lines.push(Line::from(Span::styled(
            "  export CHIBI_BACKEND_BIN=/path/to/chibi",
            cmd,
        ))),
        TargetOs::Windows => lines.push(Line::from(Span::styled(
            r"  set CHIBI_BACKEND_BIN=C:\path\to\chibi.exe",
            cmd,
        ))),
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "r retry \u{00b7} q/Esc quit",
        Style::new().fg(theme.yellow),
    )));
    lines
}

/// Offer-state content: the static intro plus the concrete install offer for
/// `rung` (first applicable ladder rung for the probed environment).
pub fn offer_lines(
    os: TargetOs,
    theme: &Theme,
    rung: Rung,
    note: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = content_lines(os, theme);
    let head = Style::new().fg(theme.cyan).add_modifier(Modifier::BOLD);
    let cmd = Style::new().fg(theme.green);
    let yellow = Style::new().fg(theme.yellow);
    let dim = Style::new().fg(theme.dim);

    if let Some(note) = note {
        lines.push(Line::from(Span::styled(note.to_owned(), yellow)));
        lines.push(String::new().into());
    }
    lines.push(Line::from(Span::styled(
        format!("One-click install ({}):", rung.label()),
        head,
    )));
    lines.push(Line::from(Span::styled("  i  install chibi-bot now", cmd)));
    lines.push(Line::from(Span::styled(
        "Output streams below; the binary is verified with",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "`stdio --help` before the backend config screen opens.",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "i install \u{00b7} r retry \u{00b7} q/Esc quit",
        yellow,
    )));
    lines
}

/// NoPython-state content: version-mismatch warning + platform install hints.
pub fn no_python_lines(os: TargetOs, theme: &Theme) -> Vec<Line<'static>> {
    let bold_fg = Style::new().fg(theme.fg).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(theme.dim);
    let cmd = Style::new().fg(theme.green);
    let yellow = Style::new().fg(theme.yellow);

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            "No usable Python 3.11+ interpreter was found.",
            bold_fg,
        )),
        Line::from(Span::styled(
            "chibi-bot needs Python >=3.11,<4 (probed by running,",
            dim,
        )),
        Line::from(Span::styled(
            "not by name \u{2014} stock /usr/bin/python3 is 3.9.6 on macOS).",
            dim,
        )),
        String::new().into(),
        Line::from(Span::styled(
            "Install Python 3.11 or newer, then re-probe:",
            dim,
        )),
    ];
    match os {
        TargetOs::Unix => {
            lines.push(Line::from(Span::styled("  brew install python@3.12", cmd)));
            lines.push(Line::from(Span::styled(
                "  or download from https://www.python.org/downloads/",
                dim,
            )));
        }
        TargetOs::Windows => {
            lines.push(Line::from(Span::styled(
                "  winget install Python.Python.3.12",
                cmd,
            )));
            lines.push(Line::from(Span::styled(
                "  or download from https://www.python.org/downloads/",
                dim,
            )));
        }
    }
    lines.push(String::new().into());
    lines.push(Line::from(Span::styled(
        "p re-probe \u{00b7} r retry \u{00b7} q/Esc quit",
        yellow,
    )));
    lines
}

/// Failed-state content: rung, reason, tail, and the explicit next-step
/// offer. A next rung of `--break-system-packages` always carries a loud
/// warning: the last-resort rung is never silent.
pub fn failed_lines(
    theme: &Theme,
    rung: Rung,
    reason: &str,
    tail: &[String],
    next: Option<Rung>,
) -> Vec<Line<'static>> {
    let bold_red = Style::new().fg(theme.red).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(theme.dim);
    let cmd = Style::new().fg(theme.green);
    let yellow = Style::new().fg(theme.yellow);

    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        format!("Install via {} failed.", rung.label()),
        bold_red,
    ))];
    lines.push(Line::from(Span::styled(reason.to_owned(), dim)));
    if !tail.is_empty() {
        lines.push(String::new().into());
        for l in tail.iter().rev().take(5).rev() {
            lines.push(Line::from(Span::styled(l.clone(), dim)));
        }
        lines.push(String::new().into());
    }
    match next {
        Some(Rung::BreakSystemPackages) => {
            lines.push(Line::from(Span::styled(
                "WARNING: the next option overrides your system Python's",
                yellow,
            )));
            lines.push(Line::from(Span::styled(
                "package protection (--break-system-packages).",
                yellow,
            )));
            lines.push(Line::from(Span::styled(
                "  n  install anyway (not recommended)",
                cmd,
            )));
        }
        Some(r) => {
            lines.push(Line::from(Span::styled(
                format!("  n  try next option: {}", r.label()),
                cmd,
            )));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "No further automatic options. Install manually or",
                dim,
            )));
            lines.push(Line::from(Span::styled(
                "point CHIBI_BACKEND_BIN at an existing binary.",
                dim,
            )));
        }
    }
    lines.push(String::new().into());
    lines.push(Line::from(Span::styled(
        "n next option \u{00b7} r retry spawn \u{00b7} q/Esc quit",
        yellow,
    )));
    lines
}

/// Installing-state content: rung title, spinner, and the streamed tail.
pub fn installing_lines(
    theme: &Theme,
    rung: Rung,
    log: &LogTail,
    spinner: usize,
) -> Vec<Line<'static>> {
    let head = Style::new().fg(theme.cyan).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(theme.dim);
    let yellow = Style::new().fg(theme.yellow);
    const SPINNER: [&str; 10] = [
        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
        "\u{2827}", "\u{2807}", "\u{280f}",
    ];

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            format!(
                "Installing chibi-bot via {} {}",
                rung.label(),
                SPINNER[spinner % SPINNER.len()]
            ),
            head,
        )),
        String::new().into(),
    ];
    if log.is_empty() {
        lines.push(Line::from(Span::styled(
            "Starting installer\u{2026} (network activity is normal)",
            dim,
        )));
    } else {
        for l in log.tail(12) {
            lines.push(Line::from(Span::styled(unicode_truncate(&l, 72), dim)));
        }
    }
    lines.push(String::new().into());
    lines.push(Line::from(Span::styled(
        "Esc cancel install \u{00b7} Ctrl+C quit",
        yellow,
    )));
    lines
}

/// Installed-state content: verified path + handoff notice.
pub fn installed_lines(theme: &Theme, binary: &Path) -> Vec<Line<'static>> {
    let green = Style::new().fg(theme.green).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(theme.dim);
    vec![
        Line::from(Span::styled("chibi-bot installed and verified.", green)),
        Line::from(Span::styled(format!("  {}", binary.display()), dim)),
        String::new().into(),
        Line::from(Span::styled("Opening backend configuration\u{2026}", dim)),
    ]
}

fn unicode_truncate(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;
    if UnicodeWidthStr::width(s) <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    for c in s.chars() {
        if UnicodeWidthStr::width(out.as_str()) + c.width().unwrap_or(0) > max - 1 {
            out.push('\u{2026}');
            return out;
        }
        out.push(c);
    }
    out
}

/// Draw the static intro screen (legacy shape kept for the shared quit
/// confirm overlay and copy-paste hints).
pub fn draw(f: &mut Frame, theme: &Theme, quit_confirm: bool) {
    let lines = content_lines(TargetOs::current(), theme);
    render_centered(
        f,
        theme,
        lines,
        " Backend setup ",
        theme.orange,
        quit_confirm,
    );
}

/// Draw one setup frame for the current onboarding state. `quit_confirm`
/// overlays the shared "Quit chibi-tui?" popup (the same renderer the
/// in-app confirm uses, the same shared grammar — see
/// [`crate::app::quit_decision`]).
fn draw_state(
    f: &mut Frame,
    state: &Screen,
    theme: &Theme,
    quit_confirm: bool,
    install_log: Option<&Arc<Mutex<LogTail>>>,
    spinner: usize,
) {
    let os = TargetOs::current();
    let (lines, title, border): (Vec<Line<'static>>, String, Color) = match state {
        Screen::Intro => (
            content_lines(os, theme),
            " Backend setup ".to_owned(),
            theme.orange,
        ),
        Screen::Probing => {
            let mut l = content_lines(os, theme);
            l.push(Line::from(Span::styled(
                "probing for Python 3.11+ and pipx\u{2026}",
                Style::new().fg(theme.yellow),
            )));
            (l, " Backend setup ".to_owned(), theme.orange)
        }
        Screen::Offer { rung, note } => (
            offer_lines(os, theme, *rung, *note),
            " Backend setup ".to_owned(),
            theme.orange,
        ),
        Screen::NoPython => (
            no_python_lines(os, theme),
            " Python needed ".to_owned(),
            theme.orange,
        ),
        Screen::Installing { rung } => {
            let view = install_log
                .and_then(|l| l.lock().ok())
                .map(|g| g.clone())
                .unwrap_or_else(|| LogTail::new(1));
            (
                installing_lines(theme, *rung, &view, spinner),
                format!(" Installing chibi-bot ({}) ", rung.label()),
                theme.orange,
            )
        }
        Screen::Failed { rung, reason, next } => {
            let tail = install_log
                .and_then(|l| l.lock().ok().map(|g| g.tail(5)))
                .unwrap_or_default();
            (
                failed_lines(theme, *rung, reason, &tail, *next),
                " Install failed ".to_owned(),
                theme.red,
            )
        }
        Screen::Installed { binary } => (
            installed_lines(theme, binary),
            " Setup complete ".to_owned(),
            theme.green,
        ),
    };
    render_centered(f, theme, lines, &title, border, quit_confirm);
}

/// Shared centered-block rendering: theme background everywhere plus a
/// bordered block sized to the content (same shape as the modal popups).
fn render_centered(
    f: &mut Frame,
    theme: &Theme,
    lines: Vec<Line<'static>>,
    title: &str,
    border_color: Color,
    quit_confirm: bool,
) {
    use unicode_width::UnicodeWidthStr;

    let area = f.area();
    f.render_widget(Block::default().style(Style::new().bg(theme.bg)), area);

    let content_w = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0) as u16;
    let content_h = lines.len() as u16;

    // +4 covers the border pair and the horizontal padding, same budget the
    // error popup uses. Tiny terminals just clip, like the popups do.
    let width = (content_w + 4)
        .clamp(30, area.width.saturating_sub(4))
        .max(20);
    let height = (content_h + 4)
        .clamp(5, area.height.saturating_sub(2))
        .max(3);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(border_color))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title,
            Style::new().fg(border_color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines), inner);

    if quit_confirm {
        crate::ui::render_quit_confirm(f, theme);
    }
}

/// Spawn the by-execution environment probe (Python, pipx, PEP 668).
fn spawn_probe(os: TargetOs) -> oneshot::Receiver<Facts> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let exec = crate::backend_install::SysProbe::default();
        let facts = discover(&exec, os).await;
        let _ = tx.send(facts);
    });
    rx
}

use crate::backend_install::discover;

/// Begin an install rung: split the handle so the loop can cancel, draw the
/// streamed tail, and await completion without borrow conflicts.
fn start_install(
    facts: &Facts,
    rung: Rung,
) -> (
    Rung,
    Arc<AtomicBool>,
    Arc<Mutex<LogTail>>,
    oneshot::Receiver<InstallOutcome>,
) {
    let venv = venv_dir_for(&backend_store::resolve_root());
    let InstallHandle {
        log,
        cancel_flag,
        done,
    } = spawn_install(facts, rung, venv);
    (rung, cancel_flag, log, done)
}

/// Run the onboarding loop.
///
/// Returns [`Flow::Retry`] for `r` (the caller re-attempts the spawn),
/// [`Flow::Installed`] after a verified install (the caller opens the
/// backend config screen and respawns), and [`Flow::Quit`] for a CONFIRMED
/// `q`/Esc/Ctrl+C: the exit keys open the shared quit-confirmation popup
/// first (`y`/Enter quits, `n`/Esc/`q` returns to the screen; the same
/// grammar as the in-app confirm and the splash). While installing, `Esc`
/// cancels the install (child killed) and Ctrl+C quits anyway after killing
/// the child.
pub async fn run(terminal: &mut Tui, reader: &mut EventStream, theme: &Theme) -> io::Result<Flow> {
    let os = TargetOs::current();
    let mut state = Screen::Intro;
    let mut probe_rx: Option<oneshot::Receiver<Facts>> = Some(spawn_probe(os));
    let mut facts = Facts::default();
    let mut install_rx: Option<oneshot::Receiver<InstallOutcome>> = None;
    let mut install_rung: Option<Rung> = None;
    let mut install_cancel: Option<Arc<AtomicBool>> = None;
    let mut install_log: Option<Arc<Mutex<LogTail>>> = None;
    let mut quit_confirm = false;
    let mut spinner = 0usize;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|f| {
            draw_state(
                f,
                &state,
                theme,
                quit_confirm,
                install_log.as_ref(),
                spinner,
            )
        })?;

        if let Screen::Installed { binary } = &state {
            let bin = binary.clone();
            backend_store::save_backend_path(&bin);
            return Ok(Flow::Installed);
        }

        // Poll the background probe (100 ms granularity is fine here).
        if let Some(rx) = probe_rx.as_mut() {
            match rx.try_recv() {
                Ok(f) => {
                    probe_rx = None;
                    facts = f;
                    state = match first_applicable_rung(&facts) {
                        Some(rung) => Screen::Offer { rung, note: None },
                        None => Screen::NoPython,
                    };
                    continue;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    probe_rx = None;
                    state = Screen::NoPython;
                    continue;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }

        // Poll install completion.
        if let Some(rx) = install_rx.as_mut() {
            match rx.try_recv() {
                Ok(outcome) => {
                    let rung = install_rung.take().unwrap_or(Rung::Venv);
                    install_rx = None;
                    install_cancel = None;
                    match outcome {
                        InstallOutcome::Installed { binary, .. } => {
                            state = Screen::Installed { binary };
                        }
                        InstallOutcome::Cancelled => {
                            state = Screen::Offer {
                                rung,
                                note: Some("Install cancelled."),
                            };
                        }
                        InstallOutcome::Failed { reason, .. } => {
                            let next = next_applicable_rung(rung, &facts);
                            state = Screen::Failed { rung, reason, next };
                        }
                    }
                    continue;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    let rung = install_rung.take().unwrap_or(Rung::Venv);
                    install_rx = None;
                    install_cancel = None;
                    let next = next_applicable_rung(rung, &facts);
                    state = Screen::Failed {
                        rung,
                        reason: "installer task ended unexpectedly".to_owned(),
                        next,
                    };
                    continue;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }

        tokio::select! {
            maybe = reader.next() => {
                match maybe {
                    Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        if quit_confirm {
                            match quit_decision(key) {
                                QuitDecision::Confirm => {
                                    if let Some(cancel) = &install_cancel {
                                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    return Ok(Flow::Quit);
                                }
                                QuitDecision::Dismiss => quit_confirm = false,
                                QuitDecision::Swallow => {}
                            }
                            continue;
                        }
                        let ctrl_c = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);

                        if let Screen::Installing { rung } = &state {
                            // During install: Esc = cancel install (child
                            // killed), Ctrl+C = quit anyway (child killed
                            // first via the cancel flag).
                            if ctrl_c {
                                if let Some(cancel) = &install_cancel {
                                    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                                }
                                return Ok(Flow::Quit);
                            }
                            match key.code {
                                KeyCode::Esc => {
                                    if let Some(cancel) = &install_cancel {
                                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    let rung = *rung;
                                    state = Screen::Offer {
                                        rung,
                                        note: Some("Install cancelled."),
                                    };
                                }
                                KeyCode::Char('q') => quit_confirm = true,
                                _ => {}
                            }
                            continue;
                        }

                        if ctrl_c {
                            quit_confirm = true; // ask before quitting
                            continue;
                        }
                        match key.code {
                            KeyCode::Char('r') | KeyCode::Char('R') => return Ok(Flow::Retry),
                            // The exit keys ask before quitting (same flow as
                            // the in-app quit confirmation).
                            KeyCode::Char('q') | KeyCode::Esc => quit_confirm = true,
                            KeyCode::Char('i') | KeyCode::Char('I') => {
                                if let Screen::Offer { rung, .. } = &state {
                                    let (rung, cancel, log, done) =
                                        start_install(&facts, *rung);
                                    install_rung = Some(rung);
                                    install_cancel = Some(cancel);
                                    install_log = Some(log);
                                    install_rx = Some(done);
                                    state = Screen::Installing { rung };
                                }
                            }
                            KeyCode::Char('p') | KeyCode::Char('P') => {
                                if let Screen::NoPython = &state {
                                    probe_rx = Some(spawn_probe(os));
                                    state = Screen::Probing;
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                if let Screen::Failed { rung, .. } = &state {
                                    if let Some(next) = next_applicable_rung(*rung, &facts) {
                                        let (rung, cancel, log, done) =
                                            start_install(&facts, next);
                                        install_rung = Some(rung);
                                        install_cancel = Some(cancel);
                                        install_log = Some(log);
                                        install_rx = Some(done);
                                        state = Screen::Installing { rung };
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        if let Some(cancel) = &install_cancel {
                            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        return Err(io::Error::other(e));
                    }
                    None => return Ok(Flow::Quit), // stream ended: nowhere to go
                }
            }
            _ = tick.tick() => {
                if let Screen::Intro = state {
                    state = Screen::Probing;
                }
                spinner = spinner.wrapping_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests;
