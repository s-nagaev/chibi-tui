//! Ratatui layout: sidebar (chat list) + chat view + spinner line + input +
//! hotkey status line.

use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::block::Title;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Connection, Focus, ModelPickerPhase, StopResetAction};
use crate::markdown;
use crate::model::{ChatLifecycle, Role};
use crate::popup::ErrorPopup;
use crate::protocol::Usage;
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

/// Panel rectangles of one frame, exactly as [`draw`] lays them out.
///
/// The mouse-event router in `main.rs` reuses this geometry to hit-test
/// which panel the cursor is over, so rendering and hit-testing can never
/// disagree. Layout is recomputed per frame / per event — there is no
/// cached geometry anywhere in the render path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutRects {
    /// Sidebar pane (chat list), left column of the main area.
    pub sidebar: Rect,
    /// Chat view pane, right of the sidebar divider.
    pub chat: Rect,
    /// Root rows: `[0]` main area, `[1]` spinner line, `[2]` input block,
    /// `[3]` hotkey-hints line.
    pub root: [Rect; 4],
}

impl LayoutRects {
    /// Chat text column: starts at the chat pane's left edge and excludes
    /// the 1-column right margin, so the prompt, tint and hints stay
    /// inside e.g. cols 26..=118 @120 and never spill onto the divider
    /// (col 25) or the margins.
    pub fn chat_column(&self) -> Rect {
        Rect {
            x: self.chat.x,
            width: self.chat.width.saturating_sub(1),
            ..self.root[2]
        }
    }
}

/// Split `area` exactly like [`draw`] does: the root vertical stack (main
/// area / spinner line / growing input block / hotkey-hints line) plus the
/// sidebar+chat horizontal split of the main area. `input_height` is the
/// RAW editor-block height (see `App::input_lines_height`); the tiny-frame
/// clamp is applied here, shared by the renderer and the mouse router.
pub fn layout_rects(area: Rect, input_height: u16) -> LayoutRects {
    // the editor block grows per frame from 1 row up to
    // MAX_INPUT_LINES rows with the multiline draft (chat pane shrinks
    // correspondingly: Min(0) absorbs the rest). On tiny terminals the
    // block is additionally clamped so spinner + hints + a sliver of chat
    // always survive; saturating math everywhere keeps ≤4-row frames safe.
    let max_block_height = area.height.saturating_sub(4).max(1);
    let input_height = input_height.min(max_block_height);
    let root = Layout::vertical([
        Constraint::Min(0),               // main area
        Constraint::Length(1),            // spinner / status line (above input)
        Constraint::Length(input_height), // growing editor block
        Constraint::Length(1),            // status line with hotkey hints
    ])
    .split(area);

    let main = Layout::horizontal([
        Constraint::Length(26), // sidebar
        Constraint::Min(20),    // chat view
    ])
    .split(root[0]);

    LayoutRects {
        sidebar: main[0],
        chat: main[1],
        root: [root[0], root[1], root[2], root[3]],
    }
}

/// Which on-screen panel a point falls into — the shared hit-test seam for
/// mouse routing (wheel scrolling now, text selection later). Modal popups
/// are NOT regions: they own the whole screen when open, and the router
/// dispatches on the app `Mode` before consulting geometry at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelRegion {
    /// The sidebar (chat list) column.
    Sidebar,
    /// The chat view column (right of the sidebar divider).
    Chat,
    /// Anywhere else (spinner / input / hints rows, margins).
    Other,
}

/// Hit-test a terminal point against the frame layout. The two panels do
/// not overlap (horizontal split), so the check order is irrelevant.
pub fn panel_region(rects: &LayoutRects, column: u16, row: u16) -> PanelRegion {
    if rects.sidebar.contains(Position { x: column, y: row }) {
        PanelRegion::Sidebar
    } else if rects.chat.contains(Position { x: column, y: row }) {
        PanelRegion::Chat
    } else {
        PanelRegion::Other
    }
}

/// Draw one full frame.
pub fn draw(f: &mut Frame, app: &mut App, theme: &Theme) {
    let rects = layout_rects(f.area(), app.input_lines_height());
    let chat_column = rects.chat_column();

    render_sidebar(f, app, theme, rects.sidebar);
    render_chat(f, app, theme, rects.chat, rects.root[1]);
    render_spinner_line(f, app, theme, rects.root[1]);
    match &app.mode {
        Mode::Renaming { .. } => render_rename_line(f, app, theme, chat_column),
        // While the confirm, a search or the log viewer popup is open the
        // underlying editor keeps rendering as the
        // normal input row (the popup overlays it and captures all keys).
        Mode::Normal
        | Mode::ConfirmDelete
        | Mode::ConfirmStopReset { .. }
        | Mode::Searching { .. }
        | Mode::SearchingAll { .. }
        | Mode::LogViewer { .. }
        | Mode::ModelPicking { .. }
        | Mode::HelpViewing { .. } => render_input(f, app, theme, chat_column),
    }
    render_status(f, app, theme, rects.root[3]);

    // Modal error popup overlays everything (rendered last).
    if app.error_popup.is_some() {
        render_error_popup(f, app, theme);
    }
    // confirm popup overlays everything (rendered last).
    if matches!(app.mode, Mode::ConfirmDelete) {
        render_delete_popup(f, app, theme);
    }
    // stop/reset confirm popup (rendered last).
    if matches!(app.mode, Mode::ConfirmStopReset { .. }) {
        render_stop_reset_popup(f, app, theme);
    }
    // search popup overlays everything (rendered last).
    if matches!(app.mode, Mode::Searching { .. }) {
        render_search_popup(f, app, theme);
    }
    // all-threads search popup (rendered last).
    if matches!(app.mode, Mode::SearchingAll { .. }) {
        render_search_all_popup(f, app, theme);
    }
    // diagnostics log viewer (rendered last).
    if matches!(app.mode, Mode::LogViewer { .. }) {
        render_log_viewer(f, app, theme);
    }
    // model picker popup (rendered last).
    if matches!(app.mode, Mode::ModelPicking { .. }) {
        render_model_picker(f, app, theme);
    }
    // keybindings help modal (rendered last).
    if matches!(app.mode, Mode::HelpViewing { .. }) {
        render_help_modal(f, app, theme);
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
/// their sidebar dot, never by this line. When the active chat reports live
/// subagents, a ` · subagents working: n` segment is appended. That segment
/// is independent of the request lifecycle: background subagents outlive
/// their turn's result frame, so an idle chat with live subagents renders
/// the counter alone (no spinner, no label); without any the line stays
/// byte-for-byte as before.
fn render_spinner_line(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let subagents = app.active_chat_subagents();
    let (label, color) = match app.active_lifecycle() {
        ChatLifecycle::Idle => {
            // Keep the request indicator empty while idle; only a live
            // subagent count still has something to show here.
            let Some(active) = subagents else {
                return;
            };
            let text = format!(" \u{00b7} subagents working: {active}");
            f.render_widget(
                Paragraph::new(Span::styled(text, Style::new().fg(theme.green))),
                area,
            );
            return;
        }
        ChatLifecycle::Awaiting { .. } => ("queued\u{2026}", theme.yellow),
        ChatLifecycle::Running { .. } => ("thinking\u{2026}", theme.purple),
    };
    let spinner = app.spinner_char();
    let mut text = format!(" {spinner} {label}");
    if let Some(active) = subagents {
        text.push_str(&format!(" \u{00b7} subagents working: {active}"));
    }
    f.render_widget(
        Paragraph::new(Span::styled(text, Style::new().fg(color))),
        area,
    );
}

/// Sidebar list rows carry a per-chat lifecycle dot at column 0:
/// `\u{25cb}` idle (dim) · `\u{25c6}` awaiting/queued (yellow) ·
/// `\u{25cf}` running (green). The ACTIVE chat additionally keeps its name
/// bold + highlighted row.
///
/// an inactive thread with an unseen reply
/// lights its idle dot in the `unread_activity` slot and renders its name
/// bold, but gets NO highlight (the thread stays visually inactive). The
/// three dot roles (active marker, unread activity, resting default) are
/// theme slots, so a future theme swap remaps them in one place.
///
/// the sidebar advertises keyboard ownership when
/// `app.focus == Focus::Sidebar` — theme-driven emphasis ONLY, no new
/// palette: border and ` Chats ` title switch from their resting colors to
/// a brighter accent pair, and the idle unselected dot column brightens
/// (dim → fg). Busy dots keep their semantic colors either way.
fn render_sidebar(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let sidebar_focused = app.focus == Focus::Sidebar;
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
                        // Selected idle dot: the active-marker slot, in
                        // both focuses.
                        theme.active_marker
                    } else if chat.unread {
                        // Unseen reply on an inactive thread: the
                        // unread-activity slot wins over the focus
                        // brightening below, the signal must survive.
                        theme.unread_activity
                    } else if sidebar_focused {
                        // Focus affordance: the dot column brightens while
                        // the sidebar owns the keyboard.
                        theme.fg
                    } else {
                        // Resting dot of a read thread: the default slot.
                        theme.dot_default
                    }
                }
            };
            let name_style = if selected {
                Style::new().fg(theme.fg).add_modifier(Modifier::BOLD)
            } else if chat.unread {
                // Unread thread: the name goes bold but keeps the dim
                // inactive color, no highlight rides along.
                Style::new().fg(theme.dim).add_modifier(Modifier::BOLD)
            } else {
                // Multi-line titles collapse to spaces inside the one-row
                // sidebar entry (Shift+Enter allows `\n` in
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
                .border_style(Style::new().fg(if sidebar_focused {
                    // Emphasis: focused sidebar's divider lifts to blue.
                    theme.blue
                } else {
                    theme.selection
                }))
                .title(Span::styled(
                    " Chats ",
                    Style::new()
                        .fg(if sidebar_focused {
                            // Emphasis: title switches from blue to cyan.
                            theme.cyan
                        } else {
                            theme.blue
                        })
                        .add_modifier(Modifier::BOLD),
                ))
                .style(Style::new().bg(theme.panel)),
        )
        .highlight_style(Style::new().bg(theme.selection))
        .style(Style::new().bg(theme.panel));

    let mut state = ListState::default().with_selected(Some(app.active));
    f.render_stateful_widget(list, area, &mut state);

    // Extend the sidebar's vertical border down through EVERY bottom row:
    // spinner + the grown editor block (1..=20 rows) +
    // hotkey hints, so the divider runs unbroken from the top edge to the
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
            // the extended divider follows the block's
            // focus emphasis so the whole divider color agrees.
            .set_style(
                Style::new()
                    .fg(if sidebar_focused {
                        theme.blue
                    } else {
                        theme.selection
                    })
                    .bg(theme.panel),
            );
    }
}

/// the assistant role header line.
///
/// `Some(label)` appends a DIM parenthetical — `● Chibi (glm-5.2)` — via
/// [`Theme::dim`]; `None` keeps the plain `● Chibi` — never a placeholder
/// like "(unknown)". The name itself stays hardcoded `Chibi` (B2/BOT_NAME
/// is a later task).
///
/// The transcript call site passes ONLY the message's own label, captured
/// when the answer arrived and persisted with the snapshot: each answer
/// keeps the model that actually produced it, and a mid-chat model switch
/// never re-labels history. Rows without their
/// own label — restored pre-label history, fieldless frames — stay plain;
/// the thread's last-known model lives in the status strip, not here.
///
/// Returns a single logical line: a long model name simply wraps within it,
/// and the row-accurate scroll math counts display rows, so nothing else
/// has to change.
fn assistant_header_line(model_label: Option<&str>, theme: &Theme) -> markdown::MdLine {
    let mut spans = vec![Span::styled(
        "\u{25cf} Chibi",
        Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
    )];
    // Trim-guarded: whitespace-only metadata must never render as "()".
    if let Some(label) = model_label.map(str::trim).filter(|l| !l.is_empty()) {
        spans.push(Span::styled(
            format!(" ({label})"),
            Style::new().fg(theme.dim),
        ));
    }
    Line::from(spans)
}

/// how many trailing lines of the raw reasoning trace the
/// block above the answer keeps. Reasoning closest to the answer is the
/// relevant part, so the head is dropped, not the tail.
const THOUGHTS_DISPLAY_LINES: usize = 10;

/// the dim reasoning block rendered ABOVE the latest
/// assistant answer, sourced from the active chat's session-only
/// `Chat::last_thoughts`.
///
/// Static plain text (no streaming, no markdown interpretation, no
/// interactivity, no scrolling UI): every line paints in the [`Theme::dim`]
/// slot. Only the LAST [`THOUGHTS_DISPLAY_LINES`] lines are kept; on
/// truncation the first kept line carries a `…` head marker. Absent or
/// whitespace-only input never reaches this helper — the renderer treats it
/// as "no block at all" (zero layout impact).
fn thoughts_block_lines(thoughts: &str, theme: &Theme) -> Vec<markdown::MdLine> {
    let all: Vec<&str> = thoughts.lines().collect();
    let truncated = all.len() > THOUGHTS_DISPLAY_LINES;
    let kept = if truncated {
        &all[all.len() - THOUGHTS_DISPLAY_LINES..]
    } else {
        &all[..]
    };
    kept.iter()
        .enumerate()
        .map(|(i, line)| {
            let text = if i == 0 && truncated {
                format!("\u{2026}{line}")
            } else {
                (*line).to_string()
            };
            Line::from(Span::styled(text, Style::new().fg(theme.dim)))
        })
        .collect()
}

fn render_chat(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect, spinner_line: Rect) {
    // chat surface tint: the FULL chat pane (border row included) carries
    // `theme.bg` BEFORE any widget renders, so empty transcript space stops
    // showing the terminal profile background and the panel < chat < input
    // ladder is app-painted. Mirrors the input zone's tint in render_input;
    // the border/title and message spans carry fg-only styles, so they patch
    // over this bg without erasing it (only code chips set their own bg).
    f.buffer_mut().set_style(area, Style::new().bg(theme.bg));

    let title = app.chat_title().replace('\n', " ");
    let left_title = format!(
        " #{}/{} \u{00b7} {} ",
        app.active + 1,
        app.chats.len(),
        title
    );
    let mut block = Block::default()
        .title(Span::styled(
            left_title.clone(),
            Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::TOP)
        .border_style(Style::new().fg(theme.selection));

    // when the strip is visible its `cwd: <path tail> ·
    // <model>` readout rides the SAME top-border row as a right-aligned
    // block title: zero extra rows (the dedicated-strip fallback was not
    // needed; rationale in the task report). The tail is the last three
    // components of the workspace path with a leading `/`. It is cut from
    // the LEFT with an ellipsis when it does not fit the width left of the
    // header title, so the working directory itself and the model survive
    // and the row never collides with the title or overflows the pane.
    if app.status_strip_visible {
        if let Some(strip) = status_strip_title(app, theme, area.width, &left_title) {
            block = block.title(strip);
        }
    }

    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(chat) = app.chats.get(app.active) else {
        return;
    };
    let chat_id = chat.id.clone();

    // Render every message into lines. `msg_ranges` records each message's
    // `[start, end)` span of LOGICAL lines (role header + content + trailing
    // blank) so the search jump can map `(message_index, line_index)` onto
    // the exact line this renderer paints.
    let mut lines: Vec<markdown::MdLine> = Vec::new();
    let mut msg_ranges: Vec<(usize, usize)> = Vec::new();
    // the reasoning block belongs to the LATEST answer —
    // it renders directly above that message's role header, inside its
    // msg_range, so the search-jump row math stays consistent. Toggle OFF
    // or absent/whitespace-only thoughts keep the transcript byte-identical
    // to the earlier layout.
    let last_assistant = chat
        .messages
        .iter()
        .rposition(|m| m.role == Role::Assistant);
    let thoughts = app
        .thoughts_visible
        .then_some(chat.last_thoughts.as_deref())
        .flatten()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    for (msg_index, msg) in chat.messages.iter().enumerate() {
        let start = lines.len();
        if Some(msg_index) == last_assistant {
            if let Some(t) = thoughts {
                lines.extend(thoughts_block_lines(t, theme));
            }
        }
        match msg.role {
            Role::User => {
                lines.push(Line::from(Span::styled(
                    "\u{25cf} You",
                    Style::new().fg(theme.orange).add_modifier(Modifier::BOLD),
                )));
            }
            Role::Assistant => {
                // Each answer renders ONLY the label captured when it was
                // produced (persisted with the snapshot): no fallback to the
                // thread's current model — a mid-chat switch must never
                // re-label past answers.
                lines.push(assistant_header_line(msg.model_label(), theme));
            }
        }
        if msg.pending {
            lines.push(Line::from(Span::styled(String::new(), Style::new())));
        } else {
            lines.extend(markdown::render(&msg.markdown, theme));
        }
        lines.push(Line::from(""));
        msg_ranges.push((start, lines.len()));
    }

    // Paragraph wrapping fix (restores the scroll contract): used to wrap
    // internally while `total` counted LOGICAL
    // markdown lines, so scroll_skip undercounted whenever any message
    // wrapped to multiple display rows, so follow-bottom then hid the tail of
    // the newest reply behind the input block. Materialize the DISPLAY rows
    // here instead, count them, and render WITHOUT an internal wrap: the
    // scrolled view and the row total are the same thing by construction.
    let width = inner.width.max(1) as usize;
    let (mut wrapped, first_row_of, row_meta) = wrap_message_rows_full(&lines, width);
    let total = wrapped.len();
    let visible = inner.height;

    // consume a pending GLOBAL search jump (Enter
    // in the all-threads search popup). `jump_to_selected_all` already
    // activated the target thread in the state layer (same mechanics as
    // Ctrl+↑/↓); this only verifies the target is STILL the active chat
    // (defensive against a chat vanishing between the state transition and
    // the next frame; the match list is recomputed on every keystroke, so
    // this is belt-and-braces) and maps the hit onto its wrapped row with
    // the SAME totals as rendering, exactly like the in-thread jump below.
    if let Some((chat_index, message_index, line_index, char_offset)) =
        app.pending_global_search_jump.take()
    {
        if chat_index == app.active {
            if let Some(row) = search_jump_wrapped_row(
                &msg_ranges,
                &first_row_of,
                &lines,
                width,
                message_index,
                line_index,
                char_offset,
            ) {
                app.scroll = scroll_for_search_jump(row, total, visible);
            }
        }
    }

    // consume a pending jump (Enter in the search
    // popup). The jump math reuses the EXACT wrapped-row totals above:
    // the same helper, the same width, so a hit inside a visually-wrapped
    // paragraph lands on the correct display row (via its char offset).
    // 1-row padding keeps the match a sliver below the top edge instead of
    // glued to it.
    if let Some((message_index, line_index, char_offset)) = app.pending_search_jump.take() {
        if let Some(row) = search_jump_wrapped_row(
            &msg_ranges,
            &first_row_of,
            &lines,
            width,
            message_index,
            line_index,
            char_offset,
        ) {
            app.scroll = scroll_for_search_jump(row, total, visible);
        }
    }

    let skip = scroll_skip(app.scroll, app.at_bottom(), total, visible);
    let overflowed = total > visible.max(1) as usize;

    // Mouse selection highlight: the selected chars of the affected rows
    // take a REVERSED overlay BEFORE the Paragraph render — markdown /
    // syntect styling of the unselected text is untouched (rows without
    // selected chars stay byte-identical).
    if let Some(sel) = &app.selection {
        apply_selection_highlight(&mut wrapped, sel);
    }

    // Render-fed geometry seam for the mouse router (same pattern as
    // chat_visible_rows): the wrapped-row model of THIS frame, so a
    // cursor position maps onto the exact document position the user
    // sees. Guarded by the thread id against the one-iteration
    // staleness window after a thread switch.
    app.chat_geometry = Some(crate::app::ChatGeometry {
        chat_id,
        inner,
        skip,
        rows: row_meta,
    });

    let text = Text::from(wrapped);
    let paragraph = Paragraph::new(text).scroll((skip as u16, 0));
    f.render_widget(paragraph, inner);

    app.chat_visible_rows = inner.height;

    if overflowed {
        render_scroll_hint(f, app.at_bottom(), theme, spinner_line);
    }
}

/// the strip's text — `cwd: <path tail> · <model>` with
/// `—` placeholders for an unwired workspace root / a chat without a known
/// model yet, plus the `ctx` usage segment rendered from the sticky
/// last-known turn usage: present once any frame this session reported
/// usage, absent until then. The tail is squeezed into the columns the strip can
/// host: cut from the LEFT with a leading `…`, so a long workspace path
/// never pushes the model readout out of the row and the working
/// directory itself stays visible. Deliberately a plain formatting seam:
/// the segment extends THIS string only — no layout or widget changes.
fn status_strip_text(app: &App, available: usize) -> String {
    let cwd_tail = app.status_cwd();
    let cwd = cwd_tail.as_deref().unwrap_or("\u{2014}");
    let model_part = format!(
        " \u{00b7} {}",
        app.active_model_label().unwrap_or("\u{2014}")
    );
    let usage_part = app
        .last_turn_usage
        .as_ref()
        .map(context_usage_segment)
        .unwrap_or_default();
    // Every column minus the `cwd: ` label, the model readout and the
    // usage segment (at least one, so the fit below is always defined).
    let cwd_budget = available
        .saturating_sub("cwd: ".width() + model_part.width() + usage_part.width())
        .max(1);
    format!(
        "cwd: {}{model_part}{usage_part}",
        left_truncate_ellipsis(cwd, cwd_budget)
    )
}

/// compact human-readable token count — raw digits under
/// 1000, `x.xk` (tenths TRUNCATED, never rounded up) under a million, else
/// `x.xM`.
fn human_tokens(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let (divisor, suffix) = if n < 1_000_000 {
        (1_000, "k")
    } else {
        (1_000_000, "M")
    };
    let whole = n / divisor;
    let tenths = (n % divisor) * 10 / divisor;
    format!("{whole}.{tenths}{suffix}")
}

/// the `ctx` segment — pct is input tokens against the
/// reported context window (floored), both counts in human format. An
/// unknown window falls back to the absolute count alone (never an
/// invented max); a zero window is treated as unknown too.
fn context_usage_segment(usage: &Usage) -> String {
    match usage.context_window {
        Some(window) if window > 0 => format!(
            " \u{00b7} ctx {}% ({}/{})",
            usage.input_tokens * 100 / window,
            human_tokens(usage.input_tokens),
            human_tokens(window)
        ),
        _ => format!(" \u{00b7} ctx {}", human_tokens(usage.input_tokens)),
    }
}

/// the strip as a right-aligned top-border [`Title`] for
/// the chat pane, dim-styled. Squeezed into the columns LEFT of the
/// left-aligned header title (1-column gutter): too-narrow panes yield
/// `None` and the strip simply does not render this frame.
fn status_strip_title<'a>(
    app: &'a App,
    theme: &Theme,
    area_width: u16,
    left_title: &str,
) -> Option<Title<'a>> {
    let gutter = 1u16;
    let available = area_width.saturating_sub(left_title.width() as u16 + gutter) as usize;
    if available == 0 {
        return None;
    }
    let text = left_truncate_ellipsis(&status_strip_text(app, available), available);
    // Alignment rides on the Line itself (ratatui groups top titles by the
    // line's own alignment; `Title::alignment` is deprecated in 0.29).
    Some(Title::from(
        Line::from(Span::styled(text, Style::new().fg(theme.dim))).alignment(Alignment::Right),
    ))
}

