//! Ratatui layout: sidebar (chat list) + chat view + spinner line + input +
//! hotkey status line.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Connection};
use crate::markdown;
use crate::model::{ChatLifecycle, Role};
use crate::popup::ErrorPopup;
use crate::theme::Theme;

/// Prompt marker shown before the typed text / rename draft. `❯` (U+276F)
/// is a single-column glyph; the trailing space is part of the marker.
const PROMPT_MARKER: &str = "\u{276f} ";
const PROMPT_MARKER_WIDTH: u16 = 2;

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
    // feat_input_grow: the editor block grows per frame from 1 row up to
    // MAX_INPUT_LINES rows with the multiline draft (chat pane shrinks
    // correspondingly — Min(0) absorbs the rest). On tiny terminals the
    // block is additionally clamped so spinner + hints + a sliver of chat
    // always survive; saturating math everywhere keeps ≤4-row frames safe.
    let max_block_height = f.area().height.saturating_sub(4).max(1);
    let input_height = app.input_lines_height().min(max_block_height);
    let root = Layout::vertical([
        Constraint::Min(0),               // main area
        Constraint::Length(1),            // spinner / status line (above input)
        Constraint::Length(input_height), // growing editor block
        Constraint::Length(1),            // status line with hotkey hints
    ])
    .split(f.area());

    let main = Layout::horizontal([
        Constraint::Length(26), // sidebar
        Constraint::Min(20),    // chat view
    ])
    .split(root[0]);

    // The chat column starts at the chat pane's left edge (past the sidebar
    // divider) and excludes the 1-column right margin, so the prompt, tint
    // and hints stay inside cols 26..=118 @120 and never spill onto the
    // divider (col 25) or the margins.
    let chat_column = Rect {
        x: main[1].x,
        width: main[1].width.saturating_sub(1),
        ..root[2]
    };

    render_sidebar(f, app, theme, main[0]);
    render_chat(f, app, theme, main[1], root[1]);
    render_spinner_line(f, app, theme, root[1]);
    match &app.mode {
        Mode::Renaming { .. } => render_rename_line(f, app, theme, chat_column),
        Mode::Normal => render_input(f, app, theme, chat_column),
    }
    render_status(f, app, theme, root[3]);

    // Modal error popup overlays everything (rendered last).
    if app.error_popup.is_some() {
        render_error_popup(f, app, theme);
    }
}

use crate::app::Mode;

/// Connection status indicator for the status line.
///
/// Returns `(label, color)`. The request-lifecycle spinner (`queued…` /
/// `thinking…` above the input) stays the single source of truth for
/// per-request progress — this indicator only reflects the backend link.
fn connection_status(app: &App) -> (&'static str, ratatui::style::Color) {
    match &app.connection {
        Connection::Connected => ("\u{25cf} connected", theme_green()),
        Connection::Connecting => ("\u{25cf} connecting\u{2026}", theme_yellow()),
        Connection::Disconnected => ("\u{25cf} disconnected (press R)", theme_red()),
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

/// Spinner + lifecycle label rendered just above the input box. Per-thread
/// async: reflects ONLY the ACTIVE chat — background chats' work is shown by
/// their sidebar dot, never by this line.
fn render_spinner_line(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let (label, color) = match app.active_lifecycle() {
        ChatLifecycle::Idle => return, // keep the line empty when idle
        ChatLifecycle::Awaiting { .. } => ("queued\u{2026}", theme.yellow),
        ChatLifecycle::Running { .. } => ("thinking\u{2026}", theme.purple),
    };
    let spinner = app.spinner_char();
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {spinner} {label}"),
            Style::new().fg(color),
        )),
        area,
    );
}

