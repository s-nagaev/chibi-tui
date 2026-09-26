//! Mouse event routing for the TUI: wheel scrolling across surfaces and
//! pointer-driven text selection in the chat pane.
//!
//! [`handle_mouse`] is called by the event loop for every mouse event;
//! [`handle_mouse_with_copy`] is the testable form taking the clipboard
//! write as a closure (see its docs). Extracted from `main.rs`, which
//! keeps the keyboard/mode dispatch in the same shape.

use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use chibi_tui::app::{Focus, Mode};
use chibi_tui::ui;

/// Wheel step per notch: chat rows in the chat panel, one step per entry /
/// line in the modal list surfaces.
pub const WHEEL_STEP: u16 = 3;

/// Which surface receives the wheel: a modal that owns the screen gets it
/// wherever the cursor is (same modal isolation as the keyboard), the base
/// panels get it routed by cursor position.
pub enum WheelSurface {
    LogViewer,
    Help,
    ModelPicker,
    Panels,
}

/// Route a crossterm mouse event. Wheel notches scroll (chat, sidebar,
/// modals); left press/drag/release inside the chat pane drive the text
/// selection (see below). Everything else is ignored.
///
/// Routing rules:
/// - an open log viewer / help modal / model picker consumes the wheel
///   regardless of cursor position, mirroring how those modals swallow
///   every key — and their other mouse events stay ignored (a modal owns
///   the whole screen, so no pointer selection can start under it);
/// - cursor over the CHAT panel scrolls the chat (`App::scroll_up` unpins
///   from follow-bottom, `scroll_down` re-pins at 0; `ui::scroll_skip`
///   clamps at render);
/// - cursor over the SIDEBAR moves the thread selection ONLY while the
///   sidebar holds keyboard focus — hovering without focus intentionally
///   does nothing (hover-switching threads was judged too noisy UX);
/// - a left PRESS inside the chat pane (Normal mode, no error popup)
///   starts a drag selection through the render-fed `App::chat_geometry`
///   hit-test seam; DRAG moves the head; RELEASE copies the selected
///   plain text through the SAME clipboard path as the log viewer's `y`
///   (`App::release_selection` turns a press+release without drag into a
///   plain click that clears). Presses outside the chat pane clear the
///   selection too, and a release is finalized anywhere in the base
///   surface — the cursor commonly leaves the pane before the button
///   comes up.
pub fn handle_mouse(app: &mut chibi_tui::app::App, mouse: MouseEvent, area: Rect) {
    handle_mouse_with_copy(app, mouse, area, &|text| {
        // Selection copy rides the SAME clipboard path as the log
        // viewer's `y` (OSC 52 + the env fallback); the outcome is
        // deliberately ignored — a failed write must not disturb the UI.
        let _ = chibi_tui::clipboard::copy_text(text);
    });
}

/// Testable form of [`handle_mouse`]: the clipboard write arrives as a
/// closure (production passes [`chibi_tui::clipboard::copy_text`], tests
/// capture into a buffer), so the dispatch flow can assert the copy
/// without touching a real clipboard.
pub fn handle_mouse_with_copy(
    app: &mut chibi_tui::app::App,
    mouse: MouseEvent,
    area: Rect,
    copy: &dyn Fn(&str),
) {
    let surface = match &app.mode {
        Mode::LogViewer { .. } => WheelSurface::LogViewer,
        Mode::HelpViewing { .. } => WheelSurface::Help,
        Mode::ModelPicking { .. } => WheelSurface::ModelPicker,
        _ => WheelSurface::Panels,
    };
    match surface {
        WheelSurface::LogViewer => match mouse.kind {
            MouseEventKind::ScrollUp => app.log_cursor_up(usize::from(WHEEL_STEP)),
            MouseEventKind::ScrollDown => app.log_cursor_down(usize::from(WHEEL_STEP)),
            _ => {}
        },
        WheelSurface::Help => match mouse.kind {
            MouseEventKind::ScrollUp => {
                for _ in 0..WHEEL_STEP {
                    app.help_scroll_up();
                }
            }
            MouseEventKind::ScrollDown => {
                for _ in 0..WHEEL_STEP {
                    app.help_scroll_down();
                }
            }
            _ => {}
        },
        WheelSurface::ModelPicker => match mouse.kind {
            MouseEventKind::ScrollUp => {
                for _ in 0..WHEEL_STEP {
                    app.model_picker_select_prev();
                }
            }
            MouseEventKind::ScrollDown => {
                for _ in 0..WHEEL_STEP {
                    app.model_picker_select_next();
                }
            }
            _ => {}
        },
        WheelSurface::Panels => {
            let rects = ui::layout_rects(area, app.input_lines_height());
            // Selection presses/drags/releases are Normal-mode-only and
            // popup-free: popups (confirm dialogs, rename, search) and the
            // error popup own the whole screen — the pointer must not draw
            // a selection under them, and events under them are swallowed
            // (same isolation as the keyboard). A selection started before
            // a popup opened stays held underneath; a stale live drag is
            // harmless — the next Normal-mode click or Esc clears it.
            // The quit-confirm popup is checked explicitly: it is a flag,
            // not a Mode, and the pointer must not select (nor copy on
            // release) under it. WHEEL routing deliberately stays live
            // under it, mirroring the error popup: scrolling the chat
            // behind a popup is harmless and pre-existing behavior; only
            // pointer-driven selection/interaction is swallowed.
            // The wheel keeps its pre-existing routing below.
            let selectable = app.mode.is_normal() && app.error_popup.is_none() && !app.quit_confirm;
            match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    match ui::panel_region(&rects, mouse.column, mouse.row) {
                        ui::PanelRegion::Chat => match mouse.kind {
                            MouseEventKind::ScrollUp => app.scroll_up(WHEEL_STEP),
                            _ => app.scroll_down(WHEEL_STEP),
                        },
                        ui::PanelRegion::Sidebar if app.focus == Focus::Sidebar => {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => app.select_prev(),
                                _ => app.select_next(),
                            }
                        }
                        _ => {}
                    }
                }
                // Finalize the drag anywhere in the base surface (before
                // the region hit-test: the release point may have left the
                // pane).
                MouseEventKind::Up(crossterm::event::MouseButton::Left) if selectable => {
                    if let Some(text) = app.release_selection() {
                        copy(&text);
                    }
                }
                MouseEventKind::Down(crossterm::event::MouseButton::Left) if selectable => {
                    let in_chat =
                        ui::panel_region(&rects, mouse.column, mouse.row) == ui::PanelRegion::Chat;
                    let point = in_chat.then(|| {
                        app.chat_geometry
                            .as_ref()
                            .and_then(|g| g.position_at(mouse.column, mouse.row))
                    });
                    match point.flatten() {
                        Some(point) => app.begin_selection(point),
                        // A press outside the chat pane is a plain click:
                        // it clears the selection (the release completes
                        // the click; nothing live to release).
                        None => app.clear_selection(),
                    }
                }
                MouseEventKind::Drag(crossterm::event::MouseButton::Left) if selectable => {
                    if let Some(point) = app
                        .chat_geometry
                        .as_ref()
                        .and_then(|g| g.position_at(mouse.column, mouse.row))
                    {
                        app.drag_selection(point);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests;