/// left-truncate to at most `max` display columns,
/// replacing the dropped head with a single-column ellipsis. The strip is
/// right-aligned, so its TAIL (the working directory and the model) is
/// what has to survive a squeeze, not the `cwd: ` label. Display width
/// (unicode-width) throughout: wide glyphs (CJK/emoji) are dropped whole
/// instead of straddling the boundary.
fn left_truncate_ellipsis(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.width() <= max {
        return text.to_owned();
    }
    // Reserve one column for the ellipsis, fill the rest from the right.
    let budget = max - 1;
    let chars: Vec<(char, usize)> = text
        .chars()
        .map(|ch| (ch, ch.width().unwrap_or(0)))
        .collect();
    let mut used = 0usize;
    let mut start = chars.len();
    while start > 0 {
        let w = chars[start - 1].1;
        if used + w > budget {
            break;
        }
        used += w;
        start -= 1;
    }
    let mut out = String::new();
    out.push('\u{2026}');
    for (ch, _) in &chars[start..] {
        out.push(*ch);
    }
    out
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

/// the editor block is tinted with the
/// input-panel background across EVERY of its rows and carries a cyan `❯`
/// marker on its FIRST row. The typed text renders INSIDE the remaining
/// columns over the full block height — tui-textarea keeps the cursor
/// visible inside that viewport automatically, scrolling the LAST visible
/// row toward the caret once the buffer exceeds
/// [`MAX_INPUT_LINES`](crate::app::MAX_INPUT_LINES) lines.
fn render_input(f: &mut Frame, app: &mut App, theme: &Theme, chat_column: Rect) {
    // Panel tint confined to the CHAT COLUMN (cols 26..width @120): the
    // divider cell and the sidebar strip keep `theme.panel` (approved
    // geometry).
    f.buffer_mut()
        .set_style(chat_column, Style::new().bg(theme.input_panel_bg));

    let send_label = "\u{23ce} send ";
    let has_visible_text = app.input.lines().iter().any(|l| !l.is_empty());

    // while the SIDEBAR owns focus the prompt's `❯`
    // marker dims from cyan to theme.dim, a subtle theme-driven cue that
    // typing goes nowhere until focus returns. Rename editor keeps its
    // own marker untouched.
    let marker_color = if app.focus == Focus::Sidebar {
        theme.dim
    } else {
        theme.cyan
    };

    if !has_visible_text {
        // Blank draft: placeholder + `⏎ send` chip composed in ONE line on
        // the block's FIRST row so they share the same baseline. Width math
        // measures DISPLAY columns (not byte lengths): `❯`/`⏎` are
        // multi-byte UTF-8 but single-column glyphs; measuring bytes drifts
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
            Span::styled(PROMPT_MARKER, Style::new().fg(marker_color)),
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
            Paragraph::new(Span::styled(PROMPT_MARKER, Style::new().fg(marker_color)))
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
    // Grown block repeats the input chip on the FIRST row (never
    // the last), flush right; a first-line tail under it clips: same
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

/// Rename editor shares the same tinted treatment: same `❯` marker
/// and panel tint, with the save/cancel hint right-aligned on the FIRST row.
/// a multiline draft (Shift+Enter / pasted `\n`s) renders
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
    // `^R rename` joined the hints. `/jk` was dropped:
    // single-letter navigation was removed earlier, so the old label was
    // stale; dropping it keeps the line within one row even with the longest
    // status label (`● disconnected (press R)`).
    // `⇧↵ nl` documents multi-line input; `^C
    // cancel/quit` shortened to `^C cancel` so everything still fits 120
    // columns with that longest label.
    // thread switching moved to `^↑↓ chats` and plain
    // `↑↓` became caret movement (`↑↓ caret`). To keep both new hints AND
    // the longest status label inside one 120-col row, the self-evident
    // `PgUp/PgDn scroll` hint retired (PgUp/PgDn still work; README
    // documents them). ^C cancel / ⇧↵ nl semantics are untouched.
    // `^D del` joined the hints; the self-evident
    // `^V paste` compacted to `^V` (the universal paste convention; README
    // documents both Ctrl+V and macOS Cmd+V) so everything still fits one
    // 120-col row with the longest status label.
    // `^F find` joined the hints; `^L clear` compacted
    // to `^L` (the action is still documented in README and self-evident
    // enough next to `^V`) so the line stays within 120 cols with the
    // longest status label (`● disconnected (press R)`).
    // `^⇧F all` joined the hints; the self-evident
    // `^L` and `^V` hints retired (both actions are README-documented and
    // universal enough (clear-screen-then and paste) so the line stays within
    // 120 cols with the longest status label. ^C cancel is never dropped.
    // ^L was later repurposed to the guarded stop
    // confirmation, so its hints-row absence stayed correct — the row was at
    // 119 cols with `F1 help` + the `^S` state token, one short of the 120
    // cap, and no token could be added without retiring another.
    // `^↑↓ chats` grew to `^/⌥↑↓ chats` (Alt is a
    // full synonym for thread switching, because macOS Mission Control hijacks
    // Ctrl+arrows). To fit the +2-col token, the self-evident `^F find`
    // compacted to `^F` (Ctrl+F is THE universal find convention, same
    // precedent as `^V`; README documents it), keeping the row within 120
    // cols with the longest status label. ^C cancel is never dropped.
    // `^T next` joined the hints (universal terminal-proof
    // thread switching that WRAPS around, the browser Ctrl+Tab convention).
    // To fit the +7-col token, the self-evident `^N new` compacted to `^N`
    // (Ctrl+N is THE universal new-chat convention, same precedent as `^F`/
    // `^V`) and `⇧↵ nl` compacted to `⇧↵` (Shift+Enter newline is a universal
    // chat-app convention; README documents both), keeping the row within 120
    // cols with the longest status label. ^C cancel is never dropped.
    // `^T next` became `^T panel`. Ctrl+T now TOGGLES
    // pane focus (Chat ↔ Sidebar) instead of wrap-cycling threads (the
    // cycling semantics were rejected in live-check). `^R rename` compacted
    // to `^R` (the action is README-documented and the popup itself is
    // self-explanatory) keeps the +1-col cost inside 120 cols with the
    // longest status label. ^C cancel is never dropped.
    // `^O info` joined the hints (the toggle for the dim
    // `cwd · model` strip on the chat header border). The strip is HIDDEN
    // by default, so a hint-shown-only-while-visible scheme would leave the
    // feature undiscoverable, so the toggle hint is PERMANENT (decision
    // documented in the README + task report). To pay the +10 cols the
    // self-evident `^D del` compacted to `^D` (the confirm popup is
    // self-explanatory, same precedent as `^R`) and `^T panel` compacted to
    // `^T` (the focus flip is instantly visible feedback; both actions stay
    // README-documented). Net width change: ZERO; the row stays at 87
    // cols, so the `log*` worst case (87 hints + 7 marker + 2 separator +
    // 24 longest label = 120 ≤ 120) holds verbatim. ^C cancel is never
    // dropped.
    // `^P` joins the hints, but ONLY when the backend
    // listed the clone command at handshake (detection, never assumption):
    // an older backend must never promise a dead key. To pay the +5 cols
    // `^O info` compacted to bare `^O` (the toggle itself stays permanent,
    // same precedent as ^N/^R/^D/^F/^T; README documents the full names).
    // Base row: 82 cols, 87 with the clone token, so the `log*` worst case
    // (87 + 7 marker + 2 separator + 24 longest label = 120 ≤ 120) still
    // holds verbatim. ^C cancel is never dropped.
    // `F1 help` joins the hints (the toggle for the
    // full keybindings modal — every chord the status row can no longer
    // spell out lives one keypress away). To pay the +10 cols the
    // self-evident `↑↓ caret` token retired: plain arrows moving the text
    // caret is the universal editor convention (same retirement precedent
    // as PgUp/PgDn scroll; README still documents them). Net width change:
    // base 82 → 81, 86 with the clone token, so the `log*` worst case
    // (86 + 7 marker + 2 separator + 24 longest label = 119 ≤ 120) still
    // holds. ^C cancel is never dropped.
    // a `^S on/off` state token joins the row — the
    // reasoning toggle's visible feedback AND its hint in one compact
    // readout (the F1 modal spells the action out; both states render, so
    // the line answers "will the block show?" at a glance). To pay the +9
    // cols the self-evident `⇧↵` newline hint retired (Shift+Enter is the
    // universal chat-app newline convention, README-documented) and
    // `^⇧F all` compacted to `^⇧F` (the shifted find next to `^F` reads as
    // the global search; README documents it). Net width change: base 72,
    // so the `log*` worst case (77 hints+clone, + 9 thoughts token off, + 7
    // marker + 2 separator + 24 longest label = 119 ≤ 120) still holds.
    // ^C cancel is never dropped.
    let spans = if let Some((message, _)) = &app.status_message {
        // Transient status toast (busy-delete refusal): replaces the hint
        // block while visible. Short message + connection label always fit
        // one row.
        vec![
            Span::styled(format!("  {message}"), Style::new().fg(theme.yellow)),
            Span::raw("  "),
            Span::styled(status_label, Style::new().fg(status_color)),
        ]
    } else {
        let mut spans = vec![Span::styled(
            "   ^/\u{2325}\u{2191}\u{2193} chats \u{00b7} ^N \u{00b7} ^R \u{00b7} ^C cancel \u{00b7} ^D \u{00b7} ^F \u{00b7} ^\u{21e7}F \u{00b7} ^T \u{00b7} ^O \u{00b7} F1 help",
            Style::new().fg(theme.selection),
        )];
        // the clone chord is advertised only when the
        // handshake capabilities listed the command (see the width math in
        // the comment block above).
        if app.supports_thread_clone() {
            spans.push(Span::styled(
                " \u{00b7} ^P",
                Style::new().fg(theme.selection),
            ));
        }
        // the reasoning toggle's state — the visible
        // Ctrl+S feedback and the row's ^S hint in one token. Both states
        // render (the line answers "will the block show?" at a glance); dim
        // like the log* readout it sits next to. Hidden while a modal owns
        // the keyboard, same transient-state rule as log*.
        if app.mode.is_normal() {
            spans.push(Span::styled(
                format!(
                    " \u{00b7} ^S {}",
                    if app.thoughts_visible { "on" } else { "off" }
                ),
                Style::new().fg(theme.dim),
            ));
        }
        // subtle dim `log*` token when unseen
        // diagnostic lines arrived since the last viewer visit (consumer-side
        // watermark over the monotonic producer total, race-free). Placement
        // check (documented in the task report): hints (87 cols worst case,
        // 82 base + 5 when the clone token shows) + ` · log*`
        // (7) + separator (2) + the longest status label
        // `● disconnected (press R)` (24) = 120 ≤ 120, so it FITS the status
        // line, so the chat-header fallback was not needed. Hidden while a
        // modal is open (transient states) and under toasts.
        if app.mode.is_normal() && crate::diag::total_appended() > app.log_seen_total {
            spans.push(Span::styled(" \u{00b7} log*", Style::new().fg(theme.dim)));
        }
        spans.push(Span::raw("  "));
        spans.push(Span::styled(status_label, Style::new().fg(status_color)));
        spans
    };

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(theme.panel)),
        area,
    );
}

/// the diagnostics-log viewer modal — a large
/// centered view over a SNAPSHOT copy of the diag ring buffer
/// (`App::begin_log_viewer` / the `log_*` cursor methods own the state,
/// `main.rs` owns the keys). Interaction core: a
/// line cursor walks LOGICAL lines: while it rests on the newest line the
/// view stays pinned to the tail and new lines stream in; one step up pins
/// the view to the cursor and the snapshot freezes (the `+K new lines`
/// footer counts arrivals instead). `w` toggles Paragraph-style reflow:
/// wrap off = compact truncated timeline, wrap on = full text chunked over
/// rows at the content width. Levels parsed at ingestion colorize per the
/// [`log_line_style`] table (`TRACE` very dim, `DEBUG` dim gray, `INFO` and
/// `SUCCESS` green, `WARNING` yellow, `ERROR` red, `CRITICAL` bold red, the
/// backend's custom levels per their registration colors); lines without a
/// recognizable level keep the default foreground and `[tui]` lifecycle
/// events stay dimmed.
fn render_log_viewer(f: &mut Frame, app: &mut App, theme: &Theme) {
    use ratatui::widgets::{Clear, Padding};

    // Tail-follow: while the cursor rests on the newest line, refresh the
    // snapshot every frame (atomic snapshot + baseline, see `diag::view`)
    // and keep the cursor parked on the tail. Pinned views never refresh.
    if let Mode::LogViewer { state } = &mut app.mode {
        // The copy feedback is brief: drop it once its window passed.
        state.expire_copy_note();
        if state.at_tail() {
            let (lines, total) = crate::diag::view();
            state.cursor = lines.len().saturating_sub(1);
            state.snapshot_total = total;
            state.lines = lines;
            // New lines can add (or evict) matches: keep the list honest.
            state.reindex_search();
        }
    }
    let (lines, cursor, wrap, snapshot_total, mut row_offset, search_buf, search, copy_note) =
        match &app.mode {
            Mode::LogViewer { state } => (
                state.lines.clone(),
                state.cursor,
                state.wrap,
                state.snapshot_total,
                state.row_offset,
                state.search_buf.clone(),
                state.search.clone(),
                state.copy_note.clone(),
            ),
            _ => return,
        };
    let at_tail = cursor + 1 >= lines.len();
    let new_since = crate::diag::total_appended().saturating_sub(snapshot_total);

    // ~3/4 of the frame, sane caps so tiny terminals never panic.
    let max_h = f.area().height.saturating_sub(2).max(3);
    let width = f.area().width.saturating_sub(4).clamp(20, 110);
    let height = (f.area().height * 3 / 4).clamp(3, max_h);
    let x = f.area().x + (f.area().width.saturating_sub(width)) / 2;
    let y = f.area().y + (f.area().height.saturating_sub(height)) / 2;
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, area);
    // Header state rides the border title: cursor
    // position, tail state and the wrap toggle. The search feature
    // appends the search prompt (while typing), the match count and the
    // copy feedback. One line, width-budgeted by capping the pattern echo.
    let pos = lines.len().min(cursor + 1);
    let tail_state = if at_tail { "live" } else { "pinned" };
    let mut title = format!(
        " Diagnostics log \u{00b7} line {pos}/{} \u{00b7} {tail_state} \u{00b7} wrap: {}",
        lines.len(),
        if wrap { "on" } else { "off" },
    );
    if let Some(buf) = &search_buf {
        title.push_str(&format!(
            " \u{00b7} search: /{}",
            truncate_for_title(buf, 12)
        ));
    }
    if let Some(search) = &search {
        match search.current {
            Some(i) => title.push_str(&format!(
                " \u{00b7} matches: {}/{}",
                i + 1,
                search.matches.len()
            )),
            None => title.push_str(&format!(" \u{00b7} matches: {}", search.matches.len())),
        }
    }
    if let Some(note) = &copy_note {
        title.push_str(&format!(" \u{00b7} {note}"));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme.blue))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title,
            Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Content above, one permanent footer row below (the hint never clips).
    let panes = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let content = panes[0];
    let footer = panes[1];

    // Seams for the cursor/page math (same pattern as chat_visible_rows):
    // the page is a page of rows, and wrap chunks at the render width.
    app.log_visible_rows = content.height.max(1);
    app.log_content_width = content.width.max(1);

    if content.height > 0 && !lines.is_empty() {
        let visible = content.height as usize;
        let content_w = content.width.max(1) as usize;

        // The line the current search hit sits on (emphasized variant).
        let current_match_line = search
            .as_ref()
            .and_then(|s| s.current.map(|i| s.matches[i]));

        // One rendered row per chunk; rows remember their logical line so
        // coloring and the cursor highlight survive the reflow. Each chunk
        // also carries its char offset inside the logical line, so match
        // ranges cut into the right pieces even when a hit spans a wrap
        // boundary.
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut logical_of_row: Vec<usize> = Vec::new();
        for (idx, logical) in lines.iter().enumerate() {
            let base_style = log_line_style(logical, theme);
            let ranges = search
                .as_ref()
                .map(|s| crate::app::find_match_ranges(&logical.text, &s.pattern))
                .unwrap_or_default();
            let chunks: Vec<(String, usize)> = if wrap {
                let mut offset = 0usize;
                crate::app::wrap_line(&logical.text, content_w)
                    .into_iter()
                    .map(|c| {
                        let start = offset;
                        offset += c.chars().count();
                        (c, start)
                    })
                    .collect()
            } else {
                vec![(logical.text.clone(), 0)]
            };
            for (chunk, chunk_start) in chunks {
                let spans = log_line_spans(
                    &chunk,
                    chunk_start,
                    &ranges,
                    idx == cursor,
                    Some(idx) == current_match_line,
                    base_style,
                    theme,
                );
                rows.push(Line::from(spans));
                logical_of_row.push(idx);
            }
        }

        // First/last rendered row of the cursor line, then the visible
        // window: at the tail the view bottom-anchors; pinned, it scrolls
        // minimally so the cursor line stays fully in view.
        let total_rows = rows.len();
        let mut first_row = 0usize;
        for (idx, logical) in lines.iter().enumerate() {
            if idx == cursor {
                break;
            }
            first_row += if wrap {
                crate::app::wrapped_row_count(&logical.text, content_w)
            } else {
                1
            };
        }
        let cursor_rows = if wrap {
            crate::app::wrapped_row_count(&lines[cursor].text, content_w)
        } else {
            1
        };
        let last_row = first_row + cursor_rows.saturating_sub(1);
        let max_offset = total_rows.saturating_sub(visible);
        if at_tail {
            row_offset = max_offset;
        } else {
            row_offset = row_offset.min(max_offset);
            if row_offset > first_row {
                row_offset = first_row;
            } else if last_row + 1 > row_offset + visible {
                row_offset = (last_row + 1).saturating_sub(visible);
            }
        }
        // Persist the viewport so the next pinned frame scrolls from here.
        if let Mode::LogViewer { state } = &mut app.mode {
            state.row_offset = row_offset;
        }

        let paragraph = Paragraph::new(Text::from(rows)).scroll((row_offset as u16, 0));
        f.render_widget(paragraph, content);
    }

    if footer.height > 0 {
        // While the search prompt is open it REPLACES the hint line: the
        // pattern echoes here with a block cursor, Enter commits, Esc
        // cancels.
        let (text, color) = if let Some(buf) = &search_buf {
            (
                format!("/{}\u{258f} Enter commit \u{00b7} Esc cancel", buf),
                theme.blue,
            )
        } else {
            let mut hint =
                "Esc close \u{00b7} k/j lines \u{00b7} PgUp/PgDn page \u{00b7} g/G ends \u{00b7} w wrap \u{00b7} / search \u{00b7} y copy"
                    .to_owned();
            if new_since > 0 && !at_tail {
                hint.push_str(&format!(" \u{00b7} +{new_since} new lines"));
            }
            (hint, theme.yellow)
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::new().fg(color)))),
            footer,
        );
    }
}

/// Level to style table for diagnostics lines, driven by the level parsed
/// at ingestion (see [`crate::diag::LogEntry::parse`]): `TRACE` very dim,
/// `DEBUG` dim gray, `INFO` and `SUCCESS` green, `WARNING` yellow, `ERROR`
/// red, `CRITICAL` bold red. The backend's custom levels take the theme
/// slots their backend registration colors mirror: `TOOL` light-blue →
/// blue, `THINK` light-magenta → purple, `CALL` magenta → purple, `CHECK`
/// and `MODERATOR` light-red → red, `SUBAGENT` cyan → cyan, `DELEGATE`
/// blue → blue (the theme keeps one accent per color family, so the
/// light/regular variants of one family share its slot). Entries without a
/// level — old backends, malformed lines, non-log stderr output,
/// unrecognized level names — keep the default foreground; `[tui]`
/// lifecycle events stay dim. No raw RGB here: everything routes through
/// the theme slots.
fn log_line_style(entry: &crate::diag::LogEntry, theme: &Theme) -> ratatui::style::Style {
    if entry.text.starts_with(crate::diag::TUI_EVENT_PREFIX) {
        return Style::new().fg(theme.dim);
    }
    use crate::diag::LogLevel;
    match entry.level {
        Some(LogLevel::Trace) => Style::new().fg(theme.log_trace),
        Some(LogLevel::Debug) => Style::new().fg(theme.log_debug),
        Some(LogLevel::Info) => Style::new().fg(theme.green),
        Some(LogLevel::Warning) => Style::new().fg(theme.yellow),
        Some(LogLevel::Error) => Style::new().fg(theme.red),
        Some(LogLevel::Critical) => Style::new().fg(theme.red).add_modifier(Modifier::BOLD),
        Some(LogLevel::Success) => Style::new().fg(theme.green),
        Some(LogLevel::Tool) => Style::new().fg(theme.blue),
        Some(LogLevel::Think) => Style::new().fg(theme.purple),
        Some(LogLevel::Call) => Style::new().fg(theme.purple),
        Some(LogLevel::Check) => Style::new().fg(theme.red),
        Some(LogLevel::Moderator) => Style::new().fg(theme.red),
        Some(LogLevel::Subagent) => Style::new().fg(theme.cyan),
        Some(LogLevel::Delegate) => Style::new().fg(theme.blue),
        None => Style::new().fg(theme.fg),
    }
}

/// Styled spans for one rendered row of the log viewer (see
/// [`render_log_viewer`]): the base level color, the cursor-line highlight,
/// and the search-match overlay cut into the row text at the right char
/// offsets. `chunk_start` is the chunk's offset inside its logical line, so
/// a hit spanning a wrap boundary lights up in both rows. The current match
/// line takes the same slot with reversed colors on top.
fn log_line_spans(
    chunk: &str,
    chunk_start: usize,
    ranges: &[(usize, usize)],
    is_cursor: bool,
    is_current_match: bool,
    base_style: ratatui::style::Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let mut base = base_style;
    if is_cursor {
        base = base.bg(theme.selection);
    }
    let chars: Vec<char> = chunk.chars().collect();
    let chunk_end = chunk_start + chars.len();

    // Per-char membership, then grouped into runs: simplest correct way to
    // split the row text at arbitrary (possibly chunk-crossing) ranges.
    let mut in_match = vec![false; chars.len()];
    for &(start, end) in ranges {
        let s = start.max(chunk_start);
        let e = end.min(chunk_end);
        for i in s..e {
            in_match[i - chunk_start] = true;
        }
    }

    let mut spans = Vec::new();
    let mut run = 0usize;
    for i in 1..=chars.len() {
        if i == chars.len() || in_match[i] != in_match[run] {
            let text: String = chars[run..i].iter().collect();
            let mut style = base;
            if in_match[run] {
                style = style.fg(theme.log_match).add_modifier(Modifier::BOLD);
                if is_current_match {
                    style = style.add_modifier(Modifier::REVERSED);
                }
            }
            spans.push(Span::styled(text, style));
            run = i;
        }
    }
    spans
}

/// Cap the pattern echo in the border title so the header stays a single
/// coherent line even with a long query typed in.
fn truncate_for_title(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

/// Centered modal model-picker popup. Same popup
/// family as the search modals — theme-driven, centered, hint row last —
/// with a stateful [`List`] body: the selection is ratatui-selection-aware,
/// so a listing longer than the viewport scrolls and the highlighted row
/// auto-scrolls into view. `Loading` shows a quiet placeholder row while
/// the hidden `/model` request is in flight. The body's visible row height
/// feeds [`App::picker_visible_rows`] so PgUp/PgDn page exactly one
/// on-screen viewport. Purely visual — all key handling lives in `main.rs`.
fn render_model_picker(f: &mut Frame, app: &mut App, theme: &Theme) {
    use ratatui::widgets::{Clear, List, ListItem, ListState, Padding};

    let Mode::ModelPicking { state } = &app.mode else {
        return;
    };
    let loading = state.phase == ModelPickerPhase::Loading;
    let entries = state.entries.clone();
    let selected = state.selected;
    let count = entries.len();

    let hint = "\u{2191}\u{2193} navigate \u{00b7} PgUp/PgDn page \u{00b7} Enter switch \u{00b7} Esc close \u{00b7} Ctrl+C quit";

    // Max model rows shown before the stateful list scrolls internally
    // (the highlighted row auto-scrolls into view).
    const MAX_LIST_ROWS: usize = 12;

    let title = if loading {
        " Model picker ".to_owned()
    } else {
        format!(" Model picker \u{00b7} {count} models ")
    };

    // Size to content with sane caps; centered on the full frame (same
    // sizing pattern as the search popups; floors keep tiny terminals safe).
    let max_w = f.area().width.saturating_sub(4).max(20);
    let widest_row = entries
        .iter()
        .map(|e| crate::model_picker::listing_row_label(e).width())
        .max()
        .unwrap_or(16)
        + 2; // selection marker + breathing room
    let width = (hint.width() as u16 + 12)
        .max(40)
        .max(widest_row.min(60) as u16)
        .clamp(20, max_w);
    let body_rows = if loading {
        1
    } else {
        count.clamp(1, MAX_LIST_ROWS)
    };
    let height = (body_rows as u16 + 1 + 2) // + hint row + borders
        .min(f.area().height.saturating_sub(2))
        .max(3);
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
        .border_style(Style::new().fg(theme.blue))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title,
            Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    // Body rows above, decision hint on the last row (same family shape).
    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let footer = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };

    app.picker_visible_rows = body.height.max(1);

    if loading {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "loading models\u{2026}",
                Style::new().fg(theme.dim),
            ))),
            body,
        );
    } else {
        let rows: Vec<ListItem<'static>> = entries
            .iter()
            .map(|e| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                if e.active {
                    // The backend's own 🟢 active-model marker, re-styled.
                    spans.push(Span::styled(
                        "\u{25cf} ".to_owned(),
                        Style::new().fg(theme.green),
                    ));
                } else {
                    spans.push(Span::styled("  ".to_owned(), Style::new()));
                }
                spans.push(Span::styled(
                    crate::model_picker::listing_row_label(e),
                    Style::new().fg(theme.fg),
                ));
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(rows)
            .style(Style::new().bg(theme.bg))
            .highlight_style(
                Style::new()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("\u{25b8} ");
        let mut list_state = ListState::default();
        list_state.select(Some(selected));
        f.render_stateful_widget(list, body, &mut list_state);
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::new().fg(theme.yellow),
        ))),
        footer,
    );
}

/// One row of the keybindings help modal: the
/// chord as displayed, the logical group it belongs to and the action the
/// dispatch performs. `group` must stay contiguous — the renderer opens a
/// group header on every change (see [`help_modal_total_lines`]).
pub struct HotkeyRow {
    /// Group label rendered as the dim separator row above the entry.
    pub group: &'static str,
    /// Chord as shown in the left column (`Ctrl+N`, `⇧↵ / ⌥↵`, `text`, …).
    pub chord: &'static str,
    /// Human action description in the right column.
    pub action: &'static str,
}