/// Sidebar list rows carry a per-chat lifecycle dot at column 0:
/// `\u{25cb}` idle (dim) · `\u{25c6}` awaiting/queued (yellow) ·
/// `\u{25cf}` running (green). The ACTIVE chat additionally keeps its name
/// bold + highlighted row.
fn render_sidebar(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let items: Vec<ListItem> = app
        .chats
        .iter()
        .enumerate()
        .map(|(i, chat)| {
            let selected = i == app.active;
            let dot_glyph = match (&chat.lifecycle, selected) {
                (ChatLifecycle::Running { .. }, _) => "\u{25cf}",
                (ChatLifecycle::Awaiting { .. }, _) => "\u{25c6}",
                (ChatLifecycle::Idle, true) => "\u{25cf}",
                (ChatLifecycle::Idle, false) => "\u{25cb}",
            };
            let dot_color = match &chat.lifecycle {
                ChatLifecycle::Running { .. } => theme.green,
                ChatLifecycle::Awaiting { .. } => theme.yellow,
                ChatLifecycle::Idle => {
                    if selected {
                        theme.green
                    } else {
                        theme.dim
                    }
                }
            };
            let name_style = if selected {
                Style::new().fg(theme.fg).add_modifier(Modifier::BOLD)
            } else {
                // Multi-line titles collapse to spaces inside the one-row
                // sidebar entry (feat_shift_enter_newline allows `\n` in
                // names).
                Style::new().fg(theme.dim)
            };
            let name = chat.name.replace('\n', " ");
            ListItem::new(Line::from(vec![
                Span::styled(format!("{dot_glyph} "), Style::new().fg(dot_color)),
                Span::styled(name, name_style),
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

    // Extend the sidebar's vertical border down through EVERY bottom row —
    // spinner + the grown editor block (feat_input_grow, 1..=20 rows) +
    // hotkey hints — so the divider runs unbroken from the top edge to the
    // status line at any editor height.
    let below = Rect {
        x: area.right().saturating_sub(1),
        y: area.bottom(),
        width: 1,
        height: f.area().bottom().saturating_sub(area.bottom()),
    };
    for y in below.top()..below.bottom() {
        f.buffer_mut()[(below.x, y)]
            .set_symbol("\u{2502}")
            .set_style(Style::new().fg(theme.selection).bg(theme.panel));
    }
}

fn render_chat(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect, spinner_line: Rect) {
    let title = app.chat_title().replace('\n', " ");
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

    app.chat_visible_rows = inner.height;

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

/// feat_input_visual + feat_input_grow: the editor block is tinted with the
/// input-panel background across EVERY of its rows and carries a cyan `❯`
/// marker on its FIRST row. The typed text renders INSIDE the remaining
/// columns over the full block height — tui-textarea keeps the cursor
/// visible inside that viewport automatically, scrolling the LAST visible
/// row toward the caret once the buffer exceeds [`MAX_INPUT_LINES`] lines.
fn render_input(f: &mut Frame, app: &mut App, theme: &Theme, chat_column: Rect) {
    // Panel tint confined to the CHAT COLUMN (cols 26..width @120): the
    // divider cell and the sidebar strip keep `theme.panel` (approved
    // feat_input_visual geometry).
    f.buffer_mut()
        .set_style(chat_column, Style::new().bg(theme.input_panel_bg));

    let send_label = "\u{23ce} send ";
    let has_visible_text = app.input.lines().iter().any(|l| !l.is_empty());

    if !has_visible_text {
        // Blank draft: placeholder + `⏎ send` chip composed in ONE line on
        // the block's FIRST row so they share the same baseline. Width math
        // measures DISPLAY columns (not byte lengths): `❯`/`⏎` are
        // multi-byte UTF-8 but single-column glyphs — measuring bytes drifts
        // the chip left.
        let marker_w = PROMPT_MARKER_WIDTH;
        let placeholder_len = "Type a message\u{2026}".width() as u16;
        let send_len = send_label.width() as u16;
        // Gap computed against the CHAT COLUMN width (the render target): the
        // chip must hug the panel's right edge.
        let gap = chat_column
            .width
            .saturating_sub(marker_w + placeholder_len + send_len) as usize;
        let line = Line::from(vec![
            Span::styled(PROMPT_MARKER, Style::new().fg(theme.cyan)),
            Span::styled("Type a message\u{2026}", Style::new().fg(theme.dim)),
            Span::raw(" ".repeat(gap)),
            Span::styled(send_label, Style::new().fg(theme.selection)),
        ]);
        f.render_widget(
            Paragraph::new(line).style(Style::new().bg(theme.input_panel_bg)),
            chat_column,
        );
        return;
    }

    // First: the `❯` marker cell ON THE FIRST ROW ONLY…
    let marker_area = Rect {
        x: chat_column.x,
        width: PROMPT_MARKER_WIDTH.min(chat_column.width),
        height: 1,
        ..chat_column
    };
    if marker_area.width > 0 {
        f.render_widget(
            Paragraph::new(Span::styled(PROMPT_MARKER, Style::new().fg(theme.cyan)))
                .style(Style::new().bg(theme.input_panel_bg)),
            marker_area,
        );
    }
    // …then the textarea over the remaining columns of ALL rows (its own
    // widget paints cursor position etc. and handles caret-following scroll).
    let rest = Rect {
        x: chat_column.x + PROMPT_MARKER_WIDTH,
        width: chat_column.width.saturating_sub(PROMPT_MARKER_WIDTH),
        ..chat_column
    };
    if rest.width > 0 && rest.height > 0 {
        f.render_widget(&app.input, rest);
    }
    // Grown block repeats feat_input_visual's chip on the FIRST row (never
    // the last), flush right; a first-line tail under it clips — same
    // graceful-degradation rule as the rename hint.
    if chat_column.height > 1 {
        let chip_w = send_label.width() as u16;
        if chat_column.width > chip_w {
            let chip_area = Rect {
                x: chat_column.x + chat_column.width - chip_w,
                y: chat_column.y,
                width: chip_w,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    send_label,
                    Style::new().fg(theme.selection),
                )))
                .style(Style::new().bg(theme.input_panel_bg)),
                chip_area,
            );
        }
    }
}

/// Rename editor shares the feat_input_visual treatment: same `❯` marker
/// and panel tint, with the save/cancel hint right-aligned on the FIRST row.
/// feat_input_grow: a multiline draft (Shift+Enter / pasted `\n`s) renders
/// its continuation lines onto the rows below on the shared panel tint; the
/// block height is driven by `App::input_lines_height`, so the layout and
/// this renderer stay in lockstep. Long lines clip under the hint.
fn render_rename_line(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let draft = app.rename_buf().unwrap_or_default();
    let rename_label = "\u{270e} Rename thread\u{2026} ";
    let hint = "\u{23ce} save \u{00b7} esc cancel ";
    let hint_len = hint.width() as u16;

    // First physical line carries the label + first draft line + hint;
    // continuation lines fill the rows below as plain foreground text.
    let mut parts = draft.split('\n');
    let head = parts.next().unwrap_or_default();

    let used = (PROMPT_MARKER_WIDTH as usize + rename_label.width() + head.width())
        .min(u16::MAX as usize) as u16;
    // The hint is right-aligned within the chat column (the divider cell to
    // its right is excluded by the caller).
    let usable = area.width.saturating_sub(hint_len);
    let gap = usable.saturating_sub(used);

    let mut lines = Vec::with_capacity(area.height.max(1) as usize);
    lines.push(Line::from(vec![
        Span::styled(PROMPT_MARKER, Style::new().fg(theme.cyan)),
        Span::styled(rename_label, Style::new().fg(theme.yellow)),
        Span::styled(head.to_owned(), Style::new().fg(theme.fg)),
        Span::raw(" ".repeat(gap as usize)),
        Span::styled(hint, Style::new().fg(theme.selection)),
    ]));
    for cont in parts {
        lines.push(Line::from(Span::styled(
            cont.to_owned(),
            Style::new().fg(theme.fg),
        )));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::new().bg(theme.input_panel_bg)),
        area,
    );
}

fn render_status(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let (status_label, status_color) = connection_status(app);
    // feat_rename_thread: `^R rename` joined the hints. `/jk` was dropped —
    // single-letter navigation was removed earlier, so the old label was
    // stale; dropping it keeps the line within one row even with the longest
    // status label (`● disconnected (press R)`).
    // feat_shift_enter_newline: `⇧↵ nl` documents multi-line input; `^C
    // cancel/quit` shortened to `^C cancel` so everything still fits 120
    // columns with that longest label.
    let spans = vec![
        Span::styled(
            "   \u{2191}\u{2193} chats \u{00b7} ^N new \u{00b7} PgUp/PgDn scroll \u{00b7} ^R rename \u{00b7} ^C cancel \u{00b7} ^L clear \u{00b7} ^V paste \u{00b7} \u{21e7}\u{21b5} nl",
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
    use crate::app::{App, Chat};
    use crate::mock;
    use crate::model::ChatLifecycle;
    use crate::theme::Theme;
    use ratatui::backend::TestBackend;

    /// Render the full UI offscreen at an arbitrary resolution and return
    /// the plain-text cell grid plus a buffer snapshot for per-cell style
    /// assertions (fg/bg colors).
    fn render_grid_at_with_buffer(
        app: &mut App,
        width: u16,
        height: u16,
    ) -> (Vec<String>, ratatui::buffer::Buffer) {
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, app, &Theme::tokyo_night()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let rows = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        (rows, buf)
    }

    /// Demo-resolution variant (120×34).
    fn render_grid_with_buffer(app: &mut App) -> (Vec<String>, ratatui::buffer::Buffer) {
        render_grid_at_with_buffer(app, 120, 34)
    }

    /// Render the full UI offscreen and return the plain-text cell grid.
    fn render_grid(app: &mut App) -> Vec<String> {
        render_grid_with_buffer(app).0
    }
    // ---- feat_input_grow --------------------------------------------------

    /// Vertical band layout, bottom-up at frame height `H` with editor block
    /// height `h`: status hints `H-1`, band `[H-1-h ..= H-2]`, spinner row
    /// directly above the band.
    fn input_band(h: u16, frame_h: u16) -> std::ops::RangeInclusive<u16> {
        (frame_h - 1 - h)..=(frame_h - 2)
    }

    /// Shift+Enter growth: a 3-line draft occupies exactly 3 rows of the
    /// bottom band (one draft line per row), chat pane shrinks upward, and
    /// clearing the input collapses the block back to a single placeholder
    /// row. Helper and grid must agree at every step.
    #[test]
    fn input_block_grows_to_three_rows_and_collapses_back() {
        let mut app = App::new(vec![Chat::new("chat")]);
        assert_eq!(app.input_lines_height(), 1);

        app.input.insert_str("one\ntwo\nthree");
        assert_eq!(app.input_lines_height(), 3);
        let rows = render_grid(&mut app);
        let h = 3u16;
        let band = input_band(h, rows.len() as u16).collect::<Vec<_>>();
        assert!(
            rows[band[0] as usize].contains("❯") && rows[band[0] as usize].contains("one"),
            "first band row must carry marker + head line: {:?}",
            rows[band[0] as usize]
        );
        assert!(
            rows[band[1] as usize].contains("two"),
            "second band row must show draft line 2: {:?}",
            rows[band[1] as usize]
        );
        assert!(
            rows[band[2] as usize].contains("three"),
            "third band row must show draft line 3: {:?}",
            rows[band[2] as usize]
        );
        // Chat pane header still rendered above the grown block.
        assert!(rows[0].contains("#1/1"));
        // Status hints pinned below the band.
        assert!(rows.last().unwrap().contains("^N new"));

        // Collapse back to exactly one placeholder row.
        app.clear_input();
        assert_eq!(app.input_lines_height(), 1);
        let rows = render_grid(&mut app);
        let last = rows.len();
        assert!(
            rows[last - 2].contains("Type a message") && rows[last - 2].contains("⏎ send"),
            "placeholder row must be back at len-2: {:?}",
            rows[last - 2]
        );
        assert!(
            !rows[last - 4..last - 2].iter().any(|r| r.contains("three")),
            "no stale draft line may linger in former band rows"
        );
    }

    /// Sidebar divider (`│`, col 25) runs unbroken through spinner + grown
    /// editor block + status line for EVERY height 2..=20; the margin cell
    /// past the chat column stays empty on each band row.
    #[test]
    fn sidebar_divider_unbroken_at_every_growth_height() {
        for h in [2u16, 3, 5, 10, 19, 20] {
            let mut app = App::new(vec![Chat::new("chat")]);
            for _ in 0..(h - 1) {
                app.input.insert_str("\n");
            }
            app.input.insert_str("x");
            assert_eq!(app.input_lines_height(), h);

            let (_, buf) = render_grid_with_buffer(&mut app);
            for y in input_band(h, buf.area.height) {
                let cell = &buf[(25, y)];
                assert_eq!(
                    cell.symbol(),
                    "\u{2502}",
                    "height {h}: divider missing at y{y}"
                );
                assert_ne!(
                    cell.bg,
                    Theme::tokyo_night().input_panel_bg,
                    "height {h}: divider cell tinted at y{y}"
                );
                assert_eq!(
                    buf[(119, y)].symbol(),
                    " ",
                    "height {h}: margin cell polluted at y{y}"
                );
            }
        }
    }

    /// The grown block repeats the `⏎ send` chip on its FIRST row only,
    /// flush right with the chat column (ends col 118 @120).
    #[test]
    fn multiline_send_chip_sits_on_first_row_flush_right_only() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.input.insert_str("aa\nbb"); // height 2
        assert_eq!(app.input_lines_height(), 2);

        let rows = render_grid(&mut app);
        let band = input_band(2, rows.len() as u16).collect::<Vec<_>>();
        let first_row = &rows[band[0] as usize];
        let second_row = &rows[band[1] as usize];

        let chip_col = col_of_sub(first_row, "⏎ send").expect("chip missing on first row");
        assert_eq!(
            chip_col + "⏎ send ".width(),
            119,
            "chip must end flush with chat column edge"
        );
        assert!(
            !second_row.contains("⏎ send"),
            "chip must not repeat on later rows: {second_row:?}"
        );

        // Exactly ONE chip glyph sequence anywhere in the frame (hints bar
        // uses ⇧↵, never ⏎ send).
        let total: usize = rows.iter().map(|r| r.matches("⏎ send").count()).sum();
        assert_eq!(total, 1, "chip must appear exactly once per frame");
    }

    /// Panel tint spans EVERY row of the grown block inside the chat column
    /// only — divider cell, sidebar strip and right margin stay untinted.
    #[test]
    fn grown_block_tints_each_row_inside_chat_column_only() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("chat")]);
        app.input.insert_str("aa\nbb\ncc"); // height 3
        assert_eq!(app.input_lines_height(), 3);

        let (_, buf) = render_grid_with_buffer(&mut app);
        for y in input_band(3, buf.area.height) {
            for x in [26u16, 60, 100, 118] {
                assert_eq!(
                    buf[(x, y)].bg,
                    theme.input_panel_bg,
                    "tint missing at ({x},{y})"
                );
            }
            assert_ne!(buf[(25, y)].bg, theme.input_panel_bg, "divider tinted");
            assert_ne!(buf[(0, y)].bg, theme.input_panel_bg, "sidebar tinted");
            assert_ne!(buf[(119, y)].bg, theme.input_panel_bg, "margin tinted");
        }
    }

    /// Cap + auto-scroll: a 23-line draft caps the block at MAX_INPUT_LINES
    /// (20) rows; the viewport keeps the caret's LAST lines visible while
    /// the earliest lines scroll out above.
    #[test]
    fn textarea_view_follows_caret_beyond_twenty_line_cap() {
        let mut app = App::new(vec![Chat::new("chat")]);
        let lines: Vec<String> = (0..23).map(|i| format!("zzq{i:02}")).collect();
        app.input.insert_str(lines.join("\n"));
        assert_eq!(app.input_lines_height(), crate::app::MAX_INPUT_LINES as u16);

        let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 50);
        let flat = rows.join("\n");

        assert!(
            flat.contains("zzq22"),
            "caret line (last draft line) must be visible"
        );
        assert!(
            flat.contains("zzq21") && flat.contains("zzq03"),
            "trailing viewport window must hold recent lines"
        );
        for lost in ["zzq00", "zzq01", "zzq02"] {
            assert!(!flat.contains(lost), "{lost} should have scrolled out");
        }
    }

    /// Terminals too small for 20 rows + chrome must not panic: the block is
    /// clamped to what fits (spinner + hints + sliver of chat survive) and
    /// repeated draws stay stable.
    #[test]
    fn grown_input_never_panics_on_tiny_terminal() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("chat")]);
        for _ in 0..30 {
            app.input.insert_str("long\n");
        }
        let (rows, buf) = render_grid_at_with_buffer(&mut app, 80, 12);
        assert!(
            rows.last().unwrap().contains("^N new"),
            "status hints must survive the clamp"
        );
        // Block clamped to 12-4 = 8 rows: band top lands at H-1-8 = 3.
        for y in 4..=10 {
            assert_eq!(
                buf[(26, y)].bg,
                theme.input_panel_bg,
                "clamped band row {y} must still be tinted"
            );
        }
        // Second draw after viewport state exists — still no panic.
        let _ = render_grid_at_with_buffer(&mut app, 80, 12);
    }

    /// Rename mode grows equally: a multiline title draft renders its first
    /// line with label + hint and continuation lines below, hint only once.
    #[test]
    fn rename_editor_grows_and_renders_multiline_draft() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.begin_rename();
        for c in "ab\ncd".chars() {
            app.rename_push(c);
        }
        assert_eq!(app.input_lines_height(), 2);

        let rows = render_grid(&mut app);
        let band = input_band(2, rows.len() as u16).collect::<Vec<_>>();
        assert!(
            rows[band[0] as usize].contains("✎ Rename thread…")
                && rows[band[0] as usize].contains("ab")
                && rows[band[0] as usize].contains("esc cancel"),
            "first rename row must keep label/draft/hint: {:?}",
            rows[band[0] as usize]
        );
        assert!(
            rows[band[1] as usize].contains("cd"),
            "continuation line missing: {:?}",
            rows[band[1] as usize]
        );
        assert!(
            !rows[band[1] as usize].contains("esc cancel"),
            "hint must not repeat below the first row"
        );
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

    /// Display column of the FIRST occurrence of `needle` in `row`.
    /// (Unlike `str::find`, this measures display columns, not byte offsets —
    /// `❯` is 3 UTF-8 bytes but 1 column.)
    fn col_of_sub(row: &str, needle: &str) -> Option<usize> {
        let chars: Vec<char> = row.chars().collect();
        let pat: Vec<char> = needle.chars().collect();
        (0..chars.len()).find(|&i| chars[i..].starts_with(&pat))
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

    // ---- per-thread async: render-level lifecycle checks -----------------

    /// Render the full UI and return ONLY the spinner line (3rd row from the
    /// bottom in the 120×34 demo grid: above input + status hotkey line).
    /// NOTE: the sidebar's vertical border extension crosses every bottom
    /// row at its column — strip it so emptiness checks are meaningful.
    fn spinner_row_of(app: &mut App) -> String {
        let mut rows = render_grid(app);
        rows.remove(rows.len() - 3)
    }

    fn spinner_line_of(app: &mut App) -> String {
        spinner_row_of(app)
            .chars()
            .filter(|&c| c != '\u{2502}')
            .collect()
    }

    /// Sidebar dots must encode the PER-CHAT lifecycle: ○ idle (dim),
    /// ◆ awaiting/queued (yellow), ● running (green) at column 0 of each
    /// chat's sidebar row. The active chat here is the queued one, which
    /// also proves selection still works while busy.
    #[test]
    fn sidebar_dots_distinguish_lifecycle_states() {
        let theme = Theme::tokyo_night();
        let idle = Chat::new("idle");
        let mut queued = Chat::new("queued");
        queued.lifecycle = ChatLifecycle::Awaiting {
            request_id: "req-queued".into(),
        };
        let mut running = Chat::new("running");
        running.lifecycle = ChatLifecycle::Running {
            request_id: "req-running".into(),
        };
        let mut app = App::new(vec![idle, queued, running]);
        app.active = 1;

        let (rows, buf) = render_grid_with_buffer(&mut app);

        // Sidebar list starts at the top-left corner: one row per chat, the
        // lifecycle dot occupying column 0.
        let expected = [
            ("\u{25cb}", theme.dim),    // idle → hollow dim dot
            ("\u{25c6}", theme.yellow), // awaiting → yellow diamond
            ("\u{25cf}", theme.green),  // running → green filled dot
        ];
        let names = ["idle", "queued", "running"];
        for (i, (glyph, color)) in expected.iter().enumerate() {
            // Sidebar block renders its " Chats " title on grid row 0; the
            // per-chat list rows start one row below.
            let y = i + 1;
            assert!(
                rows[y].contains(names[i]),
                "sidebar row {y} must list chat {}, got {:?}",
                names[i],
                rows[y]
            );
            let cell = &buf[(0, y as u16)];
            assert_eq!(
                cell.symbol(),
                *glyph,
                "sidebar row {y}: wrong lifecycle glyph"
            );
            assert_eq!(cell.fg, *color, "sidebar row {y}: wrong dot color");
        }
    }

    /// The spinner line mirrors ONLY the active chat's lifecycle: visible
    /// with its label while THIS chat is queued/running, hidden when the
    /// active chat idles — even if a background chat is still working.
    #[test]
    fn spinner_line_shows_and_hides_per_active_lifecycle() {
        let mut app = App::new(vec![Chat::new("solo")]);

        // Idle: line empty.
        assert!(
            spinner_line_of(&mut app).trim().is_empty(),
            "idle chat must keep the spinner line empty"
        );

        // Awaiting: queued… visible.
        app.chats[0].lifecycle = ChatLifecycle::Awaiting {
            request_id: "req-a".into(),
        };
        let line = spinner_line_of(&mut app);
        assert!(
            line.contains("queued") && line.contains(app.spinner_char()),
            "awaiting chat shows spinner + queued…, got {line:?}"
        );

        // Running: thinking… replaces queued….
        app.chats[0].lifecycle = ChatLifecycle::Running {
            request_id: "req-a".into(),
        };
        let line = spinner_line_of(&mut app);
        assert!(
            line.contains("thinking"),
            "running chat shows thinking…, got {line:?}"
        );

        // Back to idle: hidden again.
        app.chats[0].lifecycle = ChatLifecycle::Idle;
        assert!(spinner_line_of(&mut app).trim().is_empty());
    }

    /// Background work must NOT light up the spinner line of an idle active
    /// chat (per-thread isolation at RENDER level, not just state level).
    #[test]
    fn spinner_line_ignores_background_chat_activity() {
        let mut busy = Chat::new("busy");
        busy.lifecycle = ChatLifecycle::Running {
            request_id: "req-bg".into(),
        };
        let mut app = App::new(vec![Chat::new("focused"), busy]);
        app.active = 0;

        let line = spinner_line_of(&mut app);
        assert!(
            line.trim().is_empty(),
            "background chat must not spin the active chat's line, got {line:?}"
        );
    }

    // ---- feat_input_visual -----------------------------------------------

    /// While renaming, the prompt line must show the ✎ rename editor — the
    /// regular `❯`-marked prompt (placeholder or typed draft) must be fully
    /// hidden so the two editors never mix on one baseline.
    #[test]
    fn prompt_hidden_while_renaming() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.begin_rename();
        let rows = render_grid(&mut app);

        // Input line = 2nd row from the bottom.
        let input_row = &rows[rows.len() - 2];
        assert!(
            input_row.contains("Rename thread"),
            "rename editor must replace the prompt, got {input_row:?}"
        );
        assert!(
            !input_row.contains("Type a message"),
            "prompt placeholder must NOT leak into the rename line: {input_row:?}"
        );
    }

    /// The `❯` prompt marker must sit before the placeholder on an empty
    /// input, and before the typed text once the user types.
    #[test]
    fn input_marker_renders_before_placeholder_and_text() {
        let mut app = App::new(vec![Chat::new("chat")]);

        // Empty → marker at the chat-column start, dim placeholder after it.
        let rows = render_grid(&mut app);
        let input_row = &rows[rows.len() - 2];
        let marker_col = col_of(input_row, '❯').expect("marker missing");
        let ph_col = col_of_sub(input_row, "Type a message").expect("placeholder missing");
        assert_eq!(
            marker_col + PROMPT_MARKER_WIDTH as usize,
            ph_col,
            "marker must sit directly before the placeholder, got {input_row:?}"
        );

        // Typed text → same marker position, text after it, no placeholder.
        app.input.insert_str("hello");
        let rows = render_grid(&mut app);
        let input_row = &rows[rows.len() - 2];
        let marker_col = col_of(input_row, '❯').expect("marker missing with text");
        assert!(
            input_row.contains("hello"),
            "typed text missing: {input_row:?}"
        );
        assert!(
            !input_row.contains("Type a message"),
            "placeholder must hide once typing starts: {input_row:?}"
        );
        let text_col = col_of_sub(input_row, "hello").unwrap();
        assert_eq!(
            marker_col + PROMPT_MARKER_WIDTH as usize,
            text_col,
            "text must start right after `❯ `, got {input_row:?}"
        );
    }

    /// Empty input: the `⏎ send` chip shares the placeholder's baseline,
    /// hugging the RIGHT edge of the CHAT COLUMN (one cell before the
    /// sidebar divider). Chip starts at display column 112 and ends at 118.
    #[test]
    fn send_chip_right_aligned_on_placeholder_baseline() {
        let mut app = App::new(vec![Chat::new("chat")]);
        let rows = render_grid(&mut app);
        let row = &rows[rows.len() - 2];

        assert!(row.contains("⏎ send"), "chip missing: {row:?}");
        assert!(
            row.contains("Type a message…"),
            "placeholder missing: {row:?}"
        );

        // Chip occupies exactly the 7 display columns ending at column 118
        // (divider at 119 untouched).
        let chip_col = col_of_sub(row, "⏎ send").expect("chip missing");
        assert_eq!(chip_col, 112, "unexpected chip position: {row:?}");

        // Same-baseline guarantee: pure padding between placeholder end and
        // the chip.
        let ph_end = col_of_sub(row, "Type a message…").unwrap() + "Type a message…".width();
        let polluted: Vec<char> = row
            .chars()
            .skip(ph_end)
            .take(chip_col - ph_end)
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            polluted.is_empty(),
            "baseline polluted between placeholder and chip: {polluted:?}"
        );
    }

    // ---- feat_rename_thread: render-level checks -------------------------

    /// The rename editor shows label + current draft on one baseline with
    /// the save/cancel hint right-aligned at the panel edge.
    #[test]
    fn rename_editor_renders_draft_with_right_aligned_hint() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.begin_rename();
        app.rename_push('X');
        app.rename_push('Y');
        let rows = render_grid(&mut app);
        let input_row = &rows[rows.len() - 2];

        assert!(input_row.contains("✎ Rename thread…"), "got {input_row:?}");
        assert!(input_row.contains("XY"), "draft missing: {input_row:?}");
        assert!(
            input_row.contains("\u{23ce} save \u{00b7} esc cancel"),
            "save/cancel hint missing: {input_row:?}"
        );

        // Right-aligned: the hint's last glyph sits at display column 118 —
        // the final cell of the chat column (0..=118); divider at 119 stays.
        let hint_end =
            col_of_sub(input_row, "esc cancel ").expect("hint missing") + "esc cancel ".width();
        assert_eq!(hint_end, 119, "hint not flush with the panel edge");
    }

    /// Status hints line lists the thread tools AND the multi-line hint;
    /// `^C cancel` stays readable next to the longest connection label.
    #[test]
    fn status_line_lists_rename_and_newline_hints() {
        let mut app = App::new(mock::initial_chats());
        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();

        for needle in ["^R rename", "\u{21e7}\u{21b5} nl", "^C cancel"] {
            assert!(last.contains(needle), "{needle} missing from {last:?}");
        }
    }

    /// feat_input_visual: the input row carries the panel tint background —
    /// but ONLY inside the chat column (cols 26..=118 @120). The divider cell
    /// (col 25) and the sidebar strip (cols 0..=24) must NOT get the tint.
    #[test]
    fn input_row_has_panel_tint_background() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("chat")]);

        let (_, buf) = render_grid_with_buffer(&mut app);
        let y = buf.area.height - 2;
        assert_ne!(
            buf[(0, y)].bg,
            theme.input_panel_bg,
            "sidebar strip must NOT get the input tint"
        );
        assert_eq!(
            buf[(26, y)].bg,
            theme.input_panel_bg,
            "empty input row must carry the panel tint from the chat-column start"
        );
        assert_eq!(
            buf[(118, y)].bg,
            theme.input_panel_bg,
            "tint must run to the chat column's right edge"
        );
        assert_ne!(
            buf[(25, y)].bg,
            theme.input_panel_bg,
            "divider cell must NOT get the input tint"
        );

        app.input.insert_str("tinted");
        let (_, buf) = render_grid_with_buffer(&mut app);
        let y = buf.area.height - 2;
        assert_eq!(
            buf[(60, y)].bg,
            theme.input_panel_bg,
            "typed-over input row must keep the panel tint"
        );
    }

    // ---- feat_input_visual: restored canonical render-level checks ------

    /// Exactly ONE `│` glyph on the input row, at the sidebar-border column
    /// (col 25), keeping its own bg — NOT the input tint. Input tint confined
    /// to sampled chat-column cells; nothing left of the divider is tinted.
    #[test]
    fn sidebar_divider_unbroken_through_input_row() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("chat")]);
        let (_, buf) = render_grid_with_buffer(&mut app);
        let y = buf.area.height - 2;

        let divider_cols: Vec<u16> = (0..buf.area.width)
            .filter(|&x| buf[(x, y)].symbol() == "\u{2502}")
            .collect();
        assert_eq!(divider_cols, vec![25], "exactly one divider cell");

        let divider = &buf[(25, y)];
        assert_eq!(divider.fg, theme.selection, "divider fg");
        assert_ne!(
            divider.bg, theme.input_panel_bg,
            "divider cell must NOT carry the input tint"
        );

        for x in [26u16, 60, 100, 112] {
            assert_eq!(
                buf[(x, y)].bg,
                theme.input_panel_bg,
                "tint confined to the chat column at col {x}"
            );
        }
        assert_ne!(
            buf[(0, y)].bg,
            theme.input_panel_bg,
            "nothing left of the divider may be tinted"
        );
    }

    /// Rename line shares the feat_input_visual treatment: `❯` marker at the
    /// chat-column start AND `input_panel_bg` tint across the chat column;
    /// the divider cell stays untinted.
    #[test]
    fn rename_line_shares_marker_and_panel_tint() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("chat")]);
        app.begin_rename();

        let (_, buf) = render_grid_with_buffer(&mut app);
        let y = buf.area.height - 2;

        assert_eq!(
            buf[(26, y)].symbol(),
            "\u{276f}",
            "marker must sit at the chat-column start in rename mode too"
        );
        assert_eq!(
            buf[(60, y)].bg,
            theme.input_panel_bg,
            "rename line must carry the input-panel tint mid-row"
        );
        assert_ne!(
            buf[(25, y)].bg,
            theme.input_panel_bg,
            "rename tint must not leak onto the divider cell"
        );
    }

    /// Empty input: `⏎ send` chip hugs the RIGHT edge of the chat column.
    /// Math invariant: chip_start + chip_width == CHAT_X0 + chat_column.width
    /// → starts at col 112, ends flush at 118, zero non-whitespace between
    /// placeholder end and chip start, margin cell past the column untouched.
    #[test]
    fn send_chip_right_edge_and_baseline_math_with_marker() {
        const CHAT_X0: u16 = 26;
        let theme = Theme::tokyo_night();
        let chat_column_width = |buf_area_width: u16| -> u16 { buf_area_width - 1 - CHAT_X0 };

        let mut app = App::new(vec![Chat::new("chat")]);
        let (_, buf) = render_grid_with_buffer(&mut app);
        let y = buf.area.height - 2;
        let row = &render_grid(&mut app)[y as usize];

        assert!(row.contains("⏎ send"), "chip missing: {row:?}");

        let chip_col = col_of_sub(row, "⏎ send").expect("chip missing") as u16;
        let ccw = chat_column_width(buf.area.width);
        assert_eq!(
            chip_col + "\u{23ce} send ".width() as u16,
            CHAT_X0 + ccw,
            "chip must END flush with the chat column's right edge"
        );
        // Margin cell past the chat column stays empty and untinted.
        assert_eq!(buf[(119, y)].symbol(), " ", "margin cell past the column");
        assert_ne!(buf[(119, y)].bg, theme.input_panel_bg);

        // Same-baseline guarantee: pure padding between placeholder end and
        // the chip start.
        let ph_end = col_of_sub(row, "Type a message…").unwrap() + "Type a message…".width();
        let polluted: Vec<char> = row
            .chars()
            .skip(ph_end)
            .take((chip_col as usize) - ph_end)
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            polluted.is_empty(),
            "baseline polluted between placeholder and chip: {polluted:?}"
        );
    }
}
