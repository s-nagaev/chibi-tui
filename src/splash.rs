//! Splash screen: ASCII calico-kitten logo shown for ~1.5s at startup.
//!
//! The logo is hand-built 35x20 pixel art (one char = one pixel). Every
//! character maps to a palette entry; the oversized cyan eyes are a direct
//! nod to the original Chibi mascot. Pure Unicode/ASCII only — no Nerd Font
//! glyphs, so it renders identically in any monospace terminal.
//!
//! Any keypress skips the splash; Esc/Ctrl+C open the shared quit
//! confirmation before the app exits.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{Frame, Terminal};

use crate::theme::Theme;

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Minimum time the splash stays visible (any keypress skips the remainder).
const MIN_HOLD: Duration = Duration::from_millis(500);

/// Logo grid width in columns.
const LOGO_WIDTH: u16 = 35;

/// Calico kitten, front view: oversized head (~2x body width), tall pointed
/// ears (orange patch left, pink inner right), huge cyan eyes with white
/// glints and dark bottom corners, blush cheeks, muzzle with pink nose and
/// omega-shaped mouth, small body with orange calico spot and toe dips, and a
/// thick vertical tail with orange rings rising along the right side, tip
/// level with the ears.
const LOGO: &[&str] = &[
    "    ##                  ##",
    "   #oo#                #pp#",
    "  #oooo#              #pppp#",
    " ############################",
    " #oooooooooccccccccccccccccc#   ###",
    " #ccwweeeeeccccccccwweeeeecc#  #oo#",
    " #ccwweeeeeccccccccwweeeeecc#  #oo#",
    " #cceeeeeeecccccccceeeeeeecc#  #cc#",
    "t#cckeeeeekcccccccckeeeeekcc#  #cc#",
    "t#bbcccccccmmmmmmmmcccccccbb#  #cc#",
    "t#cccccccccmmmnnmmmccccccccc#  #oo#",
    "t#cccccccccmddmmddmccccccccc#  #to#",
    " #cccccccccccccccccccccccccc#  #cc#",
    " ############################  #cc#",
    "        #cccccccccccc#         #cc#",
    "        #cccccccccooc#         #cc#",
    "        #cccccccccccc#          #c#",
    "        #cccdccccdccc#          #c#",
    "        #cccccccccccc#          #c#",
    "        ##############          ###",
];

/// Map one art character to its color (`None` = theme background).
///
/// Accents (`cyan`, `orange`, `dim`) come from the app theme; the rest are
/// fixed fur palette entries tuned for the Tokyo Night dark background.
fn color_for(c: char, theme: &Theme) -> Option<Color> {
    match c {
        '#' => Some(Color::Rgb(158, 105, 70)), // thick dark-brown outline
        'o' => Some(theme.orange),             // light-orange calico patches
        'p' => Some(Color::Rgb(255, 202, 187)), // right ear inner pink
        'c' => Some(Color::Rgb(242, 227, 201)), // cream fur
        'w' => Some(Color::Rgb(255, 255, 255)), // eye glints
        'e' => Some(theme.cyan),               // the signature turquoise eyes
        'k' => Some(Color::Rgb(22, 64, 95)),   // dark eye corners
        'm' => Some(Color::Rgb(250, 241, 222)), // muzzle
        'b' => Some(Color::Rgb(255, 183, 166)), // cheek blush
        'n' => Some(Color::Rgb(255, 143, 163)), // nose
        'd' => Some(Color::Rgb(122, 74, 47)),  // mouth / toe dips
        't' => Some(theme.dim),                // whisker/glitch strips
        _ => None,
    }
}

/// Build the colored logo lines by merging same-color char runs into spans.
fn logo_lines(theme: &Theme) -> Vec<Line<'static>> {
    LOGO.iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            let mut run = String::new();
            let mut run_color: Option<Color> = None;
            for c in row.chars() {
                let col = color_for(c, theme);
                if col != run_color {
                    if !run.is_empty() {
                        spans.push(span_of(std::mem::take(&mut run), run_color));
                    }
                    run_color = col;
                }
                run.push(c);
            }
            if !run.is_empty() {
                spans.push(span_of(run, run_color));
            }
            Line::from(spans)
        })
        .collect()
}

fn span_of(text: String, color: Option<Color>) -> Span<'static> {
    match color {
        Some(fg) => Span::styled(text, Style::new().fg(fg)),
        None => Span::raw(text),
    }
}

/// Full splash content: logo + product line + tagline + hint.
fn content_lines(theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = logo_lines(theme);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("chibi-tui v{}", env!("CARGO_PKG_VERSION")),
        Style::new().fg(theme.fg).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "your tiny AI companion",
        Style::new().fg(theme.dim),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "press any key",
        Style::new().fg(theme.selection),
    )));
    lines
}