/// The SINGLE source of truth for the help modal's content: every chord the
/// key dispatch (`main.rs::handle_key` + the tui-textarea fall-through)
/// actually handles, grouped by surface. The dispatch itself is inline match
/// arms with no action enum to reuse, so this const table sits next to the
/// render; `main.rs`'s test module pins it against the live handlers with a
/// dispatch enumeration plus a typing-leak probe, so a chord added to the
/// dispatch without a matching row here fails the suite.
pub const HOTKEY_ROWS: &[HotkeyRow] = &[
    HotkeyRow {
        group: "Global",
        chord: "F1",
        action: "open / close this keybindings help",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+C",
        action: "cancel the active request · quit when idle",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+N",
        action: "new chat",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+P",
        action: "clone the active thread (needs backend support)",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+R",
        action: "rename the active thread",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+D",
        action: "delete the active thread (confirmation)",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+F",
        action: "find in the active thread",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+Shift+F",
        action: "find in all threads",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+T",
        action: "toggle pane focus (chat / sidebar)",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+G",
        action: "open the diagnostics log viewer",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+O",
        action: "toggle the status strip",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+S",
        action: "toggle the thoughts block",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+M",
        action: "open the model picker",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+L",
        action: "stop the running request (confirmation)",
    },
    HotkeyRow {
        group: "Global",
        chord: "Shift+Ctrl+L",
        action: "reset this thread · clear the dialog (confirmation)",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl+↑/↓ · Alt+↑/↓",
        action: "switch the active thread",
    },
    HotkeyRow {
        group: "Global",
        chord: "PgUp / PgDn",
        action: "scroll the chat view",
    },
    HotkeyRow {
        group: "Global",
        chord: "Wheel ↑ / ↓",
        action: "scroll the chat · select in the focused sidebar",
    },
    HotkeyRow {
        group: "Global",
        chord: "Drag-select",
        action: "highlight chat text · release copies it",
    },
    HotkeyRow {
        group: "Global",
        chord: "Ctrl-chords",
        action: "work under RU / UA keyboard layouts",
    },
    HotkeyRow {
        group: "Global",
        chord: "Esc",
        action: "clear the input · dismiss popups",
    },
    HotkeyRow {
        group: "Input",
        chord: "Enter",
        action: "send the message (queues while busy)",
    },
    HotkeyRow {
        group: "Input",
        chord: "⇧↵ / ⌥↵",
        action: "insert a newline",
    },
    HotkeyRow {
        group: "Input",
        chord: "↑ / ↓",
        action: "move the caret in the draft",
    },
    HotkeyRow {
        group: "Input",
        chord: "Ctrl+A / Ctrl+E",
        action: "caret to line start / end",
    },
    HotkeyRow {
        group: "Input",
        chord: "Ctrl+U",
        action: "delete to the start of the line",
    },
    HotkeyRow {
        group: "Input",
        chord: "Ctrl+V / Cmd+V",
        action: "paste the clipboard",
    },
    HotkeyRow {
        group: "Input",
        chord: "Backspace",
        action: "delete backwards",
    },
    HotkeyRow {
        group: "Input",
        chord: "text",
        action: "type into the draft",
    },
    HotkeyRow {
        group: "Sidebar (Ctrl+T)",
        chord: "↑ / ↓",
        action: "select a thread (switches live)",
    },
    HotkeyRow {
        group: "Sidebar (Ctrl+T)",
        chord: "Enter · Esc",
        action: "return to the editor",
    },
    HotkeyRow {
        group: "Rename (Ctrl+R)",
        chord: "Enter",
        action: "save the title",
    },
    HotkeyRow {
        group: "Rename (Ctrl+R)",
        chord: "⇧↵ / ⌥↵",
        action: "insert a newline into the title",
    },
    HotkeyRow {
        group: "Rename (Ctrl+R)",
        chord: "Backspace",
        action: "delete backwards",
    },
    HotkeyRow {
        group: "Rename (Ctrl+R)",
        chord: "Esc",
        action: "cancel the rename",
    },
    HotkeyRow {
        group: "Delete (Ctrl+D)",
        chord: "Enter · y",
        action: "confirm the delete",
    },
    HotkeyRow {
        group: "Delete (Ctrl+D)",
        chord: "Esc · n",
        action: "cancel",
    },
    HotkeyRow {
        group: "Model picker (Ctrl+M)",
        chord: "↑ / ↓",
        action: "move the selection",
    },
    HotkeyRow {
        group: "Model picker (Ctrl+M)",
        chord: "PgUp / PgDn",
        action: "page the selection",
    },
    HotkeyRow {
        group: "Model picker (Ctrl+M)",
        chord: "Enter",
        action: "apply the highlighted model",
    },
    HotkeyRow {
        group: "Model picker (Ctrl+M)",
        chord: "Esc",
        action: "close without switching",
    },
    HotkeyRow {
        group: "Find in thread (Ctrl+F)",
        chord: "text",
        action: "edit the query (live filter)",
    },
    HotkeyRow {
        group: "Find in thread (Ctrl+F)",
        chord: "Backspace",
        action: "delete backwards",
    },
    HotkeyRow {
        group: "Find in thread (Ctrl+F)",
        chord: "↑ / ↓",
        action: "walk the matches",
    },
    HotkeyRow {
        group: "Find in thread (Ctrl+F)",
        chord: "Enter",
        action: "jump to the match · close",
    },
    HotkeyRow {
        group: "Find in thread (Ctrl+F)",
        chord: "Esc",
        action: "close without jumping",
    },
    HotkeyRow {
        group: "Find everywhere (Ctrl+⇧F)",
        chord: "text",
        action: "edit the query (live filter)",
    },
    HotkeyRow {
        group: "Find everywhere (Ctrl+⇧F)",
        chord: "Backspace",
        action: "delete backwards",
    },
    HotkeyRow {
        group: "Find everywhere (Ctrl+⇧F)",
        chord: "↑ / ↓",
        action: "walk the matches",
    },
    HotkeyRow {
        group: "Find everywhere (Ctrl+⇧F)",
        chord: "Enter",
        action: "activate the match's thread · jump",
    },
    HotkeyRow {
        group: "Find everywhere (Ctrl+⇧F)",
        chord: "Esc",
        action: "close without jumping",
    },
    HotkeyRow {
        group: "Log viewer (Ctrl+G)",
        chord: "↑ / ↓ · k / j",
        action: "move the cursor one line",
    },
    HotkeyRow {
        group: "Log viewer (Ctrl+G)",
        chord: "PgUp / PgDn",
        action: "page the view",
    },
    HotkeyRow {
        group: "Log viewer (Ctrl+G)",
        chord: "g / G",
        action: "jump to the top / the tail",
    },
    HotkeyRow {
        group: "Log viewer (Ctrl+G)",
        chord: "w",
        action: "toggle wrap",
    },
    HotkeyRow {
        group: "Log viewer (Ctrl+G)",
        chord: "/",
        action: "search the log",
    },
    HotkeyRow {
        group: "Log viewer (Ctrl+G)",
        chord: "n / N",
        action: "next / previous match",
    },
    HotkeyRow {
        group: "Log viewer (Ctrl+G)",
        chord: "y",
        action: "copy the cursor line",
    },
    HotkeyRow {
        group: "Log viewer (Ctrl+G)",
        chord: "Esc",
        action: "close",
    },
    HotkeyRow {
        group: "Log search (/)",
        chord: "text",
        action: "edit the pattern",
    },
    HotkeyRow {
        group: "Log search (/)",
        chord: "Backspace",
        action: "delete backwards",
    },
    HotkeyRow {
        group: "Log search (/)",
        chord: "Enter",
        action: "commit the search",
    },
    HotkeyRow {
        group: "Log search (/)",
        chord: "Esc",
        action: "cancel the search",
    },
    HotkeyRow {
        group: "Error popup",
        chord: "R",
        action: "reconnect",
    },
    HotkeyRow {
        group: "Error popup",
        chord: "q",
        action: "quit",
    },
    HotkeyRow {
        group: "Error popup",
        chord: "Esc · any key",
        action: "dismiss",
    },
    HotkeyRow {
        group: "Help (F1)",
        chord: "↑ / ↓ · PgUp / PgDn",
        action: "scroll the list",
    },
    HotkeyRow {
        group: "Help (F1)",
        chord: "F1 · Esc",
        action: "close",
    },
];

/// Total rendered line count of the help modal's body: one line per
/// [`HOTKEY_ROWS`] entry plus one header per distinct (contiguous) group.
/// The scroll clamps in `app.rs` page over THIS count, so the window can
/// always reach the table's last row.
pub fn help_modal_total_lines() -> usize {
    let mut lines = HOTKEY_ROWS.len();
    let mut prev_group = "";
    for row in HOTKEY_ROWS {
        if row.group != prev_group {
            lines += 1;
            prev_group = row.group;
        }
    }
    lines
}

/// the F1 keybindings modal — a centered, bordered
/// popup over the chat panes listing EVERY active chord, built from the
/// [`HOTKEY_ROWS`] const table (the dispatch-side test in `main.rs` pins the
/// table against the real key handlers). Visual language copied from the
/// model picker: blue bordered box, bold blue title, body rows above and
/// a yellow decision hint on the last row. The body is a scrolled window of
/// `help_visible_rows` lines; the offset lives in the mode state and is
/// clamped here against the REAL viewport so a resized frame can never show
/// a stale window.
fn render_help_modal(f: &mut Frame, app: &mut App, theme: &Theme) {
    use ratatui::widgets::{Clear, Padding};

    let Mode::HelpViewing { state } = &mut app.mode else {
        return;
    };
    let hint = "\u{2191}\u{2193} scroll \u{00b7} PgUp/PgDn page \u{00b7} F1/Esc close";

    let chord_w = HOTKEY_ROWS
        .iter()
        .map(|r| r.chord.width())
        .max()
        .unwrap_or(8);
    // Rows pad the chord column to the GLOBAL max chord width, so the
    // widest rendered line is that padding + separator + the widest action.
    let action_w = HOTKEY_ROWS
        .iter()
        .map(|r| r.action.width())
        .max()
        .unwrap_or(24);
    let row_w = chord_w + 2 + action_w;

    let max_w = f.area().width.saturating_sub(4).max(20);
    let width = (hint.width() as u16 + 12)
        .max((row_w as u16 + 6).min(max_w))
        .clamp(20, max_w);
    let max_body = f.area().height.saturating_sub(6) as usize;
    let total = help_modal_total_lines();
    let body_rows = total.min(24).min(max_body.max(1));
    let height = (body_rows as u16 + 1 + 2) // + hint row + borders
        .min(f.area().height.saturating_sub(2))
        .max(3);
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
        .border_style(Style::new().fg(theme.blue))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            " Keybindings ",
            Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    // Body rows above, decision hint on the last row (same family shape).
    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let footer = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };

    app.help_visible_rows = body.height.max(1);

    let visible = body.height as usize;
    let max_scroll = total.saturating_sub(visible);
    state.scroll = state.scroll.min(max_scroll);
    let scroll = state.scroll;

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total);
    let mut prev_group = "";
    for row in HOTKEY_ROWS {
        if row.group != prev_group {
            prev_group = row.group;
            lines.push(Line::from(Span::styled(
                row.group.to_owned(),
                Style::new().fg(theme.dim).add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<width$}", row.chord, width = chord_w),
                Style::new().fg(theme.cyan),
            ),
            Span::raw("  ".to_owned()),
            Span::styled(row.action.to_owned(), Style::new().fg(theme.fg)),
        ]));
    }
    let window_end = (scroll + visible).min(lines.len());
    let window = &lines[scroll.min(lines.len())..window_end];

    f.render_widget(
        Paragraph::new(Text::from(window.to_vec())).style(Style::new().bg(theme.bg)),
        body,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::new().fg(theme.yellow),
        ))),
        footer,
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

/// Centered modal delete-confirmation popup over the full frame.
/// Theme-driven only: red border + red title for the
/// destructive action, yellow decision hint. Purely visual — all key
/// handling lives in `main.rs`.
fn render_delete_popup(f: &mut Frame, app: &mut App, theme: &Theme) {
    use ratatui::widgets::{Clear, Padding};

    if !matches!(app.mode, Mode::ConfirmDelete) {
        return;
    }
    // Multi-line titles collapse to spaces for the one-line popup message.
    let title = app.chat_title().replace('\n', " ");
    let message = format!("Delete \u{201c}{title}\u{201d} permanently?");
    let hint = "y/Enter confirm \u{00b7} Esc/n cancel \u{00b7} Ctrl+C quit";

    // Centered box at ~50% of the frame width (content-driven minimum, hard
    // floor of 20 so tiny terminals never panic on an invalid clamp range).
    let max_w = f.area().width.saturating_sub(4).max(20);
    let width = (message.width() as u16 + hint.width() as u16 + 6)
        .max(f.area().width / 2)
        .clamp(20, max_w);
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
            " Delete thread ",
            Style::new().fg(theme.red).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height > 0 {
        let lines = wrap_text(&message, inner.width.max(1) as usize);
        let hint_line = Line::from(Span::styled(hint, Style::new().fg(theme.yellow)));
        let mut all: Vec<Line<'static>> = lines;
        all.push(hint_line); // rendered last; clipped when out of room
        let text = Text::from(all);
        let visible = inner.height as usize;
        let skip = text.height().saturating_sub(visible);
        let paragraph = Paragraph::new(text).scroll((skip as u16, 0));
        f.render_widget(paragraph, inner);
    }
}

/// Centered modal stop/reset confirmation popup,
/// over the full frame. Same visual language as the delete confirm: red
/// border + red title for the destructive action, yellow decision hint —
/// theme slots only. Purely visual — all key handling lives in `main.rs`.
fn render_stop_reset_popup(f: &mut Frame, app: &mut App, theme: &Theme) {
    use ratatui::widgets::{Clear, Padding};

    let Mode::ConfirmStopReset { action } = app.mode else {
        return;
    };
    let (title, message) = match action {
        StopResetAction::Stop => (" Stop request ", "Stop the running request?"),
        StopResetAction::Reset => (" Reset thread ", "Reset this thread? (clears the dialog)"),
    };
    let hint = "y/Enter confirm \u{00b7} Esc/n cancel \u{00b7} Ctrl+C quit";

    // Centered box at ~50% of the frame width (content-driven minimum, hard
    // floor of 20 so tiny terminals never panic on an invalid clamp range).
    let max_w = f.area().width.saturating_sub(4).max(20);
    let width = (message.width() as u16 + hint.width() as u16 + 6)
        .max(f.area().width / 2)
        .clamp(20, max_w);
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
            title,
            Style::new().fg(theme.red).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height > 0 {
        let lines = wrap_text(message, inner.width.max(1) as usize);
        let hint_line = Line::from(Span::styled(hint, Style::new().fg(theme.yellow)));
        let mut all: Vec<Line<'static>> = lines;
        all.push(hint_line); // rendered last; clipped when out of room
        let text = Text::from(all);
        let visible = inner.height as usize;
        let skip = text.height().saturating_sub(visible);
        let paragraph = Paragraph::new(text).scroll((skip as u16, 0));
        f.render_widget(paragraph, inner);
    }
}

/// Centered modal in-thread search popup. Query input
/// on the first content row, live match list below (`role label + snippet`,
/// selected row highlighted), total count in the title, decision hint last.
/// Purely visual — all key handling lives in `main.rs`.
fn render_search_popup(f: &mut Frame, app: &mut App, theme: &Theme) {
    use ratatui::widgets::{Clear, Padding};

    let Mode::Searching { state } = &app.mode else {
        return;
    };
    let query = state.query.clone();
    let matches = state.matches.clone();
    let selected = state.selected;
    let count = matches.len();

    let hint =
        "\u{2191}\u{2193} navigate \u{00b7} Enter jump \u{00b7} Esc close \u{00b7} Ctrl+C quit";

    // Max match rows shown before the list clips (selection stays visible
    // via a centered window).
    const MAX_LIST_ROWS: usize = 7;

    // Size to content with sane caps; centered on the full frame. Floor of
    // 20 and the `max()` guards keep the clamp ranges valid on tiny
    // terminals (same pattern as the delete popup).
    let max_w = f.area().width.saturating_sub(4).max(20);
    let width = (hint.width() as u16 + 12).max(40).clamp(20, max_w);
    let content_rows = 1 // query row
        + if count == 0 { 1 } else { count.min(MAX_LIST_ROWS) } // matches / no-match row
        + 1; // hint row
    let height = (content_rows as u16 + 2) // + borders
        .min(f.area().height.saturating_sub(2))
        .max(3);
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
        .border_style(Style::new().fg(theme.blue))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" Find in thread \u{00b7} {count} "),
            Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    // Query row: `❯` marker + typed query (or a dim placeholder when empty).
    let mut rows: Vec<Line<'static>> = Vec::new();
    if query.is_empty() {
        rows.push(Line::from(vec![
            Span::styled(PROMPT_MARKER, Style::new().fg(theme.cyan)),
            Span::styled("type to search\u{2026}", Style::new().fg(theme.dim)),
        ]));
    } else {
        rows.push(Line::from(vec![
            Span::styled(PROMPT_MARKER, Style::new().fg(theme.cyan)),
            Span::styled(query.clone(), Style::new().fg(theme.fg)),
        ]));
    }

    // Match rows: `▸ role · …snippet…` with the selected row highlighted.
    // The window is centered on the selection so ↑/↓ navigation stays
    // visible in the fixed-height list.
    if count == 0 {
        if !query.is_empty() {
            rows.push(Line::from(Span::styled(
                "no matches",
                Style::new().fg(theme.dim),
            )));
        }
    } else {
        let half = MAX_LIST_ROWS / 2;
        let window_start = selected
            .saturating_sub(half)
            .min(count.saturating_sub(MAX_LIST_ROWS));
        for (i, m) in matches
            .iter()
            .enumerate()
            .skip(window_start)
            .take(MAX_LIST_ROWS)
        {
            let is_selected = i == selected;
            let role_label = match app
                .chats
                .get(app.active)
                .and_then(|c| c.messages.get(m.message_index))
                .map(|msg| msg.role)
            {
                Some(Role::User) => ("You", theme.orange),
                Some(Role::Assistant) => ("Chibi", theme.blue),
                None => ("?", theme.dim),
            };
            let snippet = search_snippet(m, &query);
            let row_style = if is_selected {
                Style::new().bg(theme.selection)
            } else {
                Style::new()
            };
            rows.push(Line::from(vec![
                Span::styled(
                    if is_selected { "\u{25b8} " } else { "  " },
                    row_style.fg(theme.yellow),
                ),
                Span::styled(
                    format!("{} \u{00b7} ", role_label.0),
                    row_style.fg(role_label.1).add_modifier(Modifier::BOLD),
                ),
                Span::styled(snippet, row_style.fg(theme.fg)),
            ]));
        }
    }

    // Hint row (rendered last; clipped when out of room).
    rows.push(Line::from(Span::styled(
        hint,
        Style::new().fg(theme.yellow),
    )));

    let text = Text::from(rows);
    let visible = inner.height as usize;
    let skip = text.height().saturating_sub(visible);
    let paragraph = Paragraph::new(text).scroll((skip as u16, 0));
    f.render_widget(paragraph, inner);
}

/// Centered modal ALL-threads search popup.
///
/// Same popup family as [`render_search_popup`] — theme-driven, same hint
/// line, same centered windowed list — but visually distinguishable: the
/// title reads "Search all threads" and carries BOTH the total match count
/// and the searched-thread count, and every match row is prefixed with its
/// THREAD TITLE before the role label + snippet. Match rows may belong to
/// ANY chat, so role lookup uses each match's own `chat_index` — never the
/// active chat. Purely visual — all key handling lives in `main.rs`.
fn render_search_all_popup(f: &mut Frame, app: &mut App, theme: &Theme) {
    use ratatui::widgets::{Clear, Padding};

    let Mode::SearchingAll { state } = &app.mode else {
        return;
    };
    let query = state.query.clone();
    let matches = state.matches.clone();
    let selected = state.selected;
    let count = matches.len();
    let thread_count = app.chats.len();

    let hint =
        "\u{2191}\u{2193} navigate \u{00b7} Enter jump \u{00b7} Esc close \u{00b7} Ctrl+C quit";

    // Max match rows shown before the list clips (selection stays visible
    // via a centered window).
    const MAX_LIST_ROWS: usize = 7;

    // Size to content with sane caps; centered on the full frame. Floor of
    // 20 and the `max()` guards keep the clamp ranges valid on tiny
    // terminals (same pattern as the other popups). Slightly wider minimum
    // than the in-thread popup to give `thread · role · snippet` room.
    let max_w = f.area().width.saturating_sub(4).max(20);
    let width = (hint.width() as u16 + 12).max(56).clamp(20, max_w);
    let content_rows = 1 // query row
        + if count == 0 { 1 } else { count.min(MAX_LIST_ROWS) } // matches / no-match row
        + 1; // hint row
    let height = (content_rows as u16 + 2) // + borders
        .min(f.area().height.saturating_sub(2))
        .max(3);
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
        .border_style(Style::new().fg(theme.blue))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(
                " Search all threads \u{00b7} {count} ({thread_count} {}) ",
                if thread_count == 1 {
                    "thread"
                } else {
                    "threads"
                }
            ),
            Style::new().fg(theme.blue).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    // Query row: `❯` marker + typed query (or a dim placeholder when
    // empty). The placeholder text names the global scope so the two search
    // popups are distinguishable even before typing.
    let mut rows: Vec<Line<'static>> = Vec::new();
    if query.is_empty() {
        rows.push(Line::from(vec![
            Span::styled(PROMPT_MARKER, Style::new().fg(theme.cyan)),
            Span::styled(
                "type to search all threads\u{2026}",
                Style::new().fg(theme.dim),
            ),
        ]));
    } else {
        rows.push(Line::from(vec![
            Span::styled(PROMPT_MARKER, Style::new().fg(theme.cyan)),
            Span::styled(query.clone(), Style::new().fg(theme.fg)),
        ]));
    }

    // Match rows: `▸ thread · role · …snippet…` with the selected row
    // highlighted. The window is centered on the selection so ↑/↓
    // navigation stays visible in the fixed-height list. Every match
    // carries its own chat_index, and matches may belong to ANY thread.
    if count == 0 {
        if !query.is_empty() {
            rows.push(Line::from(Span::styled(
                "no matches",
                Style::new().fg(theme.dim),
            )));
        }
    } else {
        let half = MAX_LIST_ROWS / 2;
        let window_start = selected
            .saturating_sub(half)
            .min(count.saturating_sub(MAX_LIST_ROWS));
        for (i, m) in matches
            .iter()
            .enumerate()
            .skip(window_start)
            .take(MAX_LIST_ROWS)
        {
            let is_selected = i == selected;
            let role_label = match app
                .chats
                .get(m.chat_index)
                .and_then(|c| c.messages.get(m.message_index))
                .map(|msg| msg.role)
            {
                Some(Role::User) => ("You", theme.orange),
                Some(Role::Assistant) => ("Chibi", theme.blue),
                None => ("?", theme.dim),
            };
            let snippet = search_snippet_all(m, &query);
            let row_style = if is_selected {
                Style::new().bg(theme.selection)
            } else {
                Style::new()
            };
            // Multi-line titles collapse to spaces in the one-line popup
            // row (Shift+Enter allows `\n` in names).
            let title = m.chat_title.replace('\n', " ");
            rows.push(Line::from(vec![
                Span::styled(
                    if is_selected { "\u{25b8} " } else { "  " },
                    row_style.fg(theme.yellow),
                ),
                Span::styled(
                    format!("{title} \u{00b7} "),
                    row_style.fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{} \u{00b7} ", role_label.0),
                    row_style.fg(role_label.1).add_modifier(Modifier::BOLD),
                ),
                Span::styled(snippet, row_style.fg(theme.fg)),
            ]));
        }
    }

    // Hint row (rendered last; clipped when out of room).
    rows.push(Line::from(Span::styled(
        hint,
        Style::new().fg(theme.yellow),
    )));

    let text = Text::from(rows);
    let visible = inner.height as usize;
    let skip = text.height().saturating_sub(visible);
    let paragraph = Paragraph::new(text).scroll((skip as u16, 0));
    f.render_widget(paragraph, inner);
}

/// Shared snippet core: short context window around a search hit at `col`
/// inside `line_text`, with elided ends: `…beforeHITafter…`. `query` is the
/// query text; `window` is the number of chars kept on each side. Used by
/// both the in-thread and the all-threads search popup.
fn search_snippet_from(line_text: &str, col: usize, query: &str) -> String {
    const WINDOW: usize = 16;
    let chars: Vec<char> = line_text.chars().collect();
    let hit_len = query.chars().count().max(1);
    let start = col.saturating_sub(WINDOW);
    let end = (col + hit_len + WINDOW).min(chars.len());
    let mut s = String::new();
    if start > 0 {
        s.push('\u{2026}');
    }
    s.extend(&chars[start..end]);
    if end < chars.len() {
        s.push('\u{2026}');
    }
    s
}

/// Short context window around an in-thread search hit.
fn search_snippet(m: &crate::app::SearchMatch, query: &str) -> String {
    search_snippet_from(&m.line_text, m.col, query)
}

/// Short context window around an all-threads search hit.
fn search_snippet_all(m: &crate::app::GlobalSearchMatch, query: &str) -> String {
    search_snippet_from(&m.line_text, m.col, query)
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

/// expand every logical markdown line into the
/// display rows it actually occupies at `max_width` columns.
pub fn wrap_message_rows(lines: &[markdown::MdLine], max_width: usize) -> Vec<markdown::MdLine> {
    wrap_message_rows_indexed(lines, max_width).0
}

/// like [`wrap_message_rows`], but ALSO returns, for
/// every logical input line, the index of its FIRST wrapped display row.
///
/// The jump math (and its regression tests) must use the SAME totals as
/// rendering — this is the single source of truth for both. Existing
/// callers of [`wrap_message_rows`] keep identical behavior (the mapping
/// vec is simply dropped there).
pub fn wrap_message_rows_indexed(
    lines: &[markdown::MdLine],
    max_width: usize,
) -> (Vec<markdown::MdLine>, Vec<usize>) {
    let (rows, first_row_of, _) = wrap_message_rows_full(lines, max_width);
    (rows, first_row_of)
}

/// The full wrap of the transcript, PLUS per-display-row hit-test
/// metadata ([`crate::app::ChatRowMeta`]): the owning logical line, the
/// row's char range within it and the row's plain text. The mouse
/// selection consumes the meta for hit-testing, highlighting and
/// plain-text extraction — built from the renderer's OWN wrap, so
/// hit-testing and painting can never disagree.
pub fn wrap_message_rows_full(
    lines: &[markdown::MdLine],
    max_width: usize,
) -> (
    Vec<markdown::MdLine>,
    Vec<usize>,
    Vec<crate::app::ChatRowMeta>,
) {
    let mut rows: Vec<markdown::MdLine> = Vec::new();
    let mut first_row_of = Vec::with_capacity(lines.len());
    let mut meta: Vec<crate::app::ChatRowMeta> = Vec::new();
    for (logical, line) in lines.iter().enumerate() {
        first_row_of.push(rows.len());
        let (line_rows, ranges) = wrap_line_rows_indexed(line, max_width);
        for (row, &(start, end)) in line_rows.iter().zip(ranges.iter()) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            meta.push(crate::app::ChatRowMeta {
                logical,
                start,
                end,
                text,
            });
        }
        rows.extend(line_rows);
    }
    (rows, first_row_of, meta)
}

