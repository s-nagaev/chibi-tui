//! Ratatui layout: sidebar (chat list) + chat view + status line + input.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Connection};
use crate::markdown;
use crate::model::{ChatStatus, Role};
use crate::popup::ErrorPopup;
use crate::theme::Theme;

/// Compute how many lines must be hidden from the TOP of the message list.
///
/// Scroll semantics: `scroll` counts lines scrolled UP FROM THE BOTTOM
/// (`0` = follow-bottom mode), so the number of lines hidden from the top
/// is what REMAINS: `max_scroll - scroll`.
pub fn scroll_skip(scroll: u16, at_bottom: bool, total: usize, visible: u16) -> usize {
    let visible = visible.max(1) as usize;
    let max_scroll = total.saturating_sub(visible);
    if at_bottom {
        return max_scroll;
    }
    let scrolled = (scroll as usize).min(max_scroll);
    max_scroll - scrolled
}

/// Draw one full frame.
pub fn draw(f: &mut Frame, app: &mut App, theme: &Theme) {
    let root = Layout::vertical([
        Constraint::Min(0),    // main area
        Constraint::Length(1), // spinner / status line (above input, per mockup)
        Constraint::Length(1), // input
        Constraint::Length(1), // status line with hotkey hints
    ])
    .split(f.area());

    let main = Layout::horizontal([
        Constraint::Length(26), // sidebar
        Constraint::Min(20),    // chat view
    ])
    .split(root[0]);

    render_sidebar(f, app, theme, main[0]);
    render_chat(f, app, theme, main[1], root[1]);
    render_spinner_line(f, app, theme, root[1]);
    render_input(f, app, theme, root[2]);
    render_status(f, app, theme, root[3]);

    // Modal error popup overlays everything (rendered last).
    if app.error_popup.is_some() {
        render_error_popup(f, app, theme);
    }
}

/// Connection status indicator for the status line.
///
/// Returns `(label, color)`. The request-lifecycle spinner (`queued…` /
/// `thinking…` above the input) stays the single source of truth for
/// per-request progress — this indicator only reflects the backend link.
fn connection_status(app: &App) -> (&'static str, ratatui::style::Color) {
    match &app.connection {
        Connection::Connected => ("● connected", theme_green()),
        Connection::Connecting => ("● connecting…", theme_yellow()),
        Connection::Disconnected => ("● disconnected (press R)", theme_red()),
    }
}

/// Theme colors without pulling the whole `Theme` into the signature (the
/// indicator palette is fixed Tokyo Night accent set).
fn theme_green() -> ratatui::style::Color {
    crate::theme::Theme::tokyo_night().green
}

fn theme_yellow() -> ratatui::style::Color {
    crate::theme::Theme::tokyo_night().yellow
}

fn theme_red() -> ratatui::style::Color {
    crate::theme::Theme::tokyo_night().red
}

/// Spinner + lifecycle label rendered just above the input box.
fn render_spinner_line(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    if app.status == ChatStatus::Idle {
        return; // keep the line empty when nothing is in flight
    }
    let spinner = app.spinner_char();
    let (label, color) = match app.status {
        ChatStatus::Queued => ("queued…", theme.yellow),
        ChatStatus::Running => ("thinking…", theme.purple),
        ChatStatus::Idle => unreachable!("handled above"),
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {spinner} {label}"),
            Style::new().fg(color),
        )),
        area,
    );
}