/// Draw one splash frame: theme background everywhere + centered content.
/// `quit_confirm` overlays the shared "Quit chibi-tui?" popup (the same
/// renderer the in-app confirm uses, the same shared grammar — see
/// [`crate::app::quit_decision`]).
pub fn draw(f: &mut Frame, theme: &Theme, quit_confirm: bool) {
    let area = f.area();
    f.render_widget(Block::default().style(Style::new().bg(theme.bg)), area);

    let lines = content_lines(theme);
    let w = LOGO_WIDTH;
    let h = lines.len() as u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w.min(area.width),
        height: h.min(area.height),
    };
    f.render_widget(Paragraph::new(lines), rect);

    if quit_confirm {
        crate::ui::render_quit_confirm(f, theme);
    }
}

/// Run the splash loop.
///
/// Returns `Ok(true)` when the main UI should start (timeout or skip key),
/// `Ok(false)` when the user aborted (Esc/Ctrl+C, confirmed via the shared
/// quit-confirmation popup: `y`/Enter quits, `n`/Esc/`q` returns to the
/// splash). While the confirmation is open the hold deadline is suspended —
/// a confirm opened in the last moments of the splash must never silently
/// become a "skip into the app".
pub async fn run(terminal: &mut Tui, reader: &mut EventStream, theme: &Theme) -> io::Result<bool> {
    let deadline = Instant::now() + MIN_HOLD;
    let mut quit_confirm = false;
    loop {
        terminal.draw(|f| draw(f, theme, quit_confirm))?;

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() && !quit_confirm {
            return Ok(true);
        }

        // While the confirmation is open the timeout is parked: only the
        // decision keys move the state, the hold can't expire underneath.
        let hold = if quit_confirm {
            tokio::time::sleep(Duration::from_secs(3600))
        } else {
            tokio::time::sleep(remaining)
        };
        tokio::select! {
            maybe_event = reader.next() => match maybe_event {
                Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                    if quit_confirm {
                        match crate::app::quit_decision(key) {
                            crate::app::QuitDecision::Confirm => return Ok(false),
                            crate::app::QuitDecision::Dismiss => quit_confirm = false,
                            crate::app::QuitDecision::Swallow => {}
                        }
                    } else {
                        let ctrl_c = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if key.code == KeyCode::Esc || ctrl_c {
                            quit_confirm = true; // ask before aborting
                        } else {
                            return Ok(true); // any other key skips the splash
                        }
                    }
                }
                Some(Ok(_)) => {} // mouse/resize etc: redraw on next iteration
                Some(Err(e)) => return Err(io::Error::other(e)),
                None => return Ok(true), // stream ended; fall through to UI
            },
            _ = hold => {
                // Only reachable without the confirm (it parks the timer);
                // the hold expired — start the main UI.
                return Ok(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn logo_grid_is_consistent() {
        assert_eq!(LOGO.len(), 20, "logo must be 20 rows tall");
        let max_w = LOGO.iter().map(|l| l.width()).max().unwrap();
        assert_eq!(max_w, LOGO_WIDTH as usize, "widest row defines the grid");
        assert!(
            LOGO.iter().all(|l| l.width() <= LOGO_WIDTH as usize),
            "no row may exceed the declared grid width"
        );
    }

    #[test]
    fn every_glyph_has_a_palette_entry() {
        let theme = Theme::tokyo_night();
        for row in LOGO {
            for c in row.chars() {
                if c.is_whitespace() {
                    continue; // background
                }
                assert!(
                    color_for(c, &theme).is_some(),
                    "unmapped glyph {c:?} would render invisible"
                );
            }
        }
    }

    #[test]
    fn ascii_only_no_exotic_unicode() {
        for row in LOGO {
            assert!(
                row.is_ascii(),
                "logo must stay pure ASCII (no tofu-prone glyphs)"
            );
        }
    }

    #[test]
    fn content_has_branding_lines() {
        let theme = Theme::tokyo_night();
        let lines = content_lines(&theme);
        assert_eq!(lines.len(), 20 + 5, "logo + spacers + name/tagline/hint");
    }

    /// The quit-confirm overlay renders over the splash without panicking
    /// and carries the shared popup text (same renderer as the in-app
    /// confirm, per the shared-grammar contract).
    #[test]
    fn quit_confirm_overlay_renders_over_the_splash() {
        use ratatui::{backend::TestBackend, Terminal};
        let theme = Theme::tokyo_night();
        // Tall enough that the splash hint sits BELOW the centered popup.
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| draw(f, &theme, true)).unwrap();
        let buf = terminal.backend().buffer();
        let flat: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            flat.contains("Quit chibi-tui?"),
            "the confirm popup must overlay the splash"
        );
        assert!(flat.contains("press any key"), "splash stays underneath");
    }
}