/// Overlay the mouse selection on the wrapped display rows: chars inside
/// the ordered (anchor → head) range take a REVERSED modifier ON TOP of
/// their own span style, so the markdown/syntect coloring of the
/// unselected text is preserved exactly. Rows without selected chars stay
/// byte-identical. Selection rows beyond the current content (messages
/// changed mid-selection) are clamped; a selection char range past a
/// row's length clamps at the row end.
fn apply_selection_highlight(rows: &mut [markdown::MdLine], sel: &crate::app::ChatSelection) {
    let (lo, hi) = sel.ordered();
    if rows.is_empty() {
        return;
    }
    let last = rows.len() - 1;
    let (lo_row, hi_row) = (lo.row.min(last), hi.row.min(last));
    for (r, row) in rows
        .iter_mut()
        .enumerate()
        .skip(lo_row)
        .take(hi_row - lo_row + 1)
    {
        // Effective per-char style, same precedence the wrap applies
        // (line style patched under span style).
        let line_style = row.style;
        let chars: Vec<(char, Style)> = row
            .spans
            .iter()
            .flat_map(|span| {
                let style = line_style.patch(span.style);
                span.content.chars().map(move |ch| (ch, style))
            })
            .collect();
        let len = chars.len();
        let start = if r == lo_row { lo.col.min(len) } else { 0 };
        let end = if r == hi_row { hi.col.min(len) } else { len };
        if end <= start {
            continue;
        }
        // Rebuild the row as styled runs; selected runs add REVERSED.
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(row.spans.len() + 1);
        let mut run = String::new();
        let mut run_style: Option<Style> = None;
        for (i, (ch, style)) in chars.iter().enumerate() {
            let style = if i >= start && i < end {
                style.add_modifier(Modifier::REVERSED)
            } else {
                *style
            };
            match run_style {
                Some(prev) if prev == style => run.push(*ch),
                Some(prev) => {
                    spans.push(Span::styled(std::mem::take(&mut run), prev));
                    run.push(*ch);
                    run_style = Some(style);
                }
                None => {
                    run.push(*ch);
                    run_style = Some(style);
                }
            }
        }
        if let Some(style) = run_style {
            spans.push(Span::styled(run, style));
        }
        *row = Line::from(spans).style(line_style);
    }
}

/// Map a search hit `(message_index, line_index, col)` — as produced by
/// [`crate::app::collect_search_matches`] — onto its exact WRAPPED row
/// within the chat pane's display-row list.
///
/// The logical line of the hit is the message's `start` (role header) + 1
/// (the header line itself) + `line_index` (0-based within the message's
/// rendered lines). The hit's CHAR offset inside that line is then mapped
/// through the renderer's own wrap ([`wrapped_row_of_char`]), so a hit deep
/// inside a visually-wrapped paragraph lands on the CONTINUATION row that
/// actually shows it — never on the paragraph's first row. Returns `None`
/// when the coordinates are stale (message deleted or line count changed
/// after the popup closed) — the caller then keeps the current scroll
/// instead of jumping blind.
fn search_jump_wrapped_row(
    msg_ranges: &[(usize, usize)],
    first_row_of: &[usize],
    lines: &[markdown::MdLine],
    width: usize,
    message_index: usize,
    line_index: usize,
    char_offset: usize,
) -> Option<usize> {
    let (start, end) = *msg_ranges.get(message_index)?;
    let logical = start + 1 + line_index; // +1 = the role header line
    if logical >= end {
        return None; // stale hit (message changed since the popup closed)
    }
    let first = *first_row_of.get(logical)?;
    Some(first + wrapped_row_of_char(&lines[logical], char_offset, width))
}

/// Rows of padding between the top edge and a jumped-to search match.
const SEARCH_JUMP_PADDING: usize = 1;

/// Convert a target wrapped row into the app's `scroll` value (rows up from
/// the bottom; `0` = follow-bottom) such that the target row sits near the
/// TOP of the visible pane with [`SEARCH_JUMP_PADDING`] rows of padding.
/// Saturating math keeps the result within `0..=max_scroll`, so a hit in
/// the last visible rows naturally lands at follow-bottom.
fn scroll_for_search_jump(target_row: usize, total: usize, visible: u16) -> u16 {
    let visible = visible.max(1) as usize;
    let max_scroll = total.saturating_sub(visible);
    let skip = target_row
        .saturating_sub(SEARCH_JUMP_PADDING)
        .min(max_scroll);
    (max_scroll - skip) as u16
}

/// Expand ONE logical markdown line into the DISPLAY ROWS ratatui paints at
/// `max_width` columns (the row-only view of [`wrap_line_rows_indexed`],
/// kept for the wrap-behavior tests).
///
/// Greedy word-wrap over a flattened `(char, style)` stream so styling
/// survives a mid-span break; unicode-width keeps wide glyphs (CJK, emoji,
/// box-drawing) from straddling a row boundary and makes byte length
/// irrelevant to layout. Whitespace runs inside a row are preserved
/// verbatim; a break trims only the spaces that caused it (the visible part
/// of ratatui's former `Wrap { trim: false }` behavior). Empty input yields
/// exactly one blank row, matching an empty `Paragraph`.
#[cfg(test)]
fn wrap_line_rows(line: &markdown::MdLine, max_width: usize) -> Vec<markdown::MdLine> {
    wrap_line_rows_indexed(line, max_width).0
}

/// Like the test-only `wrap_line_rows`, but ALSO returns, per output row, the half-open
/// `(start, end)` CHAR range of the original line it covers
/// (mapping a search hit's char offset onto the exact wrapped row the
/// renderer paints it on). Dropped break-triggering spaces
/// are covered by no row, so ranges may have gaps — matching the wrap
/// exactly. Existing callers of [`wrap_message_rows`] keep identical behavior.
fn wrap_line_rows_indexed(
    line: &markdown::MdLine,
    max_width: usize,
) -> (Vec<markdown::MdLine>, Vec<(usize, usize)>) {
    let max_width = max_width.max(1);

    // Flatten per char; effective style = line style patched under span
    // style (same precedence ratatui applies at render time).
    let flat: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = line.style.patch(span.style);
            span.content.chars().map(move |ch| (ch, style))
        })
        .collect();

    struct Seg {
        text: String,
        style: Style,
    }

    fn push_char(segs: &mut Vec<Seg>, ch: char, style: Style) {
        match segs.last_mut() {
            Some(seg) if seg.style == style => seg.text.push(ch),
            _ => segs.push(Seg {
                text: ch.to_string(),
                style,
            }),
        }
    }

    /// Contiguous styled segments of a char range (full-row flushes).
    fn segs_of(chunk: &[(char, Style)]) -> Vec<Seg> {
        let mut segs: Vec<Seg> = Vec::new();
        for &(ch, st) in chunk {
            push_char(&mut segs, ch, st);
        }
        segs
    }

    fn into_md(segs: Vec<Seg>) -> markdown::MdLine {
        if segs.is_empty() {
            return Line::from("");
        }
        Line::from(
            segs.into_iter()
                .map(|seg| Span::styled(seg.text, seg.style))
                .collect::<Vec<Span<'static>>>(),
        )
    }

    /// Longest char prefix of `chunk` fitting in `room` display columns.
    /// Returns `(taken_chars, taken_width)`; a single glyph wider than the
    /// whole row is still taken alone (ratatui clips it horizontally).
    fn take_fitting(chunk: &[(char, Style)], room: usize) -> (usize, usize) {
        let (mut take, mut w) = (0usize, 0usize);
        for &(ch, _) in chunk {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + cw > room && take > 0 {
                break;
            }
            w += cw;
            take += 1;
        }
        (take, w)
    }

    fn width_of(chunk: &[(char, Style)]) -> usize {
        chunk
            .iter()
            .map(|&(ch, _)| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
            .sum()
    }

    if flat.is_empty() {
        return (vec![Line::from("")], vec![(0, 0)]);
    }

    let is_space = |ch: char| ch == ' ';
    let mut rows: Vec<Vec<Seg>> = Vec::new();
    let mut row_ranges: Vec<(usize, usize)> = Vec::new();
    let mut cur: Vec<Seg> = Vec::new();
    let mut cur_w = 0usize;
    // Char range of the row currently being built, in ORIGINAL-line indices.
    let mut cur_start = 0usize;
    let mut cur_end = 0usize;

    // Space runs are soft break points; word runs never split unless they
    // cannot fit on a row of their own.
    let mut i = 0usize;
    while i < flat.len() {
        // ---- space run before the next word ----
        let sp_start = i;
        while i < flat.len() && is_space(flat[i].0) {
            i += 1;
        }
        let spaces = &flat[sp_start..i];
        if i >= flat.len() {
            // Trailing whitespace only: keep what fits, trim the rest.
            let room = max_width.saturating_sub(cur_w);
            let (cnt, _) = take_fitting(spaces, room);
            for &(ch, st) in &spaces[..cnt] {
                push_char(&mut cur, ch, st);
            }
            cur_end = sp_start + cnt;
            break;
        }

        // ---- word run ----
        let wd_start = i;
        while i < flat.len() && !is_space(flat[i].0) {
            i += 1;
        }
        let word = &flat[wd_start..i];
        let (sp_w, word_w) = (width_of(spaces), width_of(word));

        if cur_w + sp_w + word_w <= max_width {
            for &(ch, st) in spaces {
                push_char(&mut cur, ch, st);
            }
            for &(ch, st) in word {
                push_char(&mut cur, ch, st);
            }
            cur_w += sp_w + word_w;
            cur_end = i;
            continue;
        }

        // Break before the word: flush the current row; the pending spaces
        // that triggered the break are dropped (covered by no row).
        if !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            row_ranges.push((cur_start, sp_start));
            cur_w = 0;
        }

        if word_w > max_width {
            // Hard-split an oversized token across full rows.
            let mut pos = wd_start;
            loop {
                let rest = &flat[pos..i];
                let rest_w = width_of(rest);
                if rest_w <= max_width {
                    cur_start = pos;
                    for &(ch, st) in rest {
                        push_char(&mut cur, ch, st);
                    }
                    cur_w += rest_w;
                    cur_end = i;
                    break;
                }
                let (take, _) = take_fitting(rest, max_width);
                debug_assert!(take > 0, "take_fitting never returns 0");
                rows.push(segs_of(&rest[..take]));
                row_ranges.push((pos, pos + take));
                pos += take;
            }
        } else {
            cur_start = wd_start;
            for &(ch, st) in word {
                push_char(&mut cur, ch, st);
            }
            cur_w += word_w;
            cur_end = i;
        }
    }

    if !cur.is_empty() || rows.is_empty() {
        if cur.is_empty() {
            // Empty whole-line case (defensive; flat non-empty always yields
            // content above, but keep the empty-row contract).
            row_ranges.push((0, 0));
        } else {
            row_ranges.push((cur_start, cur_end.max(cur_start)));
        }
        rows.push(cur);
    }

    (rows.into_iter().map(into_md).collect(), row_ranges)
}

