//! Ratatui layout: sidebar (chat list) + chat view + spinner line + input +
//! hotkey status line.

use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
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
    // quit-confirmation popup (rendered last — TOPMOST: it may be opened
    // from any state, including above another popup, without disturbing
    // the state below; dismissing restores it exactly).
    if app.quit_confirm {
        render_quit_confirm(f, theme);
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
///
/// The sidebar is a FULL rounded rectangle: all four borders (`╭╮╰╯`
/// corners) rendered inside the existing sidebar rect — the former divider
/// column is the right border, the frame edge column hosts the left one —
/// so the outer frame dimensions and the hit-test rects are unchanged.
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
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(if sidebar_focused {
                    // Emphasis: the focused sidebar's outline lifts to blue.
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

    // Extend the sidebar's right rounded border down through EVERY bottom
    // row: spinner + the grown editor block (1..=20 rows) + hotkey hints,
    // so the divider runs unbroken from the pane's ╯ corner to the status
    // line at any editor height.
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
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        // The border color stays the resting selection tone in BOTH focuses
        // (the reference design keeps panel borders constant; the sidebar
        // carries the focus emphasis alone).
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

/// the strip as a right-aligned top-border title [`Line`] for
/// the chat pane, dim-styled. Squeezed into the columns LEFT of the
/// left-aligned header title inside the title row (1-column gutter, the two
/// rounded border columns excluded): too-narrow panes yield
/// `None` and the strip simply does not render this frame.
fn status_strip_title<'a>(
    app: &'a App,
    theme: &Theme,
    area_width: u16,
    left_title: &str,
) -> Option<Line<'a>> {
    let gutter = 1u16;
    // The two rounded border columns are no longer hostable by the title
    // row: the strip must fit between the corners (and clear of the left
    // title), so they come out of the budget too.
    let available = area_width.saturating_sub(left_title.width() as u16 + gutter + 2) as usize;
    if available == 0 {
        return None;
    }
    let text = left_truncate_ellipsis(&status_strip_text(app, available), available);
    // Alignment rides on the Line itself (ratatui groups top titles by the
    // line's own alignment; the old `block::Title` struct is gone in 0.30).
    Some(Line::from(Span::styled(text, Style::new().fg(theme.dim))).alignment(Alignment::Right))
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
/// columns over the full block height — the editor keeps the cursor
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
/// key dispatch (`main.rs::handle_key` + the readline fall-through)
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
        action: "cancel the active request · quit when idle (confirmation)",
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
        chord: "y",
        action: "copy the active chat selection",
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
        action: "quit (confirmation)",
    },
    HotkeyRow {
        group: "Error popup",
        chord: "Esc · any key",
        action: "dismiss",
    },
    HotkeyRow {
        group: "Quit confirm",
        chord: "y · Enter",
        action: "quit",
    },
    HotkeyRow {
        group: "Quit confirm",
        chord: "Esc · n · q",
        action: "stay",
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

/// Centered modal quit-confirmation banner over the full frame. Rendered
/// LAST (topmost) so it can sit above any other open popup — opening it
/// never disturbs the state below. Compact rounded banner: the title sits
/// in the top border, the question and the decision hint each take one
/// inner row — 4 rows total when the terminal allows. Purely visual — all
/// key handling lives in `main.rs` (in-app popup) and in the splash /
/// setup loops (the same renderer over their own frames).
pub fn render_quit_confirm(f: &mut Frame, theme: &Theme) {
    use ratatui::widgets::{BorderType, Clear, Padding};

    let message = "Quit chibi-tui?";
    let hint = "y/Enter quit \u{00b7} Esc/n stay";

    // Content-hugging banner geometry: width follows the wider of the two
    // inner rows (1-cell padding per side, 2 border cells) with a modest
    // 30-column floor, height is exactly the two text rows plus borders.
    let max_w = f.area().width.saturating_sub(4).max(20);
    let width = (message.width().max(hint.width()) as u16 + 4)
        .max(30)
        .clamp(20, max_w);
    let height = 4.min(f.area().height.saturating_sub(2)).max(3);
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
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.red))
        .style(Style::new().bg(theme.bg))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            " Quit ",
            Style::new().fg(theme.red).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height > 0 {
        let mut lines: Vec<Line<'static>> = wrap_text(message, inner.width.max(1) as usize)
            .into_iter()
            .map(|line| {
                Line::from(
                    line.spans
                        .into_iter()
                        .map(|span| Span::styled(span.content, Style::new().fg(theme.fg)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let hint_line = Line::from(Span::styled(hint, Style::new().fg(theme.yellow)));
        lines.push(hint_line); // rendered last; clipped when out of room
        let text = Text::from(lines);
        let visible = inner.height as usize;
        let skip = text.height().saturating_sub(visible);
        let paragraph = Paragraph::new(text).scroll((skip as u16, 0));
        f.render_widget(paragraph, inner);
    }
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
mod tests;