fn render_sidebar(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let items: Vec<ListItem> = app
        .chats
        .iter()
        .enumerate()
        .map(|(i, chat)| {
            let selected = i == app.active;
            let dot = if selected { "\u{25cf}" } else { "\u{25cb}" };
            let name = if selected {
                Span::styled(
                    chat.name.clone(),
                    Style::new().fg(theme.fg).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(chat.name.clone(), Style::new().fg(theme.dim))
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{dot} "),
                    Style::new().fg(if selected { theme.green } else { theme.dim }),
                ),
                name,
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(Style::new().fg(theme.selection))
                .title(Span::styled(
                    " Chats ",
                    Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
                ))
                .style(Style::new().bg(theme.panel)),
        )
        .highlight_style(Style::new().bg(theme.selection))
        .style(Style::new().bg(theme.panel));

    let mut state = ListState::default().with_selected(Some(app.active));
    f.render_stateful_widget(list, area, &mut state);

    // Extend the sidebar's vertical border down through the input zone so
    // the divider runs unbroken from the top edge to the status line.
    let below = Rect {
        x: area.right().saturating_sub(1),
        y: area.bottom(),
        width: 1,
        height: 3.min(f.area().bottom().saturating_sub(area.bottom())),
    };
    for y in below.top()..below.bottom() {
        f.buffer_mut()[(below.x, y)]
            .set_symbol("\u{2502}")
            .set_style(Style::new().fg(theme.selection).bg(theme.panel));
    }
}

fn render_chat(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect, spinner_line: Rect) {
    let title = app.chat_title();
    let block = Block::default()
        .title(Span::styled(
            format!(
                " #{}/{} \u{00b7} {} ",
                app.active + 1,
                app.chats.len(),
                title
            ),
            Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::TOP)
        .border_style(Style::new().fg(theme.selection));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(chat) = app.chats.get(app.active) else {
        return;
    };

    // Render every message into lines.
    let mut lines: Vec<markdown::MdLine> = Vec::new();
    for msg in &chat.messages {
        match msg.role {
            Role::User => {
                lines.push(Line::from(Span::styled(
                    "\u{25cf} You",
                    Style::new().fg(theme.orange).add_modifier(Modifier::BOLD),
                )));
            }
            Role::Assistant => {
                lines.push(Line::from(Span::styled(
                    "\u{25cf} Chibi",
                    Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
                )));
            }
        }
        if msg.pending {
            lines.push(Line::from(Span::styled(String::new(), Style::new())));
        } else {
            lines.extend(markdown::render(&msg.markdown, theme));
        }
        lines.push(Line::from(""));
    }

    let total = lines.len();
    let visible = inner.height;
    let skip = scroll_skip(app.scroll, app.at_bottom(), total, visible);
    let overflowed = total > visible.max(1) as usize;

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .scroll((skip as u16, 0));
    f.render_widget(paragraph, inner);

    if overflowed {
        render_scroll_hint(f, app.at_bottom(), theme, spinner_line);
    }
}

/// `↑ more ↓` hint tucked into the right end of the spinner/status line
/// above the input. Rendered only when chat content overflows.
fn render_scroll_hint(f: &mut Frame, at_bottom: bool, theme: &Theme, spinner_line: Rect) {
    let label = if at_bottom {
        "\u{2191} more "
    } else {
        "\u{2191}\u{2193} more "
    };
    let width = label.width() as u16;
    if spinner_line.width > width {
        let hint_area = Rect {
            x: spinner_line.x + spinner_line.width - width,
            y: spinner_line.y,
            width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(label, Style::new().fg(theme.dim))),
            hint_area,
        );
    }
}

fn render_input(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let has_text = !app.input.lines().iter().all(|l| l.is_empty());
    if has_text {
        let p = Paragraph::new(app.input.lines().join("\n")).style(Style::new().fg(theme.fg));
        f.render_widget(p, area);
    } else {
        // Placeholder and the `send` chip are composed in ONE line so they
        // are guaranteed to share the same baseline.
        let placeholder_len = "Type a message…".width() as u16;
        // `⏎` (U+23CE) is 3 bytes in UTF-8 but 1 display column — measure
        // display width, not byte length, or the chip drifts left.
        let send_label = "\u{23ce} send ";
        let send_len = send_label.width() as u16;
        let gap = area.width.saturating_sub(placeholder_len + send_len) as usize;
        let line = Line::from(vec![
            Span::styled("Type a message…", Style::new().fg(theme.dim)),
            Span::raw(" ".repeat(gap)),
            Span::styled(send_label, Style::new().fg(theme.selection)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }
}

fn render_status(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let (status_label, status_color) = connection_status(app);
    let spans = vec![
        Span::styled(
            "   \u{2191}\u{2193}/jk chats \u{00b7} N new \u{00b7} PgUp/PgDn scroll \u{00b7} ^C cancel/quit \u{00b7} ^L clear \u{00b7} ^V paste",
            Style::new().fg(theme.selection),
        ),
        Span::raw("  "),
        Span::styled(status_label, Style::new().fg(status_color)),
    ];

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(theme.panel)),
        area,
    );
}

/// Centered modal error popup over a dimmed backdrop. Purely visual — all
/// key handling lives in `main.rs`.
fn render_error_popup(f: &mut Frame, app: &mut App, theme: &Theme) {
    use ratatui::widgets::{Clear, Padding};

    let Some(popup) = app.error_popup.as_ref() else {
        return;
    };
    let message = popup.message.clone();

    // Size to content with sane caps; centered on the full frame.
    let width = (message.width() as u16 + ErrorPopup::hint().width() as u16 + 6)
        .clamp(40, f.area().width.saturating_sub(4))
        .max(20);
    let height = 5.min(f.area().height.saturating_sub(2)).max(3);
    let x = f.area().x + (f.area().width.saturating_sub(width)) / 2;
    let y = f.area().y + (f.area().height.saturating_sub(height)) / 2;
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme.red))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            " Backend error ",
            Style::new().fg(theme.red).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height > 0 {
        // Wrap the message manually so long lines never overflow the box.
        let lines = wrap_text(&message, inner.width.max(1) as usize);
        let hint = Line::from(Span::styled(
            ErrorPopup::hint(),
            Style::new().fg(theme.yellow),
        ));
        let mut all: Vec<Line<'static>> = lines;
        all.push(hint); // rendered last; clipped when out of room
        let text = Text::from(all);
        let visible = inner.height as usize;
        let skip = text.height().saturating_sub(visible);
        let paragraph = Paragraph::new(text).scroll((skip as u16, 0));
        f.render_widget(paragraph, inner);
    }
}

/// Greedy word-wrap at display-width boundaries; never splits words.
fn wrap_text(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let max_width = max_width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        for word in paragraph.split(' ') {
            let candidate_len = if current.is_empty() {
                word.width()
            } else {
                current.width() + 1 + word.width()
            };
            if candidate_len <= max_width {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            } else {
                if !current.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current)));
                }
                // A single over-long word is hard-split rather than lost.
                let mut rest = word;
                while rest.width() > max_width {
                    let mut take = 0usize;
                    let mut w = 0usize;
                    for ch in rest.chars() {
                        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                        if w + cw > max_width {
                            break;
                        }
                        w += cw;
                        take += ch.len_utf8();
                    }
                    lines.push(Line::from(rest[..take].to_owned()));
                    rest = &rest[take..];
                }
                current = rest.to_owned();
            }
        }
        lines.push(Line::from(current));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::mock;
    use crate::theme::Theme;
    use ratatui::backend::TestBackend;

    /// Render the full UI offscreen at the demo resolution and return the
    /// plain-text cell grid.
    fn render_grid(app: &mut App) -> Vec<String> {
        let backend = TestBackend::new(120, 34);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, app, &Theme::tokyo_night()))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Display column of the RIGHTMOST occurrence of `needle` in `row`.
    fn col_of(row: &str, needle: char) -> Option<usize> {
        let mut col = 0;
        let mut found = None;
        for c in row.chars() {
            if c == needle {
                found = Some(col);
            }
            col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        }
        found
    }

    /// End-to-end smoke test: at 120x34 every code panel must be a closed
    /// rectangle — right border present on EVERY body row, aligned with the
    /// corners, never wrapped or clipped by the layout.
    #[test]
    fn full_ui_code_panels_are_closed_rectangles() {
        // Chat 1 has python panels, chat 2 the widest rust panel.
        for active in [0usize, 1] {
            let mut app = App::new(mock::initial_chats());
            app.active = active;
            let rows = render_grid(&mut app);
            let tops: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.contains('\u{256d}'))
                .map(|(y, _)| y)
                .collect();
            assert!(!tops.is_empty(), "chat {active}: no code panel rendered");
            for t in tops {
                let b = (t + 1..rows.len())
                    .find(|&y| rows[y].contains('\u{2570}'))
                    .unwrap_or_else(|| {
                        panic!("chat {active}: bottom border missing under top at y{t}")
                    });
                let rc_top = col_of(&rows[t], '\u{256e}')
                    .unwrap_or_else(|| panic!("chat {active}: ╮ missing on top row y{t}"));
                let rc_bot = col_of(&rows[b], '\u{256f}')
                    .unwrap_or_else(|| panic!("chat {active}: ╯ missing on bottom row y{b}"));
                assert_eq!(
                    rc_top, rc_bot,
                    "chat {active} panel y{t}..{b}: right border column drifts"
                );
                for (dy, row) in rows[t + 1..b].iter().enumerate() {
                    let y = t + 1 + dy;
                    let n = row.matches('\u{2502}').count();
                    assert!(
                        n >= 2,
                        "chat {active} body row y{y}: only {n} borders — panel open: {row:?}"
                    );
                    let right = col_of(row, '\u{2502}').unwrap();
                    assert_eq!(
                        right, rc_top,
                        "chat {active} body row y{y}: right border at {right}, expected {rc_top}"
                    );
                }
            }
        }
    }

    // ---- ux_polish -------------------------------------------------------

    /// The status bar must always carry the connection indicator.
    #[test]
    fn status_bar_shows_connection_indicator() {
        let mut app = App::new(mock::initial_chats());

        app.connection = Connection::Connected;
        let rows = render_grid(&mut app);
        let last = rows.last().unwrap();
        assert!(last.contains("connected"), "got {last:?}");
        assert!(!last.contains("disconnected"));

        app.connection = Connection::Connecting;
        assert!(render_grid(&mut app).last().unwrap().contains("connecting"));

        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();
        assert!(
            last.contains("disconnected") && last.contains("press R"),
            "got {last:?}"
        );
    }

    /// With an error popup set, a bordered box with the message and the
    /// recovery hint must be rendered on top of the UI.
    #[test]
    fn error_popup_renders_message_and_hint_over_ui() {
        let mut app = App::new(mock::initial_chats());
        app.show_error("backend process died unexpectedly");
        let rows = render_grid(&mut app);

        let flat: String = rows.join("\n");
        assert!(
            flat.contains("Backend error"),
            "popup title missing:\n{flat}"
        );
        assert!(
            flat.contains("backend process died"),
            "popup message missing"
        );
        assert!(flat.contains("R reconnect"), "recovery hint missing");

        // Popup is centered and boxed: its top border row exists.
        assert!(
            rows.iter().any(|r| r.contains('┌') && r.contains('┐')),
            "no closed top border found"
        );
    }

    /// No popup — no overlay artifacts anywhere.
    #[test]
    fn no_popup_when_dismissed() {
        let mut app = App::new(mock::initial_chats());
        app.show_error("boom");
        app.dismiss_error();
        let flat = render_grid(&mut app).join("\n");
        assert!(!flat.contains("Backend error"));
    }

    #[test]
    fn wrap_text_respects_width_and_keeps_words() {
        let lines = wrap_text("aaa bbb ccc ddd", 7);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content, "aaa bbb");
        assert_eq!(lines[1].spans[0].content, "ccc ddd");
    }

    #[test]
    fn wrap_text_hard_splits_overlong_words() {
        let lines = wrap_text(&"x".repeat(30), 10);
        assert_eq!(lines.len(), 3);
        for l in &lines {
            assert!(l.spans[0].content.width() <= 10);
        }
    }

    #[test]
    fn wrap_text_handles_empty_and_multiline() {
        assert_eq!(wrap_text("", 10).len(), 1);
        let two = wrap_text("one\ntwo", 10);
        assert_eq!(two.len(), 2);
        assert_eq!(two[1].spans[0].content, "two");
    }

    #[test]
    fn connection_status_maps_states_to_labels() {
        let mut app = App::new(Vec::new());
        app.connection = Connection::Connected;
        let (label, _) = connection_status(&app);
        assert_eq!(label, "● connected");
        app.connection = Connection::Connecting;
        let (label, _) = connection_status(&app);
        assert_eq!(label, "● connecting…");
        app.connection = Connection::Disconnected;
        let (label, _) = connection_status(&app);
        assert_eq!(label, "● disconnected (press R)");
    }
}