/// Map a char offset inside a logical line to the 0-based index of the
/// wrapped display row containing it (jump math).
///
/// Uses the renderer's OWN span-aware wrap (via
/// [`wrap_line_rows_indexed`]), so the answer is the exact row the renderer
/// paints that char on — never a second approximation. Offsets inside
/// dropped break-triggering spaces resolve to the row the following word
/// landed on (the visible content starts there); past-the-end offsets clamp
/// to the last row.
fn wrapped_row_of_char(line: &markdown::MdLine, char_offset: usize, max_width: usize) -> usize {
    let (_, ranges) = wrap_line_rows_indexed(line, max_width);
    for (i, &(_, end)) in ranges.iter().enumerate() {
        if char_offset < end {
            return i;
        }
    }
    ranges.len().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Chat};
    use crate::mock;
    use crate::model::{ChatLifecycle, Message};
    use crate::protocol::AgentEventKind;
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

    fn plain_line(s: &str) -> markdown::MdLine {
        Line::from(s.to_owned())
    }

    /// greedy wrap keeps words intact and never
    /// lets a row exceed the width budget.
    #[test]
    fn wrap_line_rows_wraps_words_without_exceeding_width() {
        let rows = wrap_line_rows(&plain_line("alpha beta gamma delta"), 10);
        let texts: Vec<String> = rows
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(texts, vec!["alpha beta", "gamma", "delta"]);
    }

    /// Indentation and intra-row whitespace runs survive untouched — they are
    /// part of the content (code blocks relied on this).
    #[test]
    fn wrap_line_rows_preserves_indentation_and_inner_spaces() {
        let src = "    return   x";
        let rows = wrap_line_rows(&plain_line(src), 30);
        assert_eq!(rows.len(), 1);
        let got: String = rows[0]
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect::<String>();
        assert_eq!(got, src);
    }

    /// Wide-glyph regression: CJK glyphs are 2 columns wide regardless of
    /// their 3-byte UTF-8 length; they split cleanly BETWEEN glyphs.
    #[test]
    fn wrap_line_rows_splits_cjk_on_column_boundaries_not_bytes() {
        // "你好世界" = 12 bytes, 8 display columns.
        let rows = wrap_line_rows(&plain_line("\u{4f60}\u{597d}\u{4e16}\u{754c}"), 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].spans[0].content, "\u{4f60}\u{597d}");
        assert_eq!(rows[1].spans[0].content, "\u{4e16}\u{754c}");
    }

    /// Emoji (2 columns) never straddles the right edge of a row.
    #[test]
    fn wrap_line_rows_never_straddles_emoji_across_boundary() {
        // Three hourglass U+23F3 glyphs = 6 columns; budget 5.
        let rows = wrap_line_rows(&plain_line("\u{23f3}\u{23f3}\u{23f3}"), 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].spans[0].content, "\u{23f3}\u{23f3}");
        assert_eq!(rows[1].spans[0].content, "\u{23f3}");
    }

    /// An unbreakable token longer than the budget hard-splits across rows.
    #[test]
    fn wrap_line_rows_hard_splits_overlong_token() {
        let rows = wrap_line_rows(&plain_line(&"A".repeat(25)), 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].spans[0].content, "AAAAAAAAAA");
        assert_eq!(rows[1].spans[0].content, "AAAAAAAAAA");
        assert_eq!(rows[2].spans[0].content, "AAAAA");
    }

    /// Per-span styling survives a break: each output row keeps the style of
    /// the characters it carries.
    #[test]
    fn wrap_line_rows_keeps_span_style_after_break() {
        let line = Line::from(vec![
            Span::styled("RED", Style::new().fg(ratatui::style::Color::Red)),
            Span::raw(" "),
            Span::styled("BLUE", Style::new().fg(ratatui::style::Color::Blue)),
        ]);
        let rows = wrap_line_rows(&line, 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].spans[0].content, "RED");
        assert_eq!(rows[0].spans[0].style.fg, Some(ratatui::style::Color::Red));
        assert_eq!(rows[1].spans[0].content, "BLUE");
        assert_eq!(rows[1].spans[0].style.fg, Some(ratatui::style::Color::Blue));
    }

    /// Empty / whitespace-only logical lines still occupy exactly one row.
    #[test]
    fn wrap_line_rows_empty_line_yields_one_blank_row() {
        assert_eq!(wrap_line_rows(&plain_line(""), 80).len(), 1);
        assert_eq!(wrap_line_rows(&Line::from(Vec::<Span>::new()), 80).len(), 1);
        let rows = wrap_line_rows(&plain_line("   "), 2);
        assert_eq!(rows.len(), 1);
    }

    //------------------------------------------------------------------------
    const TAIL_SENTINEL: &str = "ENDOFREPLY7X";
    const HEAD_SENTINEL: &str = "GREETING_A1";

    /// Single-line markdown paragraph — one LOGICAL line, many DISPLAY rows
    /// once wrapped at ~94 chat columns.
    fn long_paragraph(seed: &str, sentences: usize) -> String {
        let sentence = format!("{seed}-ish plain wrapping filler for scroll math. ");
        sentence.repeat(sentences) + seed
    }

    fn grid_contains(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|r| r.contains(needle))
    }

    /// Regression (code_block_empty_render_bug): a fenced block containing a
    /// line WIDER than the panel's clamped interior (~70 cols) must CLIP the
    /// line to the interior, not drop it whole. The old token loop `break`-ed
    /// on the first chunk that did not fit, so an over-long line rendered as
    /// a fully BLANK interior row inside the frame (language badge and
    /// borders present, zero code text) — exactly the user-reported empty
    /// tsx box.
    #[test]
    fn code_block_long_line_clips_into_panel_not_blank_row() {
        let mut app = App::new(vec![Chat::new("code")]);
        let long_line =
            "<Item label=\"Alpha sector lightweight visible panel trim kit\" value={42} />";
        assert!(long_line.chars().count() > 70);
        let md = format!("```tsx\nconst a = 1;\nconst b = 2;\n{long_line}\n```\n");
        app.chats[0].messages.push(Message::assistant(md));

        let (rows, _) = render_grid_with_buffer(&mut app);

        // Short body lines must be fully visible inside the panel.
        assert!(
            grid_contains(&rows, "const a = 1;"),
            "short code line missing: {:?}",
            rows.iter()
                .filter(|r| r.contains('\u{2502}'))
                .collect::<Vec<_>>()
        );
        assert!(
            grid_contains(&rows, "const b = 2;"),
            "short code line missing: {:?}",
            rows.iter()
                .filter(|r| r.contains('\u{2502}'))
                .collect::<Vec<_>>()
        );
        // The over-long line must show its CLIPPED PREFIX, never a blank row.
        let prefix = &long_line[..24];
        assert!(
            grid_contains(&rows, prefix),
            "over-long code line rendered as a blank interior row (prefix {prefix:?} absent)"
        );
    }

    /// THE regression: with several exchanges plus a long wrapped final
    /// reply, follow-bottom must show the END of the last reply above the
    /// input band. The old logical-line total clipped it away.
    #[test]
    fn follow_bottom_shows_end_of_last_wrapped_reply() {
        let mut app = App::new(vec![Chat::new("scroll")]);
        {
            let chat = &mut app.chats[0];
            chat.messages.push(Message::user("first question"));
            // First reply anchors the TRUE top of the content.
            chat.messages.push(Message::assistant(format!(
                "{HEAD_SENTINEL} short ack zero"
            )));
            for n in 1..6 {
                chat.messages
                    .push(Message::user(format!("filler question {n}")));
                chat.messages
                    .push(Message::assistant(format!("short ack {n}")));
            }
            chat.messages.push(Message::user("final question"));
            chat.messages.push(Message::assistant(
                long_paragraph("wrapped", 60) + TAIL_SENTINEL,
            ));
        }

        // Follow-bottom (scroll untouched): tail sentinel must be on screen.
        let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 20);
        assert!(
            grid_contains(&rows, TAIL_SENTINEL),
            "follow-bottom hid the end of the newest wrapped reply"
        );

        // One page up, then far enough that the clamp saturates at
        // max_scroll: the view must reach the absolute top of the WRAPPED
        // content, and nothing from below may leak into that view.
        let page = app.chat_visible_rows.max(1);
        app.scroll_up(page);
        let _ = render_grid_at_with_buffer(&mut app, 120, 20); // refresh page size
        app.scroll_up(u16::MAX / 2);
        let (rows_top, _) = render_grid_at_with_buffer(&mut app, 120, 20);
        assert!(
            grid_contains(&rows_top, HEAD_SENTINEL),
            "max-scroll-up missed the true top of wrapped content"
        );
        assert!(
            !grid_contains(&rows_top, TAIL_SENTINEL),
            "top view shows rows that belong far below"
        );
    }

    /// Wide-glyph variant: totals must derive from unicode-width columns, not
    /// byte lengths, or a CJK/emoji-heavy reply clips its tail.
    #[test]
    fn follow_bottom_shows_end_of_wide_glyph_reply() {
        let mut app = App::new(vec![Chat::new("wide")]);
        app.chats[0].messages.push(Message::assistant(format!(
            "{}{}",
            "\u{4e16}\u{754c}\u{30ec}\u{30d9}\u{30eb} filler ".repeat(120),
            "\u{7d42}\u{7aef}9X" // 終端 + ASCII digits sentinel
        )));
        let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 20);
        // Ratatui paints a 2-column glyph into its leading cell and pads the
        // trailing skip-cell with a blank — collapse blanks before matching.
        let squeezed: Vec<String> = rows.iter().map(|r| r.replace(' ', "")).collect();
        assert!(
            grid_contains(&squeezed, "\u{7d42}\u{7aef}9X"),
            "wide-glyph reply tail hidden in follow-bottom mode"
        );
    }

    /// Overflow hint fires iff WRAPPED rows exceed the visible pane — even
    /// when the logical line count fits comfortably.
    #[test]
    fn overflow_hint_appears_iff_wrapped_rows_exceed_visible() {
        // 120×14 ⇒ main area 11 rows, spinner row y=11, chat inner height 10.
        let mut app = App::new(vec![Chat::new("hint")]);
        app.chats[0].messages.push(Message::user("q"));
        // One huge unbreakable token: 3 LOGICAL lines (~45 display rows).
        app.chats[0]
            .messages
            .push(Message::assistant("X".repeat(4000)));
        let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 14);
        assert!(
            rows[11].contains("more"),
            "wrapped overflow (>10 rows) must raise the hint"
        );

        let mut calm = App::new(vec![Chat::new("calm")]);
        calm.chats[0].messages.push(Message::assistant("tiny"));
        let (rows_calm, _) = render_grid_at_with_buffer(&mut calm, 120, 14);
        assert!(
            !rows_calm[11].contains("more"),
            "hint raised although content fits"
        );
    }

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
        // Status hints pinned below the band (compacted ^N token).
        assert!(rows.last().unwrap().contains("^N"));

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

        // Exactly ONE chip glyph sequence anywhere in the frame (the hints
        // bar spells no send chip at all).
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

    /// Chat transcript surface carries `theme.bg` across the FULL chat pane —
    /// top-border row, empty transcript space and the pane's right margin
    /// column included — while sidebar/divider keep `theme.panel` and the
    /// input zone keeps `theme.input_panel_bg` (the app-painted
    /// panel < chat < input ladder, independent of the terminal profile).
    #[test]
    fn chat_surface_tints_full_column_with_theme_bg() {
        let theme = Theme::tokyo_night();
        // A fresh empty chat: every inner cell is blank transcript space —
        // exactly the surface the terminal background used to show through.
        let mut app = App::new(vec![Chat::new("chat")]);

        let (_, buf) = render_grid_with_buffer(&mut app);
        // Pane spans x 26..=119; the 1-row editor block puts the pane at
        // rows 0..=30 (row 31 spinner, 32 input, 33 hints).
        for (x, y) in [
            (26u16, 0u16),
            (60, 0),
            (119, 0),
            (26, 15),
            (60, 15),
            (119, 15),
            (118, 30),
        ] {
            assert_eq!(
                buf[(x, y)].bg,
                theme.bg,
                "chat surface not tinted at ({x},{y})"
            );
        }
        // Neighbors keep their own surfaces: sidebar + divider panel, input
        // zone panel tint, input-band margin cell untouched by either fill.
        assert_eq!(buf[(10, 15)].bg, theme.panel, "sidebar must stay panel");
        assert_eq!(buf[(25, 15)].bg, theme.panel, "divider must stay panel");
        assert_eq!(
            buf[(60, 32)].bg,
            theme.input_panel_bg,
            "input zone tint lost"
        );
        assert_ne!(
            buf[(119, 32)].bg,
            theme.bg,
            "input margin must not take chat tint"
        );
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
            rows.last().unwrap().contains("^N"),
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

    //------------------------------------------------------------------------

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

    // subagent counter in the spinner line -----------

    /// While the active chat reports live subagents, the spinner line gains
    /// a ` · subagents working: n` segment; without one the line stays
    /// byte-for-byte unchanged.
    #[test]
    fn spinner_line_appends_subagents_working_counter() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0].lifecycle = ChatLifecycle::Running {
            request_id: "req-sub".into(),
        };

        let base = spinner_line_of(&mut app);
        assert!(base.contains("thinking"), "precondition: spinner visible");
        assert!(
            !base.contains("subagents"),
            "no counter before any agent_event, got {base:?}"
        );

        app.chats[0].apply_subagent_event(11, AgentEventKind::Started, 2, 5);
        let line = spinner_line_of(&mut app);
        assert_eq!(
            line.trim_end(),
            format!("{} \u{00b7} subagents working: 2", base.trim_end()),
            "counter must append to the unchanged spinner text, got {line:?}"
        );

        // Fewer live subagents → the rendered count follows the frame values.
        app.chats[0].apply_subagent_event(11, AgentEventKind::Finished, 1, 5);
        let line = spinner_line_of(&mut app);
        assert!(
            line.contains("subagents working: 1"),
            "counter must track the frame values, got {line:?}"
        );
    }

    /// The counter is independent of the request lifecycle: an IDLE chat
    /// whose turn's subagents still run renders the counter alone — no
    /// spinner, no lifecycle label — and hides it once the count hits 0.
    #[test]
    fn spinner_line_shows_subagents_without_a_request() {
        let mut app = App::new(vec![Chat::new("chat")]);
        assert!(
            spinner_line_of(&mut app).trim().is_empty(),
            "precondition: idle chat renders an empty line"
        );

        app.chats[0].apply_subagent_event(11, AgentEventKind::Started, 2, 5);
        let line = spinner_line_of(&mut app);
        assert!(
            line.contains("subagents working: 2"),
            "idle chat must keep the subagent counter visible, got {line:?}"
        );
        assert!(
            !line.contains("thinking") && !line.contains("queued"),
            "no request indicator while idle, got {line:?}"
        );
        assert!(
            !line.contains(app.spinner_char()),
            "the spinner belongs to the request lifecycle only, got {line:?}"
        );

        app.chats[0].apply_subagent_event(11, AgentEventKind::Finished, 0, 5);
        assert!(
            spinner_line_of(&mut app).trim().is_empty(),
            "count 0 must hide the counter again"
        );
    }

    /// A finished (active == 0) entry renders nothing, even while the chat
    /// is busy again.
    #[test]
    fn spinner_line_hides_zero_subagent_counters() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0].lifecycle = ChatLifecycle::Running {
            request_id: "req-sub".into(),
        };

        app.chats[0].apply_subagent_event(11, AgentEventKind::Started, 3, 3);
        app.chats[0].apply_subagent_event(11, AgentEventKind::Finished, 0, 3);
        assert!(
            !spinner_line_of(&mut app).contains("subagents"),
            "finished (active == 0) entry must not render"
        );
    }

    /// Background chats' subagents never leak into the active chat's
    /// spinner line (render-level per-thread isolation, like the lifecycle
    /// dots above).
    #[test]
    fn spinner_line_ignores_background_chat_subagents() {
        let mut focused = Chat::new("focused");
        focused.lifecycle = ChatLifecycle::Running {
            request_id: "req-front".into(),
        };
        let busy = Chat::new("busy");
        let mut app = App::new(vec![focused, busy]);
        app.active = 0;

        app.chats[1].apply_subagent_event(9, AgentEventKind::Started, 4, 4);
        let line = spinner_line_of(&mut app);
        assert!(
            line.contains("thinking") && !line.contains("subagents"),
            "background counter must not leak, got {line:?}"
        );
    }

    //------------------------------------------------------------------------

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

    // render-level checks -------------------------

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

    /// Status hints line lists the thread tools AND the thoughts state
    /// token; `^C cancel` stays readable next to the longest connection
    /// label. The `^S on/off` token joined (the reasoning toggle's
    /// visible feedback + hint); to pay for it the self-evident `⇧↵`
    /// newline hint retired and `^⇧F all` compacted to `^⇧F` (Shift+Enter
    /// is the universal chat-app newline convention and the shifted find
    /// next to `^F` reads as the global search — README documents both, and
    /// the F1 modal is the full on-screen reference).
    #[test]
    fn status_line_lists_rename_and_newline_hints() {
        let mut app = App::new(mock::initial_chats());
        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();

        for needle in [
            "^R",
            "^C cancel",
            "^/\u{2325}\u{2191}\u{2193} chats",
            "F1 help",
            "^D",
            "^F",
            "^T",
            "^O",
            "^S on",
        ] {
            assert!(last.contains(needle), "{needle} missing from {last:?}");
        }
        assert!(
            !last.contains("^T next"),
            "stale cycling hint must be gone: {last:?}"
        );
        assert!(
            !last.contains("caret"),
            "retired caret hint must be gone: {last:?}"
        );
        assert!(
            !last.contains("\u{21e7}\u{21b5}"),
            "retired newline hint must be gone: {last:?}"
        );
        assert!(
            !last.contains("^S off"),
            "default state must read on: {last:?}"
        );
    }

    /// the hints line plus the LONGEST connection
    /// label (`● disconnected (press R)`) must fit one row at 120 columns.
    /// Paragraph clips overflowing content, so the presence of the label's
    /// tail on the rendered row PROVES nothing was cut — an honest fit check.
    #[test]
    fn status_hints_fit_120_cols_with_longest_status_label() {
        let mut app = App::new(mock::initial_chats());
        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();

        assert!(
            last.contains("disconnected (press R)"),
            "status label clipped — hints overflowed 120 cols: {last:?}"
        );
        // Total painted width cannot exceed the frame width.
        assert!(
            last.trim_end().width() <= 120,
            "hints row too wide: {} cols — {:?}",
            last.trim_end().width(),
            last
        );
    }

    /// the input row carries the panel tint background —
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

    // restored canonical render-level checks ------

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

    /// Rename line shares the same tinted treatment: `❯` marker at the
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

    // render-level checks -------------------------

    /// The Ctrl+D confirm popup renders the destructive title, the active
    /// thread's title and the decision hint inside a bordered box.
    #[test]
    fn delete_popup_renders_title_thread_and_hint() {
        let mut app = App::new(vec![Chat::new("Deep Dive")]);
        app.begin_delete_confirm();
        let rows = render_grid(&mut app);
        let flat: String = rows.join("\n");

        assert!(
            flat.contains("Delete thread"),
            "popup title missing:\n{flat}"
        );
        assert!(flat.contains("Deep Dive"), "thread title missing:\n{flat}");
        assert!(flat.contains("y/Enter confirm"), "confirm hint missing");
        assert!(flat.contains("Esc/n cancel"), "cancel hint missing");
        // Boxed: a closed top border row exists.
        assert!(rows.iter().any(|r| r.contains('┌') && r.contains('┐')));
    }

    /// The popup is centered: identical left/right margins on its top row.
    #[test]
    fn delete_popup_is_centered() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.begin_delete_confirm();
        let (_, buf) = render_grid_with_buffer(&mut app);

        let top = (0..buf.area.height)
            .find(|&y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "┌"))
            .expect("popup top border");
        let left = (0..buf.area.width)
            .position(|x| buf[(x, top)].symbol() == "┌")
            .unwrap() as u16;
        let right = (0..buf.area.width)
            .rposition(|x| buf[(x, top)].symbol() == "┐")
            .unwrap() as u16;
        assert_eq!(left, buf.area.width - 1 - right, "popup not centered");
        // ~50% width at the demo resolution: ≥ half the frame.
        let width = right - left + 1;
        assert!(
            width >= buf.area.width / 2,
            "popup narrower than half the frame"
        );
    }

    /// No popup in Normal mode — no overlay artifacts.
    #[test]
    fn no_delete_popup_when_closed() {
        let mut app = App::new(vec![Chat::new("chat")]);
        let flat = render_grid(&mut app).join("\n");
        assert!(!flat.contains("Delete thread"));
    }

    /// Deleting the last chat renders the clean empty state without
    /// panicking: header, sidebar and placeholder input all survive.
    #[test]
    fn empty_state_after_deleting_last_chat_renders_cleanly() {
        let mut app = App::new(vec![Chat::new("solo")]);
        app.begin_delete_confirm();
        app.confirm_delete();
        assert!(app.chats.is_empty());

        let rows = render_grid(&mut app);
        let input_row = &rows[rows.len() - 2];
        assert!(
            input_row.contains("Type a message"),
            "placeholder input must show in the empty state: {input_row:?}"
        );
        assert!(rows.last().unwrap().contains("^N"), "hints intact");
    }

    // dim reasoning block above the answer ------------

    /// With thoughts retained and the toggle ON, the block renders in the
    /// dim slot directly ABOVE the latest answer; ^S hides it while the
    /// answer itself stays untouched.
    #[test]
    fn thoughts_block_renders_dim_above_answer_and_toggle_hides_it() {
        let mut app = App::new(vec![Chat::new("t")]);
        app.chats[0].messages.push(Message::user("question"));
        app.chats[0].messages.push(Message::assistant("the answer"));
        app.chats[0].last_thoughts = Some("reasoning trace line".into());

        let (rows, buf) = render_grid_with_buffer(&mut app);
        let thought_row = rows
            .iter()
            .position(|r| r.contains("reasoning trace line"))
            .expect("thoughts line must render");
        let answer_row = rows
            .iter()
            .position(|r| r.contains("the answer"))
            .expect("answer must render");
        assert!(
            thought_row < answer_row,
            "thoughts block must sit ABOVE the answer"
        );
        // The thought text paints in the dim slot.
        let col = rows[thought_row].find("reasoning").unwrap();
        assert_eq!(
            buf[(col as u16, thought_row as u16)].fg,
            Theme::tokyo_night().dim,
            "thoughts text must be dim"
        );

        app.toggle_thoughts();
        let flat = render_grid(&mut app).join("\n");
        assert!(
            !flat.contains("reasoning trace line"),
            "toggle OFF must hide the block"
        );
        assert!(
            flat.contains("the answer"),
            "answer must survive the toggle"
        );
    }

    /// A CHAIN of thoughts (terminal result + background continuation
    /// deltas) accumulates in the dim block: driving the REAL event path
    /// with three payloads, all three thoughts render above the latest
    /// answer. Regression for the owner report: continuation thoughts used
    /// to overwrite the block, so only one payload of the chain was ever
    /// visible.
    #[test]
    fn thought_chain_renders_all_members_above_the_latest_answer() {
        let mut app = App::new(vec![Chat::new("t")]);
        let thread_id = app.chats[0].id.clone();
        app.chats[0].messages.push(Message::user("do the thing"));
        app.chats[0].messages.push(Message::assistant_pending());
        app.chats[0].lifecycle = ChatLifecycle::Awaiting {
            request_id: "req-chain".into(),
        };

        app.apply_backend_event(crate::backend::BackendEvent::Result {
            request_id: crate::live::wire_thread_id("req-chain") as u64,
            markdown: "step one".into(),
            thread_id: thread_id.clone(),
            model: None,
            usage: None,
            thoughts: Some("first thought".into()),
        });
        for (markdown, thought) in [
            ("step two", "second thought"),
            ("step three", "third thought"),
        ] {
            app.apply_backend_event(crate::backend::BackendEvent::BackgroundMessage {
                wire_thread_id: crate::live::wire_thread_id(&thread_id),
                markdown: markdown.into(),
                model: None,
                thoughts: Some(thought.into()),
            });
        }

        let rows = render_grid(&mut app);
        let flat = rows.join("\n");
        for thought in ["first thought", "second thought", "third thought"] {
            assert!(flat.contains(thought), "chain member missing: {thought}");
        }
        let last_answer_row = rows
            .iter()
            .position(|r| r.contains("step three"))
            .expect("latest answer renders");
        let first_thought_row = rows
            .iter()
            .position(|r| r.contains("first thought"))
            .expect("first chain member renders");
        assert!(
            first_thought_row < last_answer_row,
            "the accumulated chain must sit ABOVE the latest answer"
        );
    }

    /// Absent or whitespace-only thoughts render NOTHING: the transcript is
    /// byte-identical to the no-thoughts baseline (zero layout impact), and
    /// toggle OFF reproduces that baseline even with thoughts retained. The
    /// STATUS line is excluded from the comparison: the `^S on/off` state
    /// token legitimately tracks the toggle there.
    #[test]
    fn thoughts_block_absent_or_blank_renders_nothing() {
        let mut app = App::new(vec![Chat::new("t")]);
        app.chats[0].messages.push(Message::assistant("answer"));

        assert_eq!(app.chats[0].last_thoughts, None);
        let baseline = render_grid(&mut app);
        let baseline = baseline[..baseline.len() - 1].join("\n");
        assert!(baseline.contains("answer"));

        app.chats[0].last_thoughts = Some("  \n  ".into());
        let blank = render_grid(&mut app);
        let blank = blank[..blank.len() - 1].join("\n");
        assert_eq!(
            blank, baseline,
            "whitespace-only thoughts must not change the transcript"
        );

        app.toggle_thoughts();
        app.chats[0].last_thoughts = Some("reasoning trace line".into());
        let hidden = render_grid(&mut app);
        let hidden = hidden[..hidden.len() - 1].join("\n");
        assert_eq!(
            hidden, baseline,
            "toggle OFF must reproduce the no-thoughts layout exactly"
        );
    }

    /// Long traces keep only the LAST 10 lines, with the `…` head marker on
    /// the first kept line; exactly-10 lines render unmarked.
    #[test]
    fn thoughts_block_caps_last_ten_lines_with_ellipsis_head() {
        let mut app = App::new(vec![Chat::new("t")]);
        app.chats[0].messages.push(Message::assistant("answer"));
        app.chats[0].last_thoughts = Some(
            (1..=15)
                .map(|i| format!("thought line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let flat = render_grid(&mut app).join("\n");
        assert!(!flat.contains("thought line 01"), "pre-cap head is dropped");
        assert!(!flat.contains("thought line 05"));
        assert!(
            flat.contains("thought line 06"),
            "first kept (last-10) line"
        );
        assert!(
            flat.contains("thought line 15"),
            "line closest to the answer"
        );
        assert!(
            flat.contains("\u{2026}thought line 06"),
            "ellipsis head marker on the first kept line"
        );
        assert!(
            !flat.contains("\u{2026}thought line 07"),
            "marker only on the head line"
        );

        // Exactly 10 lines: everything visible, no marker.
        app.chats[0].last_thoughts = Some(
            (1..=10)
                .map(|i| format!("thought line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let flat = render_grid(&mut app).join("\n");
        assert!(flat.contains("thought line 01"));
        assert!(
            !flat.contains("\u{2026}thought line 01"),
            "no marker when untruncated"
        );
    }

    /// Each chat renders its OWN reasoning: switching threads swaps the
    /// block to the entered chat's trace and never leaks the other chat's
    /// reasoning into the view (the renderer reads the active chat's
    /// `Chat::last_thoughts` directly).
    #[test]
    fn thoughts_follow_the_active_chat_across_thread_switches() {
        let mut app = App::new(vec![Chat::new("alpha"), Chat::new("beta")]);
        app.chats[0]
            .messages
            .push(Message::assistant("alpha answer"));
        app.chats[1]
            .messages
            .push(Message::assistant("beta answer"));
        app.chats[0].last_thoughts = Some("alpha reasoning line".into());
        app.chats[1].last_thoughts = Some("beta reasoning line".into());

        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.contains("alpha reasoning line"),
            "the active chat's own trace renders: {flat:?}"
        );
        assert!(
            !flat.contains("beta reasoning line"),
            "another chat's reasoning must not leak into the view"
        );

        app.select_next();
        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.contains("beta reasoning line"),
            "switching threads shows the entered chat's trace"
        );
        assert!(
            !flat.contains("alpha reasoning line"),
            "the chat left behind must not bleed into the new view"
        );

        app.select_prev();
        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.contains("alpha reasoning line"),
            "switching back restores the chat's own sticky trace"
        );
        assert!(!flat.contains("beta reasoning line"));
    }

    /// the status line carries the toggle state —
    /// `^S on` by default, `^S off` after the toggle — so Ctrl+S always has
    /// visible feedback (the chord doubles as the row's hint; the F1 modal
    /// spells the action out).
    #[test]
    fn status_line_shows_the_thoughts_toggle_state() {
        let mut app = App::new(vec![Chat::new("t")]);

        let last = render_grid(&mut app).last().unwrap().clone();
        assert!(
            last.contains("^S on"),
            "default state must show on the status line: {last:?}"
        );
        assert!(!last.contains("^S off"));

        app.toggle_thoughts();
        let last = render_grid(&mut app).last().unwrap().clone();
        assert!(
            last.contains("^S off"),
            "the toggle must flip the status indicator: {last:?}"
        );
        assert!(
            !last.contains("^S on"),
            "the stale state must not linger next to the new one: {last:?}"
        );
    }

    // render-level checks -------------------------

    /// The search popup renders the query row, the live match list with role
    /// labels + snippets, and the total count in the title.
    #[test]
    fn search_popup_renders_query_matches_and_count() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0]
            .messages
            .push(Message::user("the needle question"));
        app.chats[0]
            .messages
            .push(Message::assistant("answer with needle inside"));
        app.begin_search();
        for ch in "needle".chars() {
            app.search_push(ch);
        }

        let rows = render_grid(&mut app);
        let flat: String = rows.join("\n");

        assert!(
            flat.contains("Find in thread") && flat.contains("\u{00b7} 2"),
            "title must carry the total count: {flat:?}"
        );
        assert!(flat.contains("\u{276f} needle"), "query row missing");
        assert!(flat.contains("You"), "user role label missing");
        assert!(flat.contains("Chibi"), "assistant role label missing");
        assert!(
            flat.contains("needle question") && flat.contains("needle inside"),
            "snippet context windows missing"
        );
        assert!(flat.contains("Enter jump"), "hint missing");
        // Selected row marker rendered.
        assert!(flat.contains("\u{25b8}"), "selection marker missing");
    }

    /// Empty query state renders gracefully: placeholder query row, no
    /// match rows, no crash, hint still present.
    #[test]
    fn search_popup_empty_query_is_graceful() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0].messages.push(Message::user("needle"));
        app.begin_search();
        let rows = render_grid(&mut app);
        let flat: String = rows.join("\n");

        assert!(
            flat.contains("type to search\u{2026}"),
            "placeholder missing"
        );
        assert!(flat.contains("\u{00b7} 0"), "zero count in title");
        assert!(!flat.contains("\u{25b8}"), "no selection with no matches");
        assert!(flat.contains("Esc close"), "hint intact");
    }

    /// Tiny terminals must not panic while the search popup is open — the
    /// popup box clamps to whatever fits (same guarantee as the delete
    /// popup), and repeated draws stay stable.
    #[test]
    fn search_popup_never_panics_on_tiny_terminal() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0].messages.push(Message::user("needle"));
        app.begin_search();
        for ch in "needle".chars() {
            app.search_push(ch);
        }
        let (rows, _) = render_grid_at_with_buffer(&mut app, 40, 8);
        let flat: String = rows.join("\n");
        assert!(flat.contains("Find in thread"), "popup title present");
        // Second draw after viewport state exists — still no panic.
        let _ = render_grid_at_with_buffer(&mut app, 40, 8);
    }

    /// No popup in Normal mode — no overlay artifacts.
    #[test]
    fn no_search_popup_when_closed() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0].messages.push(Message::user("needle"));
        let flat = render_grid(&mut app).join("\n");
        assert!(!flat.contains("Find in thread"));
    }

    /// Snippet elides long context with `…` on both sides and keeps the hit
    /// visible.
    #[test]
    fn search_snippet_elides_ends() {
        // Long enough that the 16-char window can't cover both ends.
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau";
        let m = crate::app::SearchMatch {
            message_index: 0,
            line_index: 0,
            line_text: text.to_owned(),
            col: 30, // inside "delta"
        };
        let s = search_snippet(&m, "delta");
        assert!(s.contains("delta"), "hit must stay visible: {s:?}");
        assert!(s.starts_with('\u{2026}'), "left elision: {s:?}");
        assert!(s.ends_with('\u{2026}'), "right elision: {s:?}");
        assert!(s.len() <= text.len(), "snippet must be shorter than source");
    }

    // THE wrapped-jump regression ----------------

    /// THE core acceptance criterion: a search hit inside a visually-WRAPPED
    /// paragraph (one logical line spanning many display rows) must jump to
    /// the correct WRAPPED row — the match sits near the top of the viewport
    /// afterwards, not buried at the bottom (which a logical-line-only
    /// approximation would produce) and not at the message's first row.
    #[test]
    fn search_jump_lands_wrapped_match_near_top() {
        // Narrow chat pane forces heavy wrapping of the long paragraph.
        const FRAME_W: u16 = 60;
        const FRAME_H: u16 = 24;
        // Chat pane inner width = frame - sidebar(26); chat inner height =
        // frame - top border(1) - spinner(1) - input(1) - status(1).
        let pane_width = FRAME_W - 26;

        let mut app = App::new(vec![Chat::new("wrap")]);
        {
            let chat = &mut app.chats[0];
            chat.messages.push(Message::user("first question"));
            // A LONG single-paragraph reply: one logical line that wraps to
            // many display rows at this width. The needle sits in the MIDDLE
            // of the paragraph so the jump must land on a continuation row,
            // not the first row of the message.
            let mut para = String::from("alpha filler ");
            for _ in 0..40 {
                para.push_str("filler word ");
            }
            para.push_str("needle42 mid-paragraph ");
            for _ in 0..40 {
                para.push_str("trailing filler ");
            }
            chat.messages.push(Message::assistant(para));
            // Plenty of content BELOW so follow-bottom would hide the match
            // entirely — the jump must detach from the bottom.
            for i in 0..30 {
                chat.messages.push(Message::user(format!("filler q {i}")));
                chat.messages.push(Message::assistant("short ack"));
            }
        }

        // Find the wrapped row of the needle via the renderer's own totals.
        // Rebuild logical lines exactly as render_chat does, then use the
        // SAME indexed wrap helper the renderer consumes.
        let mut lines: Vec<markdown::MdLine> = Vec::new();
        let mut msg_ranges: Vec<(usize, usize)> = Vec::new();
        for msg in &app.chats[0].messages {
            let start = lines.len();
            match msg.role {
                Role::User => {
                    lines.push(Line::from(Span::styled("\u{25cf} You", Style::new())));
                }
                Role::Assistant => {
                    lines.push(Line::from(Span::styled("\u{25cf} Chibi", Style::new())));
                }
            }
            if msg.pending {
                lines.push(Line::from(""));
            } else {
                lines.extend(markdown::render(&msg.markdown, &Theme::tokyo_night()));
            }
            lines.push(Line::from(""));
            msg_ranges.push((start, lines.len()));
        }
        let (_, first_row_of) = wrap_message_rows_indexed(&lines, pane_width as usize);
        // Message 1 (the long paragraph) occupies logical lines
        // [msg_ranges[1].0, msg_ranges[1].1); its rendered line 0 is
        // start + 1 (after the role header).
        let logical = msg_ranges[1].0 + 1;
        let needle_wrapped_row = first_row_of[logical];
        assert!(
            needle_wrapped_row > 1,
            "needle must live on a WRAPPED continuation row, got {needle_wrapped_row}"
        );

        // Full render path: set the pending jump and draw.
        app.begin_search();
        for ch in "needle42".chars() {
            app.search_push(ch);
        }
        let (mi, li, col) = {
            let m = &app.search_matches()[0];
            (m.message_index, m.line_index, m.col)
        };
        assert!(app.jump_to_selected(), "match must be found");
        assert_eq!(
            app.pending_search_jump,
            Some((mi, li, col)),
            "hit in message {mi}, rendered line {li}, char col {col}"
        );

        let (rows, _) = render_grid_at_with_buffer(&mut app, FRAME_W, FRAME_H);
        let y = rows
            .iter()
            .position(|r| r.contains("needle42"))
            .expect("needle must be visible after the jump");
        // Chat pane inner starts at y=1 (top border). "Near top" = within the
        // first few rows of the pane; 1-row padding puts the match at inner
        // row 1 → grid y=2. Allow a small tolerance for border/glyph offsets.
        assert!(
            (1..=5).contains(&y),
            "match must be near the top of the pane, got y={y}: {rows:?}"
        );
        assert!(
            !app.at_bottom(),
            "jump must detach from follow-bottom (match is far above the end)"
        );
        assert!(
            y < rows.len() - 4,
            "match must NOT be at the bottom of the pane"
        );
    }

    /// The wrap helper exposes per-logical-line first-row offsets and keeps
    /// the plain (non-indexed) wrapper's behavior identical.
    #[test]
    fn wrap_message_rows_indexed_matches_plain_wrapper() {
        let lines = vec![
            plain_line("short"),
            plain_line("this line definitely wraps at a narrow width"),
            plain_line(""),
            plain_line("x"),
        ];
        let (rows, first_row_of) = wrap_message_rows_indexed(&lines, 12);
        // "short" → 1 row; the long line wraps to 4 rows; "" → 1; "x" → 1.
        assert_eq!(rows.len(), 7);
        assert_eq!(first_row_of.len(), lines.len());
        assert_eq!(first_row_of[0], 0, "short starts at row 0");
        assert_eq!(first_row_of[1], 1, "long line starts at row 1");
        assert_eq!(first_row_of[2], 5, "blank line starts after the long line");
        assert_eq!(first_row_of[3], 6, "x starts at the last row");
        assert_eq!(
            wrap_message_rows(&lines, 12),
            rows,
            "indexed variant must not change the plain wrapper's output"
        );
    }

    /// Hint compaction stays within budget with the longest status label.
    #[test]
    fn status_hints_still_fit_with_search_hint() {
        let mut app = App::new(mock::initial_chats());
        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();
        assert!(last.contains("^F"), "^F missing from hints: {last:?}");
        assert!(last.trim_end().width() <= 120, "hints row too wide");
    }

    /// A busy-refusal status toast renders in the status line (replacing the
    /// hint block while visible) and always fits with the longest connection
    /// label on one 120-col row.
    #[test]
    fn status_toast_renders_in_status_line() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.connection = Connection::Disconnected;
        app.show_status("can't delete — busy");
        let last = render_grid(&mut app).last().unwrap().clone();

        assert!(
            last.contains("can't delete — busy"),
            "toast missing: {last:?}"
        );
        assert!(
            last.contains("disconnected (press R)"),
            "connection label clipped by toast: {last:?}"
        );
        assert!(last.trim_end().width() <= 120, "toast row too wide");
        // Hints are hidden while the toast is up.
        assert!(!last.contains("^N"), "hints must yield to the toast");
    }

    // render-level checks --------------------

    /// The global search popup renders the query row, the live match list
    /// with THREAD TITLE + role labels + snippets, and the title carries the
    /// total match count AND the searched-thread count.
    #[test]
    fn search_all_popup_renders_thread_titles_matches_and_counts() {
        let mut app = App::new(vec![Chat::new("Alpha"), Chat::new("Beta")]);
        app.chats[0]
            .messages
            .push(Message::user("the needle question"));
        app.chats[1]
            .messages
            .push(Message::assistant("answer with needle inside"));
        app.begin_search_all();
        for ch in "needle".chars() {
            app.search_all_push(ch);
        }

        let rows = render_grid(&mut app);
        let flat: String = rows.join("\n");

        assert!(
            flat.contains("Search all threads")
                && flat.contains("\u{00b7} 2")
                && flat.contains("(2 threads)"),
            "title must carry total count + thread count: {flat:?}"
        );
        assert!(flat.contains("\u{276f} needle"), "query row missing");
        assert!(flat.contains("Alpha"), "thread title label missing");
        assert!(flat.contains("Beta"), "thread title label missing");
        assert!(flat.contains("You"), "user role label missing");
        assert!(flat.contains("Chibi"), "assistant role label missing");
        assert!(
            flat.contains("needle question") && flat.contains("needle inside"),
            "snippet context windows missing"
        );
        assert!(flat.contains("Enter jump"), "hint missing");
        // Selected row marker rendered.
        assert!(flat.contains("\u{25b8}"), "selection marker missing");
        // Visually distinguishable from the in-thread popup.
        assert!(!flat.contains("Find in thread"), "wrong popup title");
    }

    /// Empty query state renders gracefully: placeholder query row, no
    /// match rows, zero count + thread count in the title, hint intact.
    #[test]
    fn search_all_popup_empty_query_is_graceful() {
        let mut app = App::new(vec![Chat::new("solo")]);
        app.chats[0].messages.push(Message::user("needle"));
        app.begin_search_all();
        let rows = render_grid(&mut app);
        let flat: String = rows.join("\n");

        assert!(
            flat.contains("type to search all threads\u{2026}"),
            "placeholder missing: {flat:?}"
        );
        assert!(
            flat.contains("\u{00b7} 0") && flat.contains("(1 thread)"),
            "zero count + thread count in title"
        );
        assert!(!flat.contains("\u{25b8}"), "no selection with no matches");
        assert!(flat.contains("Esc close"), "hint intact");
    }

    /// Tiny terminals must not panic while the global search popup is open —
    /// the box clamps to whatever fits, and repeated draws stay stable.
    #[test]
    fn search_all_popup_never_panics_on_tiny_terminal() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0].messages.push(Message::user("needle"));
        app.begin_search_all();
        for ch in "needle".chars() {
            app.search_all_push(ch);
        }
        let (rows, _) = render_grid_at_with_buffer(&mut app, 40, 8);
        let flat: String = rows.join("\n");
        assert!(flat.contains("Search all threads"), "popup title present");
        // Second draw after viewport state exists — still no panic.
        let _ = render_grid_at_with_buffer(&mut app, 40, 8);
    }

    /// No popup in Normal mode — no overlay artifacts.
    #[test]
    fn no_search_all_popup_when_closed() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0].messages.push(Message::user("needle"));
        let flat = render_grid(&mut app).join("\n");
        assert!(!flat.contains("Search all threads"));
    }

    /// The all-threads snippet path elides like the in-thread one (same
    /// shared core — the wrapper must forward the right fields).
    #[test]
    fn search_all_snippet_elides_ends() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau";
        let m = crate::app::GlobalSearchMatch {
            chat_index: 3,
            chat_title: "deep thread".to_owned(),
            message_index: 0,
            line_index: 0,
            line_text: text.to_owned(),
            col: 30, // inside "delta"
        };
        let s = search_snippet_all(&m, "delta");
        assert!(s.contains("delta"), "hit must stay visible: {s:?}");
        assert!(s.starts_with('\u{2026}'), "left elision: {s:?}");
        assert!(s.ends_with('\u{2026}'), "right elision: {s:?}");
        assert!(s.len() <= text.len(), "snippet must be shorter than source");
    }

    // THE cross-thread wrapped-jump regression

    /// THE core acceptance criterion for the global search: a match inside a
    /// NON-active thread's visually-WRAPPED paragraph must (a) activate that
    /// thread (same mechanics as Ctrl+↑/↓ switching) and (b) land on the
    /// exact WRAPPED row near the top of the pane — never the message's
    /// first row, never the bottom.
    #[test]
    fn search_all_jump_activates_non_active_thread_and_lands_wrapped_row() {
        // Narrow chat pane forces heavy wrapping of the long paragraph.
        const FRAME_W: u16 = 60;
        const FRAME_H: u16 = 24;
        // Chat pane inner width = frame - sidebar(26).
        let pane_width = FRAME_W - 26;

        let mut app = App::new(vec![Chat::new("target"), Chat::new("decoy")]);
        // The ACTIVE chat at search time: chat 0. Lots of filler so
        // follow-bottom of THIS chat would never show chat 1's needle.
        {
            let chat = &mut app.chats[0];
            chat.messages
                .push(Message::user("filler in the active thread"));
            for i in 0..20 {
                chat.messages.push(Message::user(format!("filler q {i}")));
                chat.messages.push(Message::assistant("short ack"));
            }
        }
        // The NON-active chat holding the wrapped needle.
        {
            let chat = &mut app.chats[1];
            chat.messages.push(Message::user("first question"));
            // A LONG single-paragraph reply: one logical line that wraps to
            // many display rows at this width. The needle sits in the MIDDLE
            // of the paragraph so the jump must land on a continuation row.
            let mut para = String::from("alpha filler ");
            for _ in 0..40 {
                para.push_str("filler word ");
            }
            para.push_str("needle42 mid-paragraph ");
            for _ in 0..40 {
                para.push_str("trailing filler ");
            }
            chat.messages.push(Message::assistant(para));
            // Plenty of content BELOW so follow-bottom would hide the match
            // entirely — the jump must detach from the bottom.
            for i in 0..30 {
                chat.messages.push(Message::user(format!("filler q {i}")));
                chat.messages.push(Message::assistant("short ack"));
            }
        }
        app.active = 0; // searching from chat 0; the hit lives in chat 1

        // Rebuild logical lines of the TARGET chat exactly as render_chat
        // does, then use the SAME indexed wrap helper the renderer consumes
        // to prove the needle lives on a WRAPPED continuation row.
        let mut lines: Vec<markdown::MdLine> = Vec::new();
        let mut msg_ranges: Vec<(usize, usize)> = Vec::new();
        for msg in &app.chats[1].messages {
            let start = lines.len();
            match msg.role {
                Role::User => {
                    lines.push(Line::from(Span::styled("\u{25cf} You", Style::new())));
                }
                Role::Assistant => {
                    lines.push(Line::from(Span::styled("\u{25cf} Chibi", Style::new())));
                }
            }
            if msg.pending {
                lines.push(Line::from(""));
            } else {
                lines.extend(markdown::render(&msg.markdown, &Theme::tokyo_night()));
            }
            lines.push(Line::from(""));
            msg_ranges.push((start, lines.len()));
        }
        let (_, first_row_of) = wrap_message_rows_indexed(&lines, pane_width as usize);
        let logical = msg_ranges[1].0 + 1; // long paragraph, after role header
        let needle_wrapped_row = first_row_of[logical];
        assert!(
            needle_wrapped_row > 1,
            "needle must live on a WRAPPED continuation row, got {needle_wrapped_row}"
        );

        // Full path: open the GLOBAL search from chat 0, type, jump.
        app.begin_search_all();
        for ch in "needle42".chars() {
            app.search_all_push(ch);
        }
        assert_eq!(app.search_all_matches().len(), 1);
        let (ci, mi, li, col) = {
            let m = &app.search_all_matches()[0];
            (m.chat_index, m.message_index, m.line_index, m.col)
        };
        assert_eq!(ci, 1, "hit lives in the NON-active thread");
        assert!(app.jump_to_selected_all(), "match must be found");
        assert_eq!(app.active, 1, "target thread activated by the jump");
        assert_eq!(app.pending_global_search_jump, Some((ci, mi, li, col)));

        let (rows, _) = render_grid_at_with_buffer(&mut app, FRAME_W, FRAME_H);
        let y = rows
            .iter()
            .position(|r| r.contains("needle42"))
            .expect("needle must be visible after the jump");
        // Chat pane inner starts at y=1 (top border). "Near top" = within the
        // first few rows of the pane; 1-row padding puts the match at inner
        // row 1 → grid y=2. Allow a small tolerance for border/glyph offsets.
        assert!(
            (1..=5).contains(&y),
            "match must be near the top of the pane, got y={y}: {rows:?}"
        );
        assert!(
            !app.at_bottom(),
            "jump must detach from follow-bottom (match is far above the end)"
        );
        assert!(
            y < rows.len() - 4,
            "match must NOT be at the bottom of the pane"
        );
    }

    /// the hints line gains `^⇧F all` and still fits
    /// 120 cols with the longest status label — the self-evident `^L` and
    /// `^V` hints retired to make room (both actions stay README-documented).
    #[test]
    fn status_hints_still_fit_with_global_search_hint() {
        let mut app = App::new(mock::initial_chats());
        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();

        assert!(
            last.contains("^\u{21e7}F"),
            "^⇧F missing from hints: {last:?}"
        );
        assert!(last.contains("^F"), "^F missing: {last:?}");
        assert!(
            last.contains("^C cancel"),
            "^C cancel must never be dropped"
        );
        assert!(
            last.contains("disconnected (press R)"),
            "status label clipped — hints overflowed 120 cols: {last:?}"
        );
        assert!(
            last.trim_end().width() <= 120,
            "hints row too wide: {} cols — {:?}",
            last.trim_end().width(),
            last
        );
    }

    /// the hints line carries `^T` (focus toggle —
    /// wrap-cycling was removed) and still fits 120 cols with the longest
    /// status label — `^R rename` compacted to `^R` to absorb the +1 col;
    /// The status strip later compacted `^T panel` to bare `^T` to pay for
    /// the `^O info` token. ^C cancel is never dropped.
    #[test]
    fn status_hints_still_fit_with_ctrl_t_panel_hint() {
        let mut app = App::new(mock::initial_chats());
        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();

        assert!(last.contains("^T"), "^T missing from hints: {last:?}");
        assert!(
            !last.contains("next"),
            "stale ^T next hint must be gone: {last:?}"
        );
        assert!(
            last.contains("^C cancel"),
            "^C cancel must never be dropped"
        );
        assert!(
            last.contains("disconnected (press R)"),
            "status label clipped — hints overflowed 120 cols: {last:?}"
        );
        assert!(
            last.trim_end().width() <= 120,
            "hints row too wide: {} cols — {:?}",
            last.trim_end().width(),
            last
        );
    }

    // visuals ------------------------------------------

    /// Focus emphasis differs between focuses: with Chat focused, the
    /// sidebar divider is the resting dark selection tone; with Sidebar
    /// focused it lifts to theme.blue. Snapshot-style per-cell fg compare
    /// on the same app state, only the focus flipped.
    #[test]
    fn sidebar_border_emphasis_differs_between_focuses() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("one"), Chat::new("two")]);

        // Divider column: x=25 (sidebar width 26, right border col), any
        // main-area row inside both renders.
        let (_, chat_focused_buf) = render_grid_with_buffer(&mut app);
        assert_eq!(chat_focused_buf[(25, 5)].fg, theme.selection);

        app.focus = Focus::Sidebar;
        let (_, sidebar_focused_buf) = render_grid_with_buffer(&mut app);
        assert_eq!(sidebar_focused_buf[(25, 5)].fg, theme.blue);

        // Bottom-extended divider (below the block, e.g. the input band)
        // follows the same emphasis: row 33 of 34 @120×34 is status-ish;
        // check row just below the sidebar block too. Use row 30.
        assert_eq!(sidebar_focused_buf[(25, 30)].fg, theme.blue);
        assert_ne!(
            chat_focused_buf[(25, 5)].fg,
            sidebar_focused_buf[(25, 5)].fg,
            "border color must visibly differ between focuses"
        );
    }

    /// The ` Chats ` title brightens blue → cyan while the sidebar holds
    /// focus (same snapshot technique as the border test).
    #[test]
    fn sidebar_title_emphasis_differs_between_focuses() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("one"), Chat::new("two")]);

        // Title cell: find "C" of "Chats" on row 0 (x≈1..7).
        let title_x = |buf: &ratatui::buffer::Buffer| {
            (0..20)
                .find(|&x| buf[(x, 0)].symbol() == "C")
                .expect("title not found")
        };

        let (_, chat_buf) = render_grid_with_buffer(&mut app);
        let x_chat = title_x(&chat_buf);
        assert_eq!(chat_buf[(x_chat, 0)].fg, theme.blue);

        app.focus = Focus::Sidebar;
        let (_, side_buf) = render_grid_with_buffer(&mut app);
        let x_side = title_x(&side_buf);
        assert_eq!(side_buf[(x_side, 0)].fg, theme.cyan);
    }

    /// Idle unselected dots brighten dim → fg while the sidebar owns the
    /// keyboard; selected idle stays green either way.
    #[test]
    fn sidebar_idle_dots_brighten_when_focused() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("first"), Chat::new("second")]);

        let (_, chat_buf) = render_grid_with_buffer(&mut app);
        // Row 2 = second chat's unselected idle dot.
        assert_eq!(chat_buf[(0, 2)].fg, theme.dim);

        app.focus = Focus::Sidebar;
        let (_, side_buf) = render_grid_with_buffer(&mut app);
        assert_eq!(side_buf[(0, 2)].fg, theme.fg, "unselected dot brightens");
        // Selected (row 1) keeps green in both focuses.
        assert_eq!(chat_buf[(0, 1)].fg, theme.green);
        assert_eq!(side_buf[(0, 1)].fg, theme.green);
    }

    /// The prompt's `❯` marker dims cyan → theme.dim while the sidebar
    /// holds focus (subtle "typing goes nowhere" cue); back to cyan on
    /// return.
    #[test]
    fn prompt_marker_dims_while_sidebar_focused() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("one")]);
        type_text_into_input(&mut app, "draft");

        let marker_fg = |app: &mut App| render_grid_at_with_buffer(app, 120, 34).1[(26, 32)].fg;

        assert_eq!(marker_fg(&mut app), theme.cyan, "Chat focus → cyan");
        app.focus = Focus::Sidebar;
        assert_eq!(marker_fg(&mut app), theme.dim, "Sidebar focus → dimmed");
    }

    // selection auto-scroll ------------------------------

    /// Type into the prompt textarea through tui-textarea directly (test
    /// helper shared by marker tests).
    fn type_text_into_input(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.input.input(tui_textarea::Input {
                key: tui_textarea::Key::Char(ch),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
    }

    /// Selection-aware viewport: with MORE chats than sidebar rows, moving
    /// the selection down must scroll the list so the ACTIVE row becomes
    /// visible (ratatui's List keeps a fresh-state offset minimal but
    /// selection-inclusive every frame), and moving back up restores the
    /// top rows.
    #[test]
    fn sidebar_scrolls_selection_into_view_beyond_viewport() {
        let chats: Vec<Chat> = (0..30).map(|i| Chat::new(format!("chat-{i}"))).collect();
        let mut app = App::new(chats);

        // Tiny frame: 12 rows total → main area ≈ 9 rows ⇒ ~9 visible chats.
        let rows_of = |app: &mut App| {
            render_grid_at_with_buffer(app, 120, 12)
                .0
                .into_iter()
                .collect::<Vec<_>>()
        };
        let visible_named = |rows: &[String], name: &str| rows.iter().any(|r| r.contains(name));

        let rows = rows_of(&mut app);
        assert!(visible_named(&rows, "chat-0"), "top chat visible initially");
        assert!(!visible_named(&rows, "chat-25"), "precondition sanity");

        // Jump to a selection deep BELOW the viewport…
        for _ in 0..25 {
            app.select_next();
        }
        let rows = rows_of(&mut app);
        assert!(
            visible_named(&rows, "chat-25"),
            "active chat scrolled INTO view when below viewport"
        );

        // …and far ABOVE it again.
        app.active = 0;
        app.scroll = 0;
        let rows = rows_of(&mut app);
        assert!(
            visible_named(&rows, "chat-0"),
            "selection returned to the visible top"
        );
    }

    // header rendering -------------------------

    /// A message without model metadata renders the plain `● Chibi` header —
    /// no parentheses, no "unknown" placeholder (old backend / historical
    /// rows / fieldless frames).
    #[test]
    fn assistant_header_without_metadata_is_plain() {
        let theme = Theme::tokyo_night();
        let line = assistant_header_line(None, &theme);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(text, "\u{25cf} Chibi", "fallback must stay plain: {text:?}");
        assert_eq!(line.spans.len(), 1);
    }

    /// A labelled message renders `● Chibi (model)`: the parenthetical is a
    /// separate span styled with `theme.dim` (not bold, not the header blue).
    #[test]
    fn assistant_header_renders_dim_parenthetical_model_label() {
        let theme = Theme::tokyo_night();
        let line = assistant_header_line(Some("glm-5.2"), &theme);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(text, "\u{25cf} Chibi (glm-5.2)");

        assert_eq!(line.spans.len(), 2);
        let header = &line.spans[0];
        assert_eq!(header.content, "\u{25cf} Chibi");
        assert_eq!(header.style.fg, Some(theme.blue), "header keeps its blue");
        assert!(header.style.add_modifier.contains(Modifier::BOLD));

        let paren = &line.spans[1];
        assert_eq!(paren.content, " (glm-5.2)");
        assert_eq!(paren.style.fg, Some(theme.dim), "parenthetical must be dim");
        assert!(
            !paren.style.add_modifier.contains(Modifier::BOLD),
            "parenthetical must not inherit the bold header"
        );
    }

    /// Whitespace-only metadata must not render an empty "()" suffix.
    #[test]
    fn assistant_header_ignores_blank_model_labels() {
        let theme = Theme::tokyo_night();
        let line = assistant_header_line(Some("   "), &theme);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(text, "\u{25cf} Chibi");
    }

    /// THE wrap regression: a long model name can wrap the header line in a
    /// narrow viewport. (1) The renderer's row total must grow with the
    /// wrapped header (row-accurate math counts display rows). (2) In
    /// follow-bottom mode the newest reply must still be fully visible —
    /// a miscounted total would hide its tail behind the input block
    /// (the wrapping-math class of failure).
    #[test]
    fn long_model_name_wrap_is_absorbed_by_row_math() {
        const FRAME_W: u16 = 60;
        const FRAME_H: u16 = 24;
        // Chat pane inner width = frame - sidebar(26).
        let pane_width = (FRAME_W - 26) as usize;

        let model_name = "super-long-model-name-ultra-extended-edition-v42";
        let tail_word = "tail42";

        let mut app = App::new(vec![Chat::new("wrap")]);
        app.chats[0].messages.push(Message::user("question"));
        app.chats[0].messages.push(Message::assistant_with_model(
            format!("answer one-liner {tail_word}"),
            model_name,
        ));

        // (1) Row math: the labelled header must occupy MORE display rows
        // than the plain header at this width (the wrap is absorbed by the
        // same totals the renderer consumes).
        let theme = Theme::tokyo_night();
        let build_lines = |label: Option<&str>| {
            let mut lines: Vec<markdown::MdLine> = Vec::new();
            for msg in [
                Message::user("question"),
                Message::assistant_with_model(
                    format!("answer one-liner {tail_word}"),
                    label.unwrap_or(""),
                ),
            ] {
                match msg.role {
                    Role::User => {
                        lines.push(Line::from(Span::styled("\u{25cf} You", Style::new())))
                    }
                    Role::Assistant => lines.push(assistant_header_line(msg.model_label(), &theme)),
                }
                lines.extend(markdown::render(&msg.markdown, &theme));
                lines.push(Line::from(""));
            }
            lines
        };
        let plain_rows = wrap_message_rows_indexed(&build_lines(None), pane_width)
            .0
            .len();
        let labelled_rows = wrap_message_rows_indexed(&build_lines(Some(model_name)), pane_width)
            .0
            .len();
        assert!(
            labelled_rows > plain_rows,
            "long model name must wrap and add display rows: {labelled_rows} vs {plain_rows}"
        );

        // (2) Full render, follow-bottom: the labelled header START (it
        // wrapped) may scroll off, but the newest reply's tail must remain
        // visible right above the input row.
        let (rows, _) = render_grid_at_with_buffer(&mut app, FRAME_W, FRAME_H);
        let flat = rows.join("\n");
        assert!(
            flat.contains(tail_word),
            "newest reply tail hidden — wrap row math regressed:\n{flat}"
        );
        // The wrapped label itself renders across rows: its head chunk and
        // tail chunk must both be present (split-point-agnostic — the exact
        // break column depends on the pane width, not on the label's
        // integrity, which is precisely what the row math absorbs).
        assert!(
            flat.contains("(super-long-model-name") && flat.contains("edition-v42"),
            "wrapped label rows themselves must render:\n{flat}"
        );
    }

    /// End-to-end render: labelled and unlabelled messages coexist in one
    /// thread, each rendering exactly its own header shape.
    #[test]
    fn mixed_labelled_and_plain_messages_render_per_message() {
        let mut app = App::new(vec![Chat::new("chat")]);
        app.chats[0].messages.push(Message::user("q1"));
        app.chats[0]
            .messages
            .push(Message::assistant_with_model("old answer", "glm-5.2"));
        app.chats[0].messages.push(Message::user("q2"));
        app.chats[0].messages.push(Message::assistant("new answer"));

        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.contains("(glm-5.2)"),
            "labelled header missing:\n{flat}"
        );
        let lines_with_label = flat
            .lines()
            .filter(|r| r.contains("\u{25cf} Chibi"))
            .count();
        assert!(
            lines_with_label >= 2,
            "both assistant headers must render: {flat}"
        );
    }

    /// THE backfill regression: switching the
    /// model mid-chat must not re-label answers produced earlier. A row
    /// without its own per-message label — restored pre-label history or a
    /// fieldless result frame — keeps the plain `● Chibi` header even when
    /// the thread's last-known model (the status strip source) names a
    /// different model.
    #[test]
    fn model_switch_never_relabels_unlabeled_answers() {
        // Post-switch state: the picker stamped the thread's last-known
        // model with the NEW selection (Ctrl+M confirm), while the earlier
        // answer predates the label and carries no annotation of its own.
        let mut chat = Chat::new("switched");
        chat.messages
            .push(Message::assistant("answer made before the switch"));
        chat.last_model = Some("kimi-k3".to_string());

        let mut app = App::new(vec![chat]);
        assert_eq!(
            app.active_model_label(),
            Some("kimi-k3"),
            "panel reflects the switched model"
        );

        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.lines()
                .any(|r| r.contains("\u{25cf} Chibi") && !r.contains('(')),
            "the unlabeled answer must keep the plain header:\n{flat}"
        );
        assert!(
            !flat.contains("(kimi-k3)"),
            "the switched model must not backfill old answers:\n{flat}"
        );
    }

    /// Per-answer independence: each header renders the model that produced
    /// ITS message, captured at answer time — never the thread's current
    /// selection, never a neighbor's label.
    #[test]
    fn each_answer_keeps_its_own_captured_label() {
        let mut chat = Chat::new("multi");
        chat.messages
            .push(Message::assistant_with_model("first answer", "glm-5.2"));
        chat.messages
            .push(Message::assistant_with_model("second answer", "kimi-k3"));
        chat.last_model = Some("qwen-flash".to_string());

        let mut app = App::new(vec![chat]);
        let flat = render_grid(&mut app).join("\n");

        assert!(
            flat.lines()
                .any(|r| r.contains("\u{25cf} Chibi") && r.contains("(glm-5.2)")),
            "the first answer keeps its own model:\n{flat}"
        );
        assert!(
            flat.lines()
                .any(|r| r.contains("\u{25cf} Chibi") && r.contains("(kimi-k3)")),
            "the second answer keeps its own model:\n{flat}"
        );
        assert!(
            !flat.contains("(qwen-flash)"),
            "the panel selection must not leak into the transcript:\n{flat}"
        );
    }

    /// Restart seam: restored rows render ONLY their own persisted label.
    /// Rows saved before labels were persisted (no `model` key) reload
    /// plain and are never backfilled from the thread's last-known model;
    /// the panel readout (status strip source) still names that model.
    #[test]
    fn restored_rows_render_own_persisted_label_and_legacy_rows_stay_plain() {
        let mut chat = Chat::new("restored");
        chat.messages
            .push(Message::assistant_with_model("old answer", "some/model"));
        chat.messages.push(Message::assistant("pre-label answer"));
        chat.last_model = Some("some/model".to_string());

        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-annotation-{}-{}",
            std::process::id(),
            crate::history::new_thread_id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        crate::history::save_chat_in(Some(&dir), &chat).expect("save");
        let restored = crate::history::load_chats_from(Some(&dir));
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            restored[0].last_model.as_deref(),
            Some("some/model"),
            "last-known model persists with the thread"
        );
        assert_eq!(
            restored[0].messages[0].model_label(),
            Some("some/model"),
            "the per-answer label persists with its message"
        );
        assert!(
            restored[0].messages[1].model_label().is_none(),
            "pre-label rows stay label-less on disk and in memory"
        );

        let mut app = App::new(restored);
        assert_eq!(
            app.active_model_label(),
            Some("some/model"),
            "status strip source seeded from the snapshot"
        );

        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.lines()
                .any(|r| r.contains("\u{25cf} Chibi") && r.contains("(some/model)")),
            "the labeled row keeps its persisted label after restart:\n{flat}"
        );
        let labeled_headers = flat
            .lines()
            .filter(|r| r.contains("\u{25cf} Chibi ("))
            .count();
        assert_eq!(
            labeled_headers, 1,
            "no backfill: only the row with its own label is labeled:\n{flat}"
        );
    }

    fn unique_marker(tag: &str) -> String {
        format!(
            "ui-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// The `log*` unseen-lines marker appears on the status line (dim token
    /// after the hints) when the viewer is closed and unseen lines exist —
    /// and the whole row still fits the 120-col discipline with the LONGEST
    /// status label (`● disconnected (press R)`).
    #[test]
    fn log_marker_shows_on_status_line_and_fits_120_cols() {
        let marker = unique_marker("unseen");
        crate::diag::append(&marker);

        let mut app = App::new(mock::initial_chats()); // popup-free state
        app.connection = crate::app::Connection::Disconnected;
        let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 34);
        let status_row = rows.last().expect("status row exists");

        assert!(
            status_row.contains("log*"),
            "unseen marker must show: {status_row:?}"
        );
        let width: usize = UnicodeWidthStr::width(status_row.as_str());
        assert!(
            width <= 120,
            "status row must fit 120 cols with the marker + longest label, got {width}: {status_row:?}"
        );
    }

    /// The marker hides while the log viewer modal is open (the user is
    /// looking at the stream) — mode-gated, so no global-state racing.
    #[test]
    fn log_marker_hidden_while_viewer_modal_is_open() {
        let mut app = App::new(mock::initial_chats());
        app.begin_log_viewer();
        let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 34);
        let status_row = rows.last().expect("status row exists");
        assert!(
            !status_row.contains("log*"),
            "marker must hide while the viewer is open: {status_row:?}"
        );
    }

    /// The viewer modal renders buffered lines (mono view) with the closing
    /// hint; live-tail shows the newest content.
    #[test]
    fn log_viewer_modal_renders_lines_and_hint() {
        let marker = unique_marker("modal");
        crate::diag::append(&marker);

        let mut app = App::new(mock::initial_chats());
        app.begin_log_viewer();
        let flat = render_grid(&mut app).join("\n");

        assert!(flat.contains("Diagnostics log"), "title: {flat}");
        assert!(
            flat.contains(&marker),
            "buffered line visible in the modal: {flat}"
        );
        assert!(
            flat.contains("Esc close") && flat.contains("PgUp/PgDn page"),
            "footer hint visible: {flat}"
        );
        // Header state: position + tail + wrap state
        // ride the footer row.
        assert!(
            flat.contains("live") && flat.contains("wrap: off"),
            "header shows tail and wrap state: {flat}"
        );
        // At the bottom: no "+K new lines" (nothing arrived since the open).
        assert!(
            !flat.contains("new lines"),
            "no +K hint at the live tail: {flat}"
        );
    }

    /// Pinned (cursor above the tail) with arrivals pending: the frozen view
    /// keeps its position and the footer counts the new lines
    /// (`+K new lines`).
    #[test]
    fn log_viewer_modal_shows_plus_k_hint_when_detached() {
        let mut app = App::new(mock::initial_chats());
        for i in 0..12 {
            crate::diag::append(format!("filler-{i}"));
        }
        app.begin_log_viewer();
        app.log_cursor_up(3);
        assert!(
            match &app.mode {
                Mode::LogViewer { state } => !state.at_tail(),
                _ => false,
            },
            "precondition: pinned"
        );

        let marker = unique_marker("arrived");
        crate::diag::append(&marker);

        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.contains("new lines"),
            "+K footer must show while pinned with arrivals: {flat}"
        );
        assert!(
            !flat.contains(&marker),
            "frozen snapshot must NOT show lines that arrived while pinned: {flat}"
        );

        // Re-arming the tail (G / cursor back to the newest line) refreshes
        // the snapshot and clears the hint.
        app.log_jump_bottom();
        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.contains(&marker),
            "live-tail shows the arrival: {flat}"
        );
        assert!(
            !flat.contains("new lines"),
            "hint clears at the bottom: {flat}"
        );
    }

    /// `[tui]` lifecycle events render (so the unified stream is visible);
    /// long stderr lines are right-truncated instead of wrapping (row math):
    /// the line HEAD is visible, the tail is cut, and the content never
    /// spills onto a second row.
    #[test]
    fn log_viewer_renders_tui_events_and_truncates_long_lines() {
        crate::diag::append_tui("spawn `chibi` (pid 4242)");
        let long = format!("LONGHEAD-{}", "x".repeat(300));
        crate::diag::append(&long);

        let mut app = App::new(mock::initial_chats());
        app.begin_log_viewer();
        let flat = render_grid(&mut app).join("\n");

        assert!(
            flat.contains("[tui] spawn `chibi` (pid 4242)"),
            "[tui] event visible: {flat}"
        );
        assert!(
            flat.contains("LONGHEAD-"),
            "long line reaches the modal: {flat}"
        );
        // No wrap: the 300-x tail must NOT reappear on any row (truncated).
        assert!(
            !flat.contains(&"x".repeat(150)),
            "long line must be truncated, never wrapped: {flat}"
        );
    }

    // header state, wrap, level colors -----------

    use crate::app::LogViewerState;

    /// Hand-built viewer state for the hermetic render tests below (the
    /// global diag stream is shared by parallel tests and would race).
    /// Lines go through the same ingestion parse as real arrivals, so the
    /// tests exercise the production level attribution. The
    /// The search fields default to off.
    fn viewer_state(cursor: usize, wrap: bool, lines: Vec<String>) -> LogViewerState {
        LogViewerState {
            cursor,
            wrap,
            row_offset: 0,
            lines: lines
                .into_iter()
                .map(crate::diag::LogEntry::parse)
                .collect(),
            snapshot_total: crate::diag::total_appended(),
            search_buf: None,
            search: None,
            copy_note: None,
            copy_note_at: None,
        }
    }

    /// The header (border title) carries the cursor position, the tail
    /// state and the wrap toggle; stepping the cursor up flips live →
    /// pinned, `w` flips wrap: off → on.
    #[test]
    fn log_viewer_header_shows_position_tail_and_wrap() {
        let mut app = App::new(mock::initial_chats());
        for i in 0..5 {
            crate::diag::append(format!("filler-{i}"));
        }
        app.begin_log_viewer();

        let rows = render_grid(&mut app);
        let title = rows
            .iter()
            .find(|r| r.contains("Diagnostics log"))
            .expect("viewer title row exists");
        assert!(title.contains("line "), "position shown: {title}");
        assert!(title.contains("live"), "tail state shown: {title}");
        assert!(title.contains("wrap: off"), "wrap state shown: {title}");

        // Two cursor steps up: the header must flip to pinned.
        app.log_cursor_up(2);
        let rows = render_grid(&mut app);
        let title = rows
            .iter()
            .find(|r| r.contains("Diagnostics log"))
            .expect("viewer title row exists");
        assert!(title.contains("pinned"), "pinned state shown: {title}");
        assert!(!title.contains(" live "), "no live while pinned: {title}");

        // `w` toggles wrap and the header follows.
        app.log_toggle_wrap();
        let rows = render_grid(&mut app);
        let title = rows
            .iter()
            .find(|r| r.contains("Diagnostics log"))
            .expect("viewer title row exists");
        assert!(title.contains("wrap: on"), "wrap on shown: {title}");
    }

    /// Wrap off keeps the compact truncated timeline (the old contract);
    /// wrap on reflows the SAME logical line over several rows so its tail
    /// becomes readable. Hermetic: hand-built pinned state, so the global
    /// diag stream (shared by parallel tests) cannot race the render.
    #[test]
    fn log_viewer_wrap_toggle_reflows_long_lines() {
        let long = format!("WRAPHEAD-{}", "y".repeat(200));
        let mk_state = |wrap: bool| {
            viewer_state(
                1,
                wrap,
                vec![
                    "head filler".to_owned(),
                    long.clone(),
                    "tail filler".to_owned(),
                ],
            )
        };

        let mut app = App::new(mock::initial_chats());
        app.mode = Mode::LogViewer {
            state: mk_state(false),
        };
        let flat = render_grid(&mut app).join("\n");
        assert!(
            !flat.contains(&"y".repeat(150)),
            "wrap off: long line stays truncated, never reflowed: {flat}"
        );

        app.mode = Mode::LogViewer {
            state: mk_state(true),
        };
        let flat = render_grid(&mut app).join("\n");
        assert!(
            flat.contains(&"y".repeat(100)),
            "wrap on: the line tail is reflowed into view: {flat}"
        );
    }

    /// Levels parsed at ingestion colorize per the log_line_style table:
    /// TRACE very dim, DEBUG dim gray, INFO/SUCCESS green, WARNING yellow,
    /// ERROR red, CRITICAL bold red. Checked against the actual rendered
    /// cells. The viewer state is built by hand (pinned, so the render
    /// never refreshes it): the global diag stream is shared by parallel
    /// tests and would race.
    #[test]
    fn log_viewer_colors_levels_per_mapping() {
        let theme = Theme::tokyo_night();
        let cases: [(&str, &str, ratatui::style::Color, bool); 7] = [
            ("LVC-TRACE", "TRACE", theme.log_trace, false),
            ("LVC-DEBUG", "DEBUG", theme.log_debug, false),
            ("LVC-INFO", "INFO", theme.green, false),
            ("LVC-WARN", "WARNING", theme.yellow, false),
            ("LVC-ERROR", "ERROR", theme.red, false),
            ("LVC-CRIT", "CRITICAL", theme.red, true),
            ("LVC-OK", "SUCCESS", theme.green, false),
        ];
        let lines: Vec<String> = cases
            .iter()
            .map(|(marker, level, _, _)| {
                format!("2026-09-01 10:00:00.000 | {level} | chibi.m:1 - body {marker}")
            })
            .collect();

        let mut app = App::new(mock::initial_chats());
        app.mode = Mode::LogViewer {
            // Pinned to a middle line: the snapshot stays frozen.
            state: viewer_state(3, false, lines),
        };
        let (rows, buf) = render_grid_with_buffer(&mut app);

        for (marker, _, expected, bold) in &cases {
            let (y, row) = rows
                .iter()
                .enumerate()
                .find(|(_, r)| r.contains(marker))
                .unwrap_or_else(|| panic!("level line {marker} visible in the viewer"));
            let col = col_of_sub(row, marker).expect("marker column");
            let cell = &buf[(col as u16, y as u16)];
            assert_eq!(cell.fg, *expected, "level color for {marker}");
            assert_eq!(
                cell.modifier.contains(Modifier::BOLD),
                *bold,
                "bold flag for {marker}"
            );
        }
    }
    /// The backend's custom levels colorize per their registration colors
    /// routed through the theme slots: TOOL light-blue → blue, THINK
    /// light-magenta / CALL magenta → purple, CHECK/MODERATOR light-red →
    /// red, SUBAGENT cyan → cyan, DELEGATE blue → blue. Rendered-cell
    /// check, same technique as the standard levels above.
    #[test]
    fn log_viewer_colors_custom_levels_per_mapping() {
        let theme = Theme::tokyo_night();
        let cases: [(&str, &str, ratatui::style::Color); 7] = [
            ("LVC-TOOL", "TOOL", theme.blue),
            ("LVC-THINK", "THINK", theme.purple),
            ("LVC-CALL", "CALL", theme.purple),
            ("LVC-CHECK", "CHECK", theme.red),
            ("LVC-MODERATOR", "MODERATOR", theme.red),
            ("LVC-SUBAGENT", "SUBAGENT", theme.cyan),
            ("LVC-DELEGATE", "DELEGATE", theme.blue),
        ];
        let lines: Vec<String> = cases
            .iter()
            .map(|(marker, level, _)| {
                format!("2026-09-01 10:00:00 | {level} | chibi.m:1 - body {marker}")
            })
            .collect();

        let mut app = App::new(mock::initial_chats());
        app.mode = Mode::LogViewer {
            // Pinned to a middle line: the snapshot stays frozen.
            state: viewer_state(3, false, lines),
        };
        let (rows, buf) = render_grid_with_buffer(&mut app);

        for (marker, _, expected) in &cases {
            let (y, row) = rows
                .iter()
                .enumerate()
                .find(|(_, r)| r.contains(marker))
                .unwrap_or_else(|| panic!("custom level line {marker} visible in the viewer"));
            let col = col_of_sub(row, marker).expect("marker column");
            let cell = &buf[(col as u16, y as u16)];
            assert_eq!(cell.fg, *expected, "custom level color for {marker}");
            assert!(
                !cell.modifier.contains(Modifier::BOLD),
                "custom levels carry no extra modifiers: {marker}"
            );
        }
    }

    /// DoD: every custom level resolves to a REAL theme slot — not the
    /// level-less fallback — in EVERY bundled theme, so a future theme that
    /// misses a slot fails here instead of shipping an uncolored tier.
    #[test]
    fn log_line_style_resolves_custom_levels_to_slots_in_every_bundled_theme() {
        use crate::diag::LogLevel;
        for theme in Theme::bundled() {
            let cases = [
                (LogLevel::Tool, theme.blue),
                (LogLevel::Think, theme.purple),
                (LogLevel::Call, theme.purple),
                (LogLevel::Check, theme.red),
                (LogLevel::Moderator, theme.red),
                (LogLevel::Subagent, theme.cyan),
                (LogLevel::Delegate, theme.blue),
            ];
            for (level, slot) in cases {
                let entry = crate::diag::LogEntry {
                    text: "t".to_owned(),
                    level: Some(level),
                };
                let style = log_line_style(&entry, &theme);
                assert_eq!(
                    style.fg,
                    Some(slot),
                    "{level:?} must take its theme slot in every bundled theme"
                );
                assert_ne!(
                    style.fg,
                    Some(theme.fg),
                    "{level:?} must not fall through to the level-less default"
                );
            }
        }
    }

    /// Unknown level names stay graceful at the style-table level too: the
    /// level parses to `None` and renders the default foreground — no
    /// panic, no raw leak, no invented color.
    #[test]
    fn log_line_style_falls_back_to_default_fg_for_unknown_levels() {
        let theme = Theme::tokyo_night();
        for token in ["NOTALEVEL", "TOOOL", "tool2", ""] {
            let entry =
                crate::diag::LogEntry::parse(format!("2026-09-01 10:00:00 | {token} | body"));
            assert_eq!(entry.level, None, "unknown token {token:?} parses to None");
            assert_eq!(
                log_line_style(&entry, &theme).fg,
                Some(theme.fg),
                "unknown level {token:?} keeps the default foreground"
            );
        }
    }

    /// Graceful degradation: lines without a recognizable ` | LEVEL | `
    /// field (old backends, malformed output, plain stderr noise) render in
    /// the default foreground exactly as before, and `[tui]` lifecycle
    /// events stay dim.
    #[test]
    fn log_viewer_unknown_lines_render_default_and_tui_events_stay_dim() {
        let theme = Theme::tokyo_night();
        let lines = vec![
            "plain stderr noise".to_owned(),
            "2026-09-01 10:00:00.000 | NOTALEVEL | chibi.m:1 - body".to_owned(),
            "a | INFO".to_owned(),
            "[tui] handshake ok (protocol v1)".to_owned(),
        ];

        let mut app = App::new(mock::initial_chats());
        app.mode = Mode::LogViewer {
            state: viewer_state(1, false, lines),
        };
        let (rows, buf) = render_grid_with_buffer(&mut app);

        for marker in ["plain stderr noise", "NOTALEVEL", "a | INFO"] {
            let (y, row) = rows
                .iter()
                .enumerate()
                .find(|(_, r)| r.contains(marker))
                .unwrap_or_else(|| panic!("line {marker:?} visible in the viewer"));
            let col = col_of_sub(row, marker).expect("marker column");
            assert_eq!(
                buf[(col as u16, y as u16)].fg,
                theme.fg,
                "default foreground for {marker:?}"
            );
        }

        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains("handshake ok"))
            .expect("tui event visible in the viewer");
        let col = col_of_sub(row, "handshake ok").expect("marker column");
        assert_eq!(
            buf[(col as u16, y as u16)].fg,
            theme.dim,
            "[tui] events stay dim"
        );
    }

    // search + copy ------------------------

    /// Every occurrence of the pattern lights up in the log_match slot, and
    /// the line the cursor's current hit sits on is emphasized (reversed on
    /// top of the slot). Non-match text keeps its level color. Hand-built
    /// pinned state, so the shared diag stream cannot race the render.
    #[test]
    fn log_viewer_search_highlights_matches_and_current() {
        use crate::app::LogSearch;
        let theme = Theme::tokyo_night();
        let mut app = App::new(mock::initial_chats());
        app.mode = Mode::LogViewer {
            state: viewer_state(
                0,
                false,
                vec![
                    "one ALPHA mid".to_owned(),
                    "plain middle".to_owned(),
                    "three alpha end".to_owned(),
                ],
            ),
        };
        if let Mode::LogViewer { state } = &mut app.mode {
            state.search = Some(LogSearch {
                pattern: "alpha".to_owned(),
                matches: vec![0, 2],
                current: Some(0),
            });
        }
        let (rows, buf) = render_grid_with_buffer(&mut app);

        // Current hit line: the match cells take the slot AND are reversed.
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains("ALPHA"))
            .expect("current match line visible");
        let col = col_of_sub(row, "ALPHA").expect("match column");
        for dx in 0..5usize {
            let cell = &buf[((col + dx) as u16, y as u16)];
            assert_eq!(cell.fg, theme.log_match, "match slot fg at dx={dx}");
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "current match emphasized at dx={dx}"
            );
        }
        let before = &buf[((col - 1) as u16, y as u16)];
        assert_ne!(before.fg, theme.log_match, "text before the hit untouched");

        // The other hit line: same slot, no reversed emphasis.
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains("alpha"))
            .expect("second match line visible");
        let col = col_of_sub(row, "alpha").expect("match column");
        let cell = &buf[(col as u16, y as u16)];
        assert_eq!(cell.fg, theme.log_match);
        assert!(
            !cell.modifier.contains(Modifier::REVERSED),
            "non-current hits stay plain slot color"
        );
    }

    /// A hit past the first wrap chunk still highlights: the search runs on
    /// the logical line and the ranges are cut into the reflowed rows at
    /// their char offsets.
    #[test]
    fn log_viewer_search_highlight_survives_wrap_boundary() {
        use crate::app::LogSearch;
        let theme = Theme::tokyo_night();
        // 110 x's push `alpha` past the first chunk at the demo width.
        let long = format!("{}alpha tail", "x".repeat(110));
        let mut app = App::new(mock::initial_chats());
        // Pinned (cursor 0 of 2 lines): the snapshot never refreshes.
        app.mode = Mode::LogViewer {
            state: viewer_state(0, true, vec![long, "tail filler".to_owned()]),
        };
        if let Mode::LogViewer { state } = &mut app.mode {
            state.search = Some(LogSearch {
                pattern: "alpha".to_owned(),
                matches: vec![0],
                current: Some(0),
            });
        }
        let (rows, buf) = render_grid_with_buffer(&mut app);

        // The reflow split the line: chunk rows exist, and the row that
        // carries the hit highlights exactly the pattern cells.
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains("alpha"))
            .expect("the second chunk shows the tail");
        let col = col_of_sub(row, "alpha").expect("match column in the wrapped row");
        for dx in 0..5usize {
            let cell = &buf[((col + dx) as u16, y as u16)];
            assert_eq!(
                cell.fg, theme.log_match,
                "highlight crossed the wrap at dx={dx}"
            );
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "current hit emphasis survives the reflow at dx={dx}"
            );
        }
        let before = &buf[((col - 1) as u16, y as u16)];
        assert_ne!(
            before.fg, theme.log_match,
            "padding x before the hit untouched"
        );
    }

    /// Header carries the whole search/copy state in one line: plain count
    /// before the first n, `k/N` while navigating, the open prompt echo,
    /// and the copy feedback (both flavors).
    #[test]
    fn log_viewer_header_shows_search_and_copy_states() {
        use crate::app::LogSearch;
        let mk = |cursor: usize, lines: Vec<String>| {
            let mut app = App::new(mock::initial_chats());
            app.mode = Mode::LogViewer {
                state: viewer_state(cursor, false, lines),
            };
            app
        };
        let title_of = |app: &mut App| {
            render_grid(app)
                .into_iter()
                .find(|r| r.contains("Diagnostics log"))
                .expect("viewer title row exists")
        };

        // Committed, nothing selected yet: bare count. A 4th line keeps the
        // cursor off the tail after n, so the hand-built snapshot never
        // gets replaced by the live refresh.
        let mut app = mk(
            0,
            vec![
                "alpha one".to_owned(),
                "filler".to_owned(),
                "two alpha".to_owned(),
                "end filler".to_owned(),
            ],
        );
        if let Mode::LogViewer { state } = &mut app.mode {
            state.search = Some(LogSearch {
                pattern: "alpha".to_owned(),
                matches: vec![0, 2],
                current: None,
            });
        }
        let title = title_of(&mut app);
        assert!(title.contains("matches: 2"), "bare count: {title}");
        assert!(
            !title.contains("matches: 2/"),
            "no position before n: {title}"
        );

        // After n: the cursor sat on hit #1 already, so n moves to hit #2.
        app.log_search_next();
        let title = title_of(&mut app);
        assert!(title.contains("matches: 2/2"), "navigating count: {title}");

        // Open prompt: pattern echo in the header, prompt line at the
        // bottom instead of the hint row.
        app.log_open_search();
        app.log_search_push('a');
        app.log_search_push('l');
        let rows = render_grid(&mut app);
        let title = rows
            .iter()
            .find(|r| r.contains("Diagnostics log"))
            .expect("title row");
        assert!(title.contains("search: /al"), "prompt echo: {title}");
        let footer = rows
            .iter()
            .find(|r| r.contains("Enter commit"))
            .expect("prompt line at the bottom");
        assert!(footer.contains("/al"), "prompt shows the buffer: {footer}");

        // Copy feedback rides the header too, both flavors.
        let mut app = mk(0, vec!["a".to_owned(), "b".to_owned()]);
        if let Mode::LogViewer { state } = &mut app.mode {
            state.copy_note = Some("copied".to_owned());
            state.copy_note_at = Some(std::time::Instant::now());
        }
        let title = title_of(&mut app);
        assert!(title.contains("\u{00b7} copied"), "success note: {title}");

        let mut app = mk(0, vec!["a".to_owned(), "b".to_owned()]);
        if let Mode::LogViewer { state } = &mut app.mode {
            state.copy_note = Some("copy: unavailable".to_owned());
            state.copy_note_at = Some(std::time::Instant::now());
        }
        let title = title_of(&mut app);
        assert!(title.contains("copy: unavailable"), "failure note: {title}");

        // The hint row advertises the new keys when nothing is open.
        let mut app = mk(0, vec!["a".to_owned(), "b".to_owned()]);
        let rows = render_grid(&mut app);
        assert!(
            rows.iter().any(|r| r.contains("/ search")),
            "hint mentions search"
        );
        assert!(
            rows.iter().any(|r| r.contains("y copy")),
            "hint mentions copy"
        );
    }

    // cwd + model strip -------------------------------

    /// Default contract: the strip is VISIBLE — the chat header border
    /// carries the `cwd:` readout on startup; ^O toggles it off.
    #[test]
    fn status_strip_is_visible_by_default() {
        let mut app = App::new(mock::initial_chats());
        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains("cwd:"),
            "strip must be visible by default: {:?}",
            rows[0]
        );
    }

    /// Visible by default: the readout rides the SAME top-border row as the chat
    /// header title — right-aligned into the pane's last columns and
    /// dim-styled (theme-driven). Mock chats carry no model labels and the
    /// workspace root is unwired here, so both segments show `—`.
    #[test]
    fn status_strip_renders_right_aligned_and_dim_on_the_header_border() {
        let mut app = App::new(mock::initial_chats());
        let theme = Theme::tokyo_night();
        // Zero vertical cost: render with the strip visible (the default),
        // then toggle it off — ONLY the header border row (row 0) may
        // change; every other row is untouched, proving the strip never
        // steals a content row.
        let (before, _) = render_grid_with_buffer(&mut app);
        app.toggle_status_strip();
        let (rows, _) = render_grid_with_buffer(&mut app);
        for (y, (b, a)) in before.iter().zip(rows.iter()).enumerate().skip(1) {
            assert_eq!(b, a, "row {y} must be untouched by the strip");
        }

        app.toggle_status_strip();
        let (rows, buf) = render_grid_with_buffer(&mut app);
        let header = &rows[0];
        let text = "cwd: \u{2014} \u{00b7} \u{2014}";
        assert!(header.contains(text), "strip text missing: {header:?}");
        // Right-aligned: the readout's last column is the chat pane's last
        // column (119 @120), i.e. it starts at 120 - 10 = 110.
        let start = col_of_sub(header, text).expect("strip column");
        assert_eq!(start, 110, "strip not right-aligned: {header:?}");
        // Dim styling: every cell of the readout uses theme.dim (not the
        // bold header blue, not the border selection color).
        for (i, _) in text.chars().enumerate() {
            assert_eq!(
                buf[((start + i) as u16, 0)].fg,
                theme.dim,
                "strip cell {i} not dim"
            );
        }
    }

    /// Model segment reuses the model-label metadata: the active
    /// chat's LAST KNOWN label is shown; a chat without any label shows the
    /// `—` placeholder (live switching re-labels per chat).
    #[test]
    fn status_strip_shows_active_chats_last_model() {
        let mut app = App::new(mock::initial_chats());
        app.chats[0]
            .messages
            .push(Message::assistant_with_model("labelled answer", "glm-5.2"));

        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains("cwd: \u{2014} \u{00b7} glm-5.2"),
            "model label missing: {:?}",
            rows[0]
        );

        // Switch to a chat with no labels: placeholder returns.
        app.select_next();
        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains("\u{00b7} \u{2014}"),
            "unlabelled chat must show the placeholder: {:?}",
            rows[0]
        );
    }

    /// Sticky display state at render level: an unlabelled answer (command
    /// result) appended AFTER a labelled one must not change the strip, and
    /// the ctx segment rides on the sticky last-known usage.
    #[test]
    fn status_strip_keeps_last_known_model_and_ctx_after_command_answer() {
        let mut app = App::new(mock::initial_chats());
        app.chats[0]
            .messages
            .push(Message::assistant_with_model("labelled answer", "glm-5.2"));
        app.last_turn_usage = Some(Usage {
            input_tokens: 18432,
            output_tokens: 512,
            context_window: Some(131_072),
        });

        // A command answer arrives: model-less message, no new usage.
        app.chats[0].messages.push(Message::assistant("done"));

        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains("cwd: \u{2014} \u{00b7} glm-5.2"),
            "model must stay last-known: {:?}",
            rows[0]
        );
        assert!(
            rows[0].contains(" \u{00b7} ctx 14% (18.4k/131.0k)"),
            "ctx must stay last-known: {:?}",
            rows[0]
        );
    }

    /// Long workspace path: the readout is truncated to the space left of
    /// the header title with a single-column ellipsis — no overflow, left
    /// title intact, no collision.
    #[test]
    fn status_strip_truncates_long_paths_with_ellipsis() {
        let mut app = App::new(mock::initial_chats());
        app.workspace_root = Some(format!("/tmp/{}", "w".repeat(120)));

        let rows = render_grid(&mut app);
        let header = &rows[0];
        assert!(
            header.contains("#1/4"),
            "left header title must survive: {header:?}"
        );
        assert!(
            header.contains('\u{2026}'),
            "ellipsis missing from truncated strip: {header:?}"
        );
        assert!(
            !header.contains(&"w".repeat(120)),
            "untruncated path leaked into the row: {header:?}"
        );
        assert!(
            header.trim_end().width() <= 120,
            "header row overflowed: {} cols",
            header.trim_end().width()
        );
    }

    /// The cwd segment shows the last three path components with a leading
    /// `/` (path-tail format), not the bare basename.
    #[test]
    fn status_strip_shows_the_cwd_path_tail() {
        let mut app = App::new(mock::initial_chats());
        app.workspace_root = Some("/Users/sergio/Develop/personal/chibi-tui".into());

        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains("cwd: /Develop/personal/chibi-tui"),
            "cwd tail missing: {:?}",
            rows[0]
        );
    }

    /// Under width pressure the tail is cut from the LEFT with a leading
    /// `…` while the working directory itself and the model readout
    /// survive (the strip is right-aligned, so its tail end is the part
    /// worth keeping).
    #[test]
    fn status_strip_left_truncates_the_cwd_tail_but_keeps_the_directory() {
        let mut app = App::new(mock::initial_chats());
        app.workspace_root = Some(format!("/Users/sergio/{}/personal/chibi", "d".repeat(100)));

        let rows = render_grid(&mut app);
        let header = &rows[0];
        assert!(header.contains('\u{2026}'), "ellipsis missing: {header:?}");
        assert!(
            header.contains("/personal/chibi"),
            "working directory lost: {header:?}"
        );
        assert!(
            !header.contains(&"d".repeat(100)),
            "untruncated tail leaked into the row: {header:?}"
        );
        assert!(
            !header.contains("/sergio/"),
            "tail must be cut from the left, not the right: {header:?}"
        );
        assert!(
            header.contains("\u{00b7} \u{2014}"),
            "model readout must survive the squeeze: {header:?}"
        );
        assert!(
            header.trim_end().width() <= 120,
            "header row overflowed: {} cols",
            header.trim_end().width()
        );
    }

    // ctx usage segment in the strip -----------------------------

    /// Human token formatting: raw digits under 1000, `x.xk` under a
    /// million, else `x.xM`; tenths truncate (never round up).
    #[test]
    fn human_tokens_format_boundaries() {
        assert_eq!(human_tokens(0), "0");
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(1000), "1.0k");
        assert_eq!(human_tokens(18432), "18.4k");
        assert_eq!(human_tokens(999_999), "999.9k");
        assert_eq!(human_tokens(1_050_000), "1.0M");
        assert_eq!(human_tokens(1_310_720), "1.3M");
    }

    /// Known window: pct is input tokens against the window (floored) and
    /// both counts render in human format.
    #[test]
    fn status_strip_shows_context_usage_with_known_window() {
        let mut app = App::new(mock::initial_chats());
        app.last_turn_usage = Some(Usage {
            input_tokens: 18432,
            output_tokens: 512,
            context_window: Some(131_072),
        });

        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains(" \u{00b7} ctx 14% (18.4k/131.0k)"),
            "ctx segment missing or malformed: {:?}",
            rows[0]
        );
    }

    /// Unknown window (and the zero-window degenerate): absolute count
    /// only — no pct, no invented max.
    #[test]
    fn status_strip_shows_absolute_usage_when_window_unknown() {
        let mut app = App::new(mock::initial_chats());
        app.last_turn_usage = Some(Usage {
            input_tokens: 18432,
            output_tokens: 512,
            context_window: None,
        });

        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains(" \u{00b7} ctx 18.4k"),
            "absolute usage missing: {:?}",
            rows[0]
        );
        assert!(
            !rows[0].contains('%'),
            "no pct without a window: {:?}",
            rows[0]
        );

        let zero_window = Usage {
            input_tokens: 999,
            output_tokens: 0,
            context_window: Some(0),
        };
        assert_eq!(
            context_usage_segment(&zero_window),
            " \u{00b7} ctx 999",
            "zero window must degrade to absolute"
        );
    }

    /// No usage (fresh app, cleared mid-request, old backend): the segment
    /// vanishes entirely — the strip carries only `cwd` and `model`.
    #[test]
    fn status_strip_omits_the_usage_segment_without_usage() {
        let mut app = App::new(mock::initial_chats());

        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains("cwd:"),
            "strip itself must render: {:?}",
            rows[0]
        );
        assert!(
            !rows[0].contains("ctx"),
            "usage leaked without data: {:?}",
            rows[0]
        );
    }

    /// Turning usage on/off changes the strip ONLY by the segment: same
    /// `cwd` + `model` text, segment appended at its tail (the border-dash
    /// fill shrinks to make room — the strip is right-aligned).
    #[test]
    fn strip_with_usage_differs_from_without_only_by_the_segment() {
        let mut app = App::new(mock::initial_chats());
        let (rows, _) = render_grid_with_buffer(&mut app);
        let without = &rows[0][rows[0].find("cwd:").unwrap()..];

        app.last_turn_usage = Some(Usage {
            input_tokens: 18432,
            output_tokens: 512,
            context_window: Some(131_072),
        });
        let (rows, _) = render_grid_with_buffer(&mut app);
        let with = &rows[0][rows[0].find("cwd:").unwrap()..];
        assert_eq!(
            with,
            format!("{without} \u{00b7} ctx 14% (18.4k/131.0k)"),
            "strip must differ only by the ctx segment"
        );
    }

    /// Ctrl+O parity: the segment rides INSIDE the existing strip — shown
    /// by default together with the strip, hidden once ^O turns it off.
    #[test]
    fn usage_segment_inherits_ctrl_o_visibility() {
        let mut app = App::new(mock::initial_chats());
        app.last_turn_usage = Some(Usage {
            input_tokens: 18432,
            output_tokens: 512,
            context_window: Some(131_072),
        });

        let rows = render_grid(&mut app);
        assert!(
            rows[0].contains("ctx"),
            "usage rides the visible strip by default: {:?}",
            rows[0]
        );

        app.toggle_status_strip();
        let rows = render_grid(&mut app);
        assert!(
            !rows[0].contains("ctx"),
            "usage must hide together with the strip: {:?}",
            rows[0]
        );
    }

    /// A tight budget reads `…onal/chibi`, the working
    /// directory itself never falls under the cut; a roomy budget leaves
    /// the tail untouched.
    #[test]
    fn left_truncate_ellipsis_cuts_from_the_left() {
        assert_eq!(
            left_truncate_ellipsis("/Develop/personal/chibi", 11),
            "\u{2026}onal/chibi"
        );
        assert_eq!(
            left_truncate_ellipsis("/Develop/personal/chibi", 40),
            "/Develop/personal/chibi"
        );
    }

    /// Too-narrow chat pane: the strip yields entirely instead of colliding
    /// with the header title (no panic, no readout, no overflow).
    #[test]
    fn status_strip_skips_when_the_chat_pane_cannot_host_it() {
        let mut app = App::new(mock::initial_chats());
        let (rows, _) = render_grid_at_with_buffer(&mut app, 40, 20);
        assert!(
            !rows[0].contains("cwd:"),
            "strip must not render in a too-narrow pane: {:?}",
            rows[0]
        );
        assert!(
            rows[0].trim_end().width() <= 40,
            "narrow frame overflowed: {} cols",
            rows[0].trim_end().width()
        );
    }

    /// the hints line carries `^O` and, after
    /// compacting `^D del`/`^T panel` to bare tokens, still fits 120 cols
    /// with the longest status label. ^C cancel is never dropped.
    #[test]
    fn status_hints_still_fit_with_status_strip_hint() {
        let mut app = App::new(mock::initial_chats());
        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();

        assert!(last.contains("^O"), "^O missing from hints: {last:?}");
        assert!(
            last.contains("^C cancel"),
            "^C cancel must never be dropped"
        );
        assert!(
            last.contains("disconnected (press R)"),
            "status label clipped — hints overflowed 120 cols: {last:?}"
        );
        assert!(
            last.trim_end().width() <= 120,
            "hints row too wide: {} cols — {:?}",
            last.trim_end().width(),
            last
        );
    }

    /// the `^P` hint is advertised only when the backend
    /// listed the clone command at handshake, and the row still fits 120
    /// cols with the longest status label once it shows.
    #[test]
    fn clone_hint_advertised_only_when_backend_supports_it() {
        let mut app = App::new(mock::initial_chats());
        app.connection = Connection::Disconnected;
        let last = render_grid(&mut app).last().unwrap().clone();
        assert!(
            !last.contains("^P"),
            "clone hint must stay hidden without the capability: {last:?}"
        );

        app.set_backend_commands(vec![
            "/reset".to_owned(),
            "/new_thread_with_current_context".to_owned(),
        ]);
        let last = render_grid(&mut app).last().unwrap().clone();
        assert!(last.contains("^P"), "clone hint missing: {last:?}");
        assert!(
            last.trim_end().width() <= 120,
            "hints row too wide: {} cols — {:?}",
            last.trim_end().width(),
            last
        );
    }

    // popup rendering -----------------------------

    use crate::app::{ModelPickerPhase, ModelPickerState};

    /// A Ready picker over the REAL captured listing (104 rows) with the
    /// selection parked on `selected` (0-based).
    fn picker_app_at(selected: usize) -> App {
        let mut app = App::new(mock::initial_chats());
        let entries = crate::model_picker::parse_model_listing(include_str!(
            "../tests/fixtures/model_listing_captured.txt"
        ));
        assert_eq!(entries.len(), 104);
        app.mode = Mode::ModelPicking {
            state: ModelPickerState {
                phase: ModelPickerPhase::Ready,
                entries,
                selected,
            },
        };
        app
    }

    #[test]
    fn model_picker_popup_renders_rows_hint_and_selection_marker() {
        let mut app = picker_app_at(0);
        let rows = render_grid(&mut app);
        let text: String = rows.join("\n");
        assert!(text.contains("Model picker"), "popup title");
        assert!(text.contains("104 models"), "row count in the title");
        assert!(
            text.contains("1. Qwen3.8 Max (Alibaba)"),
            "first parsed row rendered: {text}"
        );
        assert!(
            text.contains("↑↓ navigate · PgUp/PgDn page · Enter switch · Esc close · Ctrl+C quit"),
            "decision hint on the footer row"
        );
        // The highlighted row carries the ▸ marker.
        let marked = rows
            .iter()
            .any(|r| r.contains('▸') && r.contains("1. Qwen3.8 Max"));
        assert!(marked, "selection marker on the highlighted row");
    }

    #[test]
    fn model_picker_list_scrolls_and_the_selection_stays_on_screen() {
        // Selection on the LAST of 104 rows while the body shows only
        // ~12 rows: the ratatui selection-aware list must auto-scroll the
        // highlighted row into view.
        let mut app = picker_app_at(103);
        let rows = render_grid_at_with_buffer(&mut app, 120, 34).0;
        let text: String = rows.join("\n");
        assert!(
            text.contains("104. GLM 4.7 FlashX (ZhipuAI)"),
            "the selected last row auto-scrolled into view"
        );
        assert!(
            !text.contains("1. Qwen3.8 Max (Alibaba)"),
            "early rows scrolled out of the viewport"
        );

        // And back to the top.
        let mut app = picker_app_at(0);
        let rows = render_grid_at_with_buffer(&mut app, 120, 34).0;
        let text: String = rows.join("\n");
        assert!(text.contains("1. Qwen3.8 Max (Alibaba)"));
        assert!(!text.contains("104. GLM 4.7 FlashX (ZhipuAI)"));
    }

    #[test]
    fn model_picker_loading_phase_renders_a_quiet_placeholder() {
        let mut app = App::new(mock::initial_chats());
        app.begin_model_picker();
        assert!(matches!(app.mode, Mode::ModelPicking { .. }));
        let rows = render_grid(&mut app);
        let text: String = rows.join("\n");
        assert!(text.contains("loading models…"), "in-flight placeholder");
        assert!(
            text.contains("Model picker"),
            "the popup family title renders during the fetch too"
        );
        assert!(
            !text.contains("1. Qwen3.8 Max"),
            "no rows exist before the hidden listing resolves"
        );
    }

    #[test]
    fn picker_render_feeds_the_page_size_seam() {
        let mut app = picker_app_at(0);
        render_grid(&mut app);
        assert_eq!(
            app.picker_visible_rows, 12,
            "the page size is the popup's actual visible row count"
        );

        let mut app = picker_app_at(0);
        render_grid_at_with_buffer(&mut app, 120, 8);
        assert_eq!(
            app.picker_visible_rows, 3,
            "a tiny terminal shrinks the page to the real viewport"
        );
    }

    // rendering ----------------------------

    /// Background reply rendering: the inactive thread's dot turns yellow
    /// (unread-activity slot) and its name goes bold, while the row keeps
    /// the resting panel background (NO selection highlight). The selected
    /// chat keeps its active green dot + highlight, a read neighbour keeps
    /// the default dim dot and an unbold name. The marker survives fresh
    /// re-renders (state, not a one-frame effect).
    #[test]
    fn unread_background_thread_renders_yellow_dot_bold_name_no_highlight() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![
            Chat::new("fresh"),
            Chat::new("marked"),
            Chat::new("quiet"),
        ]);
        app.active = 1; // "marked" selected, both neighbours inactive
        app.chats[0].unread = true;

        let (rows, buf) = render_grid_with_buffer(&mut app);
        // Sidebar rows: " Chats " title on row 0, chat rows start at 1.
        assert!(rows[1].contains("fresh") && rows[2].contains("marked"));

        // Unread inactive dot: hollow glyph in the unread-activity yellow.
        assert_eq!(buf[(0, 1)].symbol(), "\u{25cb}");
        assert_eq!(buf[(0, 1)].fg, theme.unread_activity);
        // Bold dim name, resting background: no highlight.
        assert_eq!(buf[(2, 1)].fg, theme.dim);
        assert!(buf[(2, 1)].modifier.contains(Modifier::BOLD));
        assert_ne!(buf[(2, 1)].bg, theme.selection, "no selection highlight");
        assert_eq!(buf[(2, 1)].bg, theme.panel);

        // Selected chat: active-marker dot, highlight on its row.
        assert_eq!(buf[(0, 2)].fg, theme.active_marker);
        assert_eq!(buf[(2, 2)].bg, theme.selection, "selected row highlighted");

        // Read inactive neighbour: default dot, plain dim name.
        assert_eq!(buf[(0, 3)].fg, theme.dot_default);
        assert!(!buf[(2, 3)].modifier.contains(Modifier::BOLD));

        // Fresh frame + scrolling: the marker is sticky until selection.
        let (_, buf2) = render_grid_with_buffer(&mut app);
        assert_eq!(buf2[(0, 1)].fg, theme.unread_activity, "survives re-render");
        app.scroll_up(5);
        let (_, buf3) = render_grid_with_buffer(&mut app);
        assert_eq!(buf3[(0, 1)].fg, theme.unread_activity, "survives scrolling");
    }

    //------------------------------------------------------------------------

    /// The help modal renders the picker-family visual language (centered
    /// bordered box, blue border, bold title, yellow hint footer) and, swept
    /// across its scroll pages, EVERY row of the const table. One frame
    /// cannot hold all rows by design — the body is a scrolled window — so
    /// the sweep walks the clamped page offsets and unions the frames.
    #[test]
    fn help_modal_renders_every_table_row_across_scroll_pages() {
        let mut app = App::new(mock::initial_chats());
        app.begin_help_modal();
        let total = help_modal_total_lines();
        let page = {
            render_grid(&mut app);
            app.help_visible_rows as usize
        };
        assert!(page > 0 && page < total, "small viewport must paginate");

        let mut seen = String::new();
        let mut scroll = 0;
        loop {
            if let Mode::HelpViewing { state } = &mut app.mode {
                state.scroll = scroll;
            }
            let rows = render_grid(&mut app).join("\n");
            seen.push_str(&rows);
            seen.push('\n');
            if scroll + page >= total {
                break;
            }
            scroll += page;
        }

        assert!(seen.contains(" Keybindings "), "title missing");
        assert!(seen.contains("F1/Esc close"), "hint footer missing");
        for row in HOTKEY_ROWS {
            assert!(
                seen.contains(row.chord),
                "chord {:?} never rendered",
                row.chord
            );
            assert!(
                seen.contains(row.action),
                "action {:?} never rendered",
                row.action
            );
        }
        // Group headers render once per group at the group's first row.
        assert!(seen.contains("Global"));
        assert!(seen.contains("Help (F1)"));
    }

    /// On a small frame the modal is a scrolled window: the top row is only
    /// visible at scroll 0 and the table's last row only after paging to the
    /// bottom clamp, with the render-fed viewport reported back to the App
    /// seam that PgUp/PgDn page by.
    #[test]
    fn help_modal_scrolls_when_the_table_exceeds_the_viewport() {
        let mut app = App::new(mock::initial_chats());
        app.begin_help_modal();
        // 14 rows tall: body = 14 - 4 (hint+borders+title slack) leaves
        // a handful of visible rows — far fewer than the table.
        let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 14);
        let joined = rows.join("\n");
        let page = app.help_visible_rows as usize;
        assert!(page < help_modal_total_lines(), "must paginate");
        assert!(
            joined.contains("open / close this keybindings help"),
            "top row visible at scroll 0: {joined}"
        );
        assert!(
            !joined.contains("copy the cursor line"),
            "bottom rows hidden at scroll 0"
        );

        // Page to the bottom clamp: the last row surfaces, the first row
        // scrolls away.
        let total = help_modal_total_lines();
        for _ in 0..(total / page + 2) {
            app.help_page_down();
        }
        if let Mode::HelpViewing { state } = &mut app.mode {
            assert_eq!(state.scroll, total - page, "bottom clamp");
        }
        let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 14);
        let joined = rows.join("\n");
        assert!(
            joined.contains("Help (F1)") && joined.contains("scroll the list"),
            "last rows visible at the bottom clamp: {joined}"
        );
        assert!(
            !joined.contains("open / close this keybindings help"),
            "top row scrolled away"
        );
    }

    /// The renderer opens a group header on every group CHANGE; duplicated
    /// or interleaved groups would render phantom headers and skew the
    /// total-line count the scroll clamps use.
    #[test]
    fn help_table_groups_are_contiguous() {
        let mut seen = std::collections::HashSet::new();
        let mut prev = "";
        for row in HOTKEY_ROWS {
            if row.group != prev {
                assert!(
                    seen.insert(row.group),
                    "group {:?} appears twice (non-contiguous)",
                    row.group
                );
                prev = row.group;
            }
        }
    }

    // ---- layout rects / panel region (mouse hit-testing) ----

    #[test]
    fn layout_rects_matches_the_draw_layout() {
        let rects = layout_rects(Rect::new(0, 0, 120, 40), 1);
        // Sidebar is the 26-col left column of the main area; the chat
        // pane takes the rest of the width.
        assert_eq!(rects.sidebar.width, 26);
        assert_eq!(rects.chat.x, 26);
        assert_eq!(rects.chat.width, 120 - 26);
        // Root stack: main area of 37 rows, then spinner / input / hints
        // rows of one row each at the bottom.
        assert_eq!(rects.root[0].height, 37);
        assert_eq!(rects.root[1].height, 1);
        assert_eq!(rects.root[2].height, 1);
        assert_eq!(rects.root[3].height, 1);
        assert_eq!(rects.root[3].y, 39);
    }

    #[test]
    fn layout_rects_clamps_the_input_block_on_tiny_frames() {
        // 6-row frame with a 10-row draft: the clamp keeps spinner + hints
        // + a sliver of chat, so the block collapses to 2 rows and the
        // chat pane absorbs what remains.
        let rects = layout_rects(Rect::new(0, 0, 120, 6), 10);
        assert_eq!(rects.root[2].height, 2);
        assert_eq!(rects.root[0].height, 2);
        // A 1-row frame never underflows.
        let rects = layout_rects(Rect::new(0, 0, 40, 1), 1);
        assert_eq!(rects.root[2].height, 1);
    }

    #[test]
    fn chat_column_excludes_the_right_margin_and_matches_the_input_row() {
        let rects = layout_rects(Rect::new(0, 0, 120, 40), 1);
        let column = rects.chat_column();
        // Starts past the sidebar divider, skips the 1-col right margin,
        // and is vertically aligned with the input block (root[2]).
        assert_eq!(column.x, 26);
        assert_eq!(column.width, 120 - 26 - 1);
        assert_eq!(column.y, rects.root[2].y);
        assert_eq!(column.height, rects.root[2].height);
    }

    #[test]
    fn panel_region_hit_tests_the_panels_and_the_chrome() {
        let rects = layout_rects(Rect::new(0, 0, 120, 40), 1);
        // Inside the sidebar column.
        assert_eq!(panel_region(&rects, 10, 5), PanelRegion::Sidebar);
        assert_eq!(panel_region(&rects, 0, 0), PanelRegion::Sidebar);
        // Inside the chat pane (right of the divider).
        assert_eq!(panel_region(&rects, 26, 0), PanelRegion::Chat);
        assert_eq!(panel_region(&rects, 60, 10), PanelRegion::Chat);
        // Chrome rows (spinner / input / hints) are never panels.
        assert_eq!(panel_region(&rects, 60, 37), PanelRegion::Other);
        assert_eq!(panel_region(&rects, 60, 38), PanelRegion::Other);
        assert_eq!(panel_region(&rects, 60, 39), PanelRegion::Other);
        // Out of bounds is Other too.
        assert_eq!(panel_region(&rects, 120, 10), PanelRegion::Other);
        assert_eq!(panel_region(&rects, 60, 40), PanelRegion::Other);
    }

    // ---- mouse text selection: wrap meta + highlight -----------------------

    /// The full wrap exposes per-row hit-test metadata: the owning logical
    /// line, the char range within it (break spaces dropped by the wrap
    /// leave gaps) and the plain text — and the row/first-row outputs stay
    /// identical to the indexed wrapper's.
    #[test]
    fn wrap_message_rows_full_records_logical_and_char_ranges() {
        let lines = vec![plain_line("alpha beta gamma delta"), plain_line("second")];
        let (rows, first_row_of, meta) = wrap_message_rows_full(&lines, 10);
        assert_eq!(rows.len(), meta.len(), "one meta entry per display row");
        assert_eq!(first_row_of, vec![0, 3], "first-row map unchanged");

        // "alpha beta" | "gamma" | "delta": the wrap drops the break
        // spaces, so the char ranges gap (0..10, 11..16, 17..22).
        assert_eq!(meta[0].logical, 0);
        assert_eq!((meta[0].start, meta[0].end), (0, 10));
        assert_eq!(meta[0].text, "alpha beta");
        assert_eq!(meta[1].logical, 0);
        assert_eq!((meta[1].start, meta[1].end), (11, 16));
        assert_eq!(meta[1].text, "gamma");
        assert_eq!(meta[2].text, "delta");
        // The next LOGICAL line starts a fresh range.
        assert_eq!(meta[3].logical, 1);
        assert_eq!((meta[3].start, meta[3].end), (0, 6));
        assert_eq!(meta[3].text, "second");
    }

    /// THE render acceptance criterion: exactly the selected chars take a
    /// REVERSED overlay on top of their own style — unselected text keeps
    /// its style untouched, and rows outside the selection are
    /// byte-identical. Rendered through the full `TestBackend` path with
    /// the selection anchored in display-row space.
    #[test]
    fn selection_highlights_selected_chars_only_in_render() {
        let theme = Theme::tokyo_night();
        let mut app = App::new(vec![Chat::new("sel")]);
        app.chats[0]
            .messages
            .push(Message::assistant("hello world"));
        // First render populates the geometry seam (the same one the
        // mouse router hit-tests through).
        let _ = render_grid_with_buffer(&mut app);
        let geom = app.chat_geometry.clone().expect("geometry cached");
        let r = geom
            .rows
            .iter()
            .position(|m| m.text.contains("hello world"))
            .expect("message row in the geometry");
        // The row's y on the grid: inner top + (row - skip).
        let y = geom.inner.y + (r - geom.skip) as u16;

        app.selection = Some(crate::app::ChatSelection {
            anchor: crate::app::SelectionPoint { row: r, col: 0 },
            head: crate::app::SelectionPoint { row: r, col: 5 },
            dragging: false,
        });
        let (rows, buf) = render_grid_with_buffer(&mut app);
        let row_text = &rows[y as usize];
        let x0 = col_of_sub(row_text, "hello").expect("message rendered");

        // "hello" (5 chars) reversed; the space and "world" not.
        for dx in 0..5usize {
            let cell = &buf[(x0 as u16 + dx as u16, y)];
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "selected char {dx} must be reversed"
            );
        }
        for dx in 5..11usize {
            let cell = &buf[(x0 as u16 + dx as u16, y)];
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "unselected char {dx} must not be reversed"
            );
        }
        // Styling preserved: reversed is an overlay, the fg slot of a
        // selected char equals its unselected neighbor's slot family
        // (plain markdown text renders in the theme fg either way).
        assert_eq!(
            buf[(x0 as u16, y)].fg,
            buf[(x0 as u16 + 6, y)].fg,
            "selected and unselected text share the markdown fg slot"
        );
        assert_eq!(buf[(x0 as u16, y)].fg, theme.fg);

        // Clearing the selection restores the plain render.
        app.clear_selection();
        let (rows_plain, buf_plain) = render_grid_with_buffer(&mut app);
        assert_eq!(rows_plain[y as usize], *row_text, "text identical");
        assert!(
            !buf_plain[(x0 as u16, y)]
                .modifier
                .contains(Modifier::REVERSED),
            "no highlight after the clear"
        );
    }

    /// A selection spanning several wrapped rows highlights its slice on
    /// every affected row: full rows entirely, edge rows partially.
    #[test]
    fn selection_highlight_spans_wrapped_rows() {
        let mut app = App::new(vec![Chat::new("wrap")]);
        // Narrow pane: force the paragraph onto multiple display rows.
        app.chats[0]
            .messages
            .push(Message::assistant("alpha beta gamma delta epsilon"));
        let _ = render_grid_at_with_buffer(&mut app, 50, 24);
        let geom = app.chat_geometry.clone().expect("geometry cached");
        let content: Vec<usize> = geom
            .rows
            .iter()
            .enumerate()
            .filter(|(_, m)| m.text.contains("alpha") || m.text.contains("epsilon"))
            .map(|(i, _)| i)
            .collect();
        assert!(
            content.len() >= 2,
            "precondition: the message wraps over several rows"
        );
        let first = content[0];
        let second = content[1];

        app.selection = Some(crate::app::ChatSelection {
            anchor: crate::app::SelectionPoint { row: first, col: 6 },
            head: crate::app::SelectionPoint {
                row: second,
                col: 5,
            },
            dragging: false,
        });
        let (_, buf) = render_grid_at_with_buffer(&mut app, 50, 24);
        let y_first = geom.inner.y + (first - geom.skip) as u16;
        let y_second = geom.inner.y + (second - geom.skip) as u16;

        // First row: from char 6 to the row end is reversed.
        let reversed_first: Vec<bool> = (0..geom.rows[first].text.chars().count())
            .map(|dx| {
                buf[((geom.inner.x as usize + dx) as u16, y_first)]
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect();
        assert!(!reversed_first[5], "chars before the anchor stay plain");
        assert!(
            reversed_first[6..].iter().all(|&rev| rev),
            "the anchor-to-end slice is reversed: {reversed_first:?}"
        );
        // Second row: chars 0..5 reversed (its slice of the selection).
        for dx in 0..5usize {
            assert!(
                buf[((geom.inner.x as usize + dx) as u16, y_second)]
                    .modifier
                    .contains(Modifier::REVERSED),
                "second-row slice char {dx} reversed"
            );
        }
    }
}
