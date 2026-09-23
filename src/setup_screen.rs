//! Startup screen for a missing backend binary.
//!
//! Live mode spawns the `chibi` backend from PATH. When that binary is not
//! there (a fresh `cargo install chibi-tui` without the backend), a generic
//! error popup does not really help. This screen names the problem and lists
//! copy-friendly install commands for the current OS, plus the
//! `CHIBI_BACKEND_BIN` escape hatch for a binary that already exists
//! somewhere else.
//!
//! The screen runs before the main event loop starts, so `r` needs no extra
//! wiring: the caller just re-enters its connect attempt.

use std::ffi::OsStr;
use std::io;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use ratatui::{Frame, Terminal};

use crate::backend_client::BackendError;
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

/// Outcome of the screen loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// `r`: re-attempt the backend spawn.
    Retry,
    /// `q`/Esc/Ctrl+C, confirmed via the shared quit confirmation: leave
    /// the app.
    Quit,
}

/// Full screen content for `os`, styled with the app theme. Command lines
/// stay on their own lines so a terminal copy-paste grabs them cleanly.
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
        Line::from(Span::styled("  pipx install chibi", cmd)),
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

/// Draw one setup frame: theme background everywhere plus a centered
/// bordered block sized to the content (same shape as the modal popups).
/// `quit_confirm` overlays the shared "Quit chibi-tui?" popup (the same
/// renderer the in-app confirm uses, the same shared grammar — see
/// [`crate::app::quit_decision`]).
pub fn draw(f: &mut Frame, theme: &Theme, quit_confirm: bool) {
    use unicode_width::UnicodeWidthStr;

    let area = f.area();
    f.render_widget(Block::default().style(Style::new().bg(theme.bg)), area);

    let lines = content_lines(TargetOs::current(), theme);
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
        .border_style(Style::new().fg(theme.orange))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            " Backend setup ",
            Style::new().fg(theme.orange).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines), inner);

    if quit_confirm {
        crate::ui::render_quit_confirm(f, theme);
    }
}

/// Run the setup screen loop.
///
/// Returns [`Flow::Retry`] for `r` (the caller re-attempts the spawn) and
/// [`Flow::Quit`] for a CONFIRMED `q`/Esc/Ctrl+C: the exit keys open the
/// shared quit-confirmation popup first (`y`/Enter quits, `n`/Esc/`q`
/// returns to the screen; the same grammar as the in-app confirm and the
/// splash). Other keys and mouse/resize events only trigger a redraw.
pub async fn run(terminal: &mut Tui, reader: &mut EventStream, theme: &Theme) -> io::Result<Flow> {
    let mut quit_confirm = false;
    loop {
        terminal.draw(|f| draw(f, theme, quit_confirm))?;
        let maybe_event = reader.next().await;
        match maybe_event {
            Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                if quit_confirm {
                    match crate::app::quit_decision(key) {
                        crate::app::QuitDecision::Confirm => return Ok(Flow::Quit),
                        crate::app::QuitDecision::Dismiss => quit_confirm = false,
                        crate::app::QuitDecision::Swallow => {}
                    }
                    continue;
                }
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl_c {
                    quit_confirm = true; // ask before quitting
                } else {
                    match key.code {
                        KeyCode::Char('r') | KeyCode::Char('R') => return Ok(Flow::Retry),
                        // The exit keys ask before quitting (same flow as
                        // the in-app quit confirmation).
                        KeyCode::Char('q') | KeyCode::Esc => quit_confirm = true,
                        _ => {}
                    }
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(io::Error::other(e)),
            None => return Ok(Flow::Quit), // stream ended: nowhere to go
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten styled lines into one plain string for substring checks.
    fn flat(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn unix_content_lists_pipx_and_venv_commands() {
        let text = flat(&content_lines(TargetOs::Unix, &Theme::tokyo_night()));
        assert!(text.contains("not found on PATH"), "got: {text}");
        assert!(text.contains("pipx install chibi"));
        assert!(text.contains("python3 -m venv ~/.local/share/chibi-tui/venv"));
        assert!(text.contains("~/.local/share/chibi-tui/venv/bin/pip install chibi-bot"));
        assert!(text.contains("export CHIBI_BACKEND_BIN=~/.local/share/chibi-tui/venv/bin/chibi"));
        assert!(text.contains("export CHIBI_BACKEND_BIN=/path/to/chibi"));
    }

    #[test]
    fn windows_content_lists_py_and_venv_commands() {
        let text = flat(&content_lines(TargetOs::Windows, &Theme::tokyo_night()));
        assert!(text.contains(r"py -3 -m venv %USERPROFILE%"), "got: {text}");
        assert!(text.contains(r"\Scripts\pip install chibi-bot"));
        assert!(text.contains(r"set CHIBI_BACKEND_BIN=%USERPROFILE%"));
        assert!(text.contains(r"set CHIBI_BACKEND_BIN=C:\path\to\chibi.exe"));
        // pipx works on Windows too, so it stays on that page as well.
        assert!(text.contains("pipx install chibi"));
    }

    #[test]
    fn both_variants_carry_the_backend_bin_escape_hatch() {
        for os in [TargetOs::Unix, TargetOs::Windows] {
            let text = flat(&content_lines(os, &Theme::tokyo_night()));
            assert!(text.contains("Already have a backend?"), "os: {os:?}");
            assert!(text.contains("CHIBI_BACKEND_BIN"), "os: {os:?}");
        }
    }

    #[test]
    fn hint_names_retry_and_quit_keys() {
        let text = flat(&content_lines(TargetOs::Unix, &Theme::tokyo_night()));
        assert!(text.contains("r retry"));
        assert!(text.contains("q/Esc quit"));
    }

    #[test]
    fn applies_only_to_spawn_errors_without_an_override() {
        let spawn = BackendError::Spawn {
            program: "chibi".to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        let exec_failed = BackendError::UnexpectedExit {
            status: crate::backend_client::ExitStatusDisplay(1),
        };
        assert!(applies(&spawn, None));
        assert!(!applies(&spawn, Some(OsStr::new("/bin/false"))));
        // A binary that exists but fails right after exec is a different
        // story: the popup must keep the real error text.
        assert!(!applies(&exec_failed, None));
    }

    /// The quit-confirm overlay renders over the setup screen without
    /// panicking and carries the shared popup text (same renderer and
    /// grammar as the in-app confirm).
    #[test]
    fn quit_confirm_overlay_renders_over_the_setup_screen() {
        use ratatui::{backend::TestBackend, Terminal};
        let theme = Theme::tokyo_night();
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| draw(f, &theme, true)).unwrap();
        let buf = terminal.backend().buffer();
        let flat: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            flat.contains("Quit chibi-tui?"),
            "the confirm popup must overlay the setup screen"
        );
        assert!(
            flat.contains("not found on PATH"),
            "setup content stays underneath"
        );
    }
}
