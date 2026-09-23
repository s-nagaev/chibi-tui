use super::super::*;

pub(super) use crate::app::Chat;
pub(super) use crate::mock;
pub(super) use crate::model::Message;
pub(super) use crate::protocol::AgentEventKind;
pub(super) use ratatui::backend::TestBackend;

/// Render the full UI offscreen at an arbitrary resolution and return
/// the plain-text cell grid plus a buffer snapshot for per-cell style
/// assertions (fg/bg colors).
pub(super) fn render_grid_at_with_buffer(
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
pub(super) fn render_grid_with_buffer(app: &mut App) -> (Vec<String>, ratatui::buffer::Buffer) {
    render_grid_at_with_buffer(app, 120, 34)
}

/// Render the full UI offscreen and return the plain-text cell grid.
pub(super) fn render_grid(app: &mut App) -> Vec<String> {
    render_grid_with_buffer(app).0
}

pub(super) fn plain_line(s: &str) -> markdown::MdLine {
    Line::from(s.to_owned())
}

/// Display column of the RIGHTMOST occurrence of `needle` in `row`.
pub(super) fn col_of(row: &str, needle: char) -> Option<usize> {
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
pub(super) fn col_of_sub(row: &str, needle: &str) -> Option<usize> {
    let chars: Vec<char> = row.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    (0..chars.len()).find(|&i| chars[i..].starts_with(&pat))
}
