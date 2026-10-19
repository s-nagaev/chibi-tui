use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};
use ratatui::layout::Rect;

use super::{handle_mouse, handle_mouse_with_copy, WHEEL_STEP};
use crate::keymap::handle_key_with_copy;
use crate::keymap::tests::log_viewer::log_viewer_state;
use crate::tests::support::*;

// ---- mouse wheel routing --------------------------------------------

/// Hand-built wheel notch at a terminal position, as crossterm delivers it.
fn wheel(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

use crossterm::event::MouseEventKind;

/// 120×40 frame: sidebar cols 0..=25, chat cols 26.., chrome rows 37..39.
const WHEEL_AREA: Rect = Rect::new(0, 0, 120, 40);

#[test]
fn wheel_up_over_the_chat_unpins_from_follow_bottom_by_the_wheel_step() {
    let mut app = app_with_chats(1);
    assert!(app.at_bottom(), "opens pinned to the tail");
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollUp, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(app.scroll, 3, "one notch = WHEEL_STEP rows");
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollUp, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(app.scroll, 6);
    assert!(!app.at_bottom());
}

#[test]
fn wheel_down_over_the_chat_returns_to_follow_bottom() {
    let mut app = app_with_chats(1);
    app.scroll_up(6);
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(app.scroll, 3);
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(app.scroll, 0, "saturates at 0 = follow-bottom re-pinned");
    assert!(app.at_bottom());
    // further down-notches stay pinned: scroll can never go negative.
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(app.scroll, 0);
}

#[test]
fn wheel_over_the_sidebar_without_focus_does_nothing() {
    use chibi_tui::app::Focus;
    let mut app = app_with_chats(3);
    assert_eq!(app.focus, Focus::Chat);
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 10, 5),
        WHEEL_AREA,
    );
    assert_eq!(app.active, 0, "hovering must never switch threads");
    handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, 10, 5), WHEEL_AREA);
    assert_eq!(app.active, 0);
    assert_eq!(app.scroll, 0, "sidebar hover never scrolls the chat either");
}

#[test]
fn wheel_over_the_focused_sidebar_moves_the_selection() {
    use chibi_tui::app::Focus;
    let mut app = app_with_chats(3);
    app.focus = Focus::Sidebar;
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 10, 5),
        WHEEL_AREA,
    );
    assert_eq!(app.active, 1, "wheel down = next thread (live switching)");
    handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, 10, 5), WHEEL_AREA);
    assert_eq!(app.active, 0, "wheel up = previous thread");
    // The top clamp holds: no wraparound past the first thread.
    handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, 10, 5), WHEEL_AREA);
    assert_eq!(app.active, 0);
}

#[test]
fn wheel_over_the_chrome_rows_is_ignored() {
    let mut app = app_with_chats(2);
    // spinner / input / hints rows (y >= 37) are no panel.
    for row in [37, 38, 39] {
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollUp, 60, row),
            WHEEL_AREA,
        );
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, row),
            WHEEL_AREA,
        );
    }
    assert_eq!(app.scroll, 0);
    assert_eq!(app.active, 0);
}

#[test]
fn non_wheel_mouse_events_are_ignored() {
    let mut app = app_with_chats(2);
    app.scroll_up(9);
    let kinds = [
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        MouseEventKind::Moved,
    ];
    for kind in kinds {
        handle_mouse(&mut app, wheel(kind, 60, 10), WHEEL_AREA);
        handle_mouse(&mut app, wheel(kind, 10, 5), WHEEL_AREA);
    }
    assert_eq!(app.scroll, 9, "clicks/drag/motion never touch the scroll");
    assert_eq!(app.active, 0);
}

#[test]
fn wheel_inside_the_open_help_modal_scrolls_the_table() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    let scroll = |app: &chibi_tui::app::App| match &app.mode {
        chibi_tui::app::Mode::HelpViewing { state } => state.scroll,
        _ => panic!("modal must stay open"),
    };
    // The modal owns the wheel wherever the cursor is — same isolation
    // as the keyboard.
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(scroll(&app), 3, "three lines per notch");
    handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, 10, 5), WHEEL_AREA);
    assert_eq!(scroll(&app), 0, "clamped at the top edge");
}

#[test]
fn wheel_inside_the_open_log_viewer_moves_the_cursor() {
    let mut app = app_with_chats(1);
    app.mode = chibi_tui::app::Mode::LogViewer {
        state: log_viewer_state(
            5,
            vec![
                "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            ],
            None,
        ),
    };
    let cursor = |app: &chibi_tui::app::App| match &app.mode {
        chibi_tui::app::Mode::LogViewer { state } => state.cursor,
        _ => panic!("viewer must stay open"),
    };
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollUp, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(cursor(&app), 2, "wheel up walks three logical lines up");
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 60, 10),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(cursor(&app), 8);
}

#[test]
fn wheel_inside_the_open_model_picker_moves_the_selection() {
    let mut app = app_with_chats(1);
    inject_ready_picker(&mut app, 10);
    let selected = |app: &chibi_tui::app::App| match &app.mode {
        chibi_tui::app::Mode::ModelPicking { state } => state.selected,
        _ => panic!("picker must stay open"),
    };
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(selected(&app), 3, "wheel down walks three rows");
    // The bottom clamp holds: no wraparound past the last row.
    for _ in 0..10 {
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, 10),
            WHEEL_AREA,
        );
    }
    assert_eq!(selected(&app), 9);
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollUp, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(selected(&app), 6);
}

// ---- mouse text selection dispatch --------------------------------------

use chibi_tui::app::ChatSelection;

/// Render one frame so the renderer caches `App::chat_geometry` (the
/// hit-test seam the mouse selection maps positions through).
fn render_for_geometry(app: &mut chibi_tui::app::App) {
    let backend = ratatui::backend::TestBackend::new(WHEEL_AREA.width, WHEEL_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| chibi_tui::ui::draw(f, app, &chibi_tui::theme::Theme::tokyo_night()))
        .unwrap();
}

/// Left-button press / drag / release at a terminal position.
fn button(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> MouseEvent {
    wheel(kind, column, row)
}

/// THE dispatch flow: press in the chat pane anchors, drag extends,
/// release finalizes and copies the plain text through the injected
/// clipboard seam.
#[test]
fn press_drag_release_selects_and_copies_through_the_seam() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::assistant("hello world from selection"));
    render_for_geometry(&mut app);

    let copied = std::cell::RefCell::new(Vec::<String>::new());
    {
        let sink = &copied;
        let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
        // Press at the first content cell of the message row, drag
        // eleven columns right ("hello world"), release.
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                27,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        assert!(
            app.selection.as_ref().is_some_and(|s| s.dragging),
            "press starts the live drag"
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                38,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                40,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
    }

    let sel = app.selection.expect("real selection held after release");
    assert!(!sel.dragging, "released");
    assert_eq!(
        copied.borrow().as_slice(),
        ["hello world"],
        "the release copied the selected plain text"
    );
}

/// A press+release without drag is a plain click: the selection clears
/// and nothing is copied.
#[test]
fn plain_click_clears_and_copies_nothing() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::assistant("text here"));
    render_for_geometry(&mut app);

    let copied = std::cell::RefCell::new(Vec::<String>::new());
    let sink = &copied;
    let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
    // Start a real selection first, then plain-click it away.
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            27,
            2,
        ),
        WHEEL_AREA,
        &copy,
    );
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            31,
            2,
        ),
        WHEEL_AREA,
        &copy,
    );
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            31,
            2,
        ),
        WHEEL_AREA,
        &copy,
    );
    assert!(app.selection.is_some(), "drag held a selection");
    assert_eq!(
        copied.borrow().as_slice(),
        ["text"],
        "the drag release copied the selection"
    );

    // Fresh sink for the click phase.
    let click_copied = std::cell::RefCell::new(Vec::<String>::new());
    let click_sink = &click_copied;
    let click_copy = |text: &str| click_sink.borrow_mut().push(text.to_owned());
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            31,
            2,
        ),
        WHEEL_AREA,
        &click_copy,
    );
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            31,
            2,
        ),
        WHEEL_AREA,
        &click_copy,
    );
    assert!(app.selection.is_none(), "plain click cleared");
    assert!(click_sink.borrow().is_empty(), "plain click copied nothing");
}

/// `y` with a held chat selection copies its plain text through the
/// SAME injected clipboard seam as the release copy, and the selection
/// STAYS active afterwards (the user still sees what they copied).
#[test]
fn y_copies_the_held_selection_and_keeps_it_active() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::assistant("hello world from selection"));
    render_for_geometry(&mut app);

    // Build a real selection through the mouse dispatch ("hello").
    let copied = std::cell::RefCell::new(Vec::<String>::new());
    {
        let sink = &copied;
        let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                27,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                32,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                31,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        assert_eq!(
            copied.borrow().as_slice(),
            ["hello"],
            "the release copied the dragged range"
        );
        copied.borrow_mut().clear();
    }
    assert!(app.selection.is_some(), "release held the selection");

    // `y` copies through the keyboard seam — WITHOUT clearing.
    let y_copied = std::cell::RefCell::new(Vec::<String>::new());
    {
        let sink = &y_copied;
        let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
        handle_key_with_copy(
            &mut app,
            key_event(KeyCode::Char('y'), KeyModifiers::NONE),
            &copy,
        );
    }
    assert_eq!(
        y_copied.borrow().as_slice(),
        ["hello"],
        "y copied the selected plain text"
    );
    assert!(
        app.selection.is_some(),
        "the selection stays active after the copy"
    );

    // A later Esc clears it as before (the deselect path is untouched).
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.selection.is_none(), "Esc still deselects after a copy");
}

/// Without a selection `y` is NOT claimed by the copy binding: nothing
/// is copied and the pre-existing behavior (plain typing into the
/// draft) is preserved — no binding is shadowed.
#[test]
fn y_without_a_selection_copies_nothing_and_types_into_the_draft() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "draf");

    let copied = std::cell::RefCell::new(Vec::<String>::new());
    {
        let sink = &copied;
        let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
        handle_key_with_copy(
            &mut app,
            key_event(KeyCode::Char('y'), KeyModifiers::NONE),
            &copy,
        );
    }
    assert!(
        copied.borrow().is_empty(),
        "nothing selected, nothing copied"
    );
    assert_eq!(
        app.input.lines().join("\n"),
        "drafy",
        "plain y keeps its typing behavior without a selection"
    );
}

/// Esc clears the selection    /// Esc clears the selection (the keyboard's deselect), including a
/// live drag, and never quits.
#[test]
fn esc_clears_the_mouse_selection() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::assistant("text here"));
    render_for_geometry(&mut app);
    let noop = |_text: &str| {};

    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            27,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            35,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    assert!(app.selection.is_some());

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.selection.is_none(), "Esc cleared the selection");
    assert!(!app.should_quit);
}

/// A press outside the chat pane (sidebar / chrome rows) is a plain
/// click: it clears the selection instead of starting one.
#[test]
fn press_outside_the_chat_pane_clears_the_selection() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::assistant("text here"));
    render_for_geometry(&mut app);
    let noop = |_text: &str| {};

    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            27,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            31,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    assert!(app.selection.is_some(), "precondition");

    // Sidebar press (no focus — no thread switch either).
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            5,
        ),
        WHEEL_AREA,
        &noop,
    );
    assert!(app.selection.is_none(), "sidebar press cleared");
    assert_eq!(app.active, 0, "no hover switching");
}

/// Wheel scrolling during a live drag leaves the head in document-row
/// space (the documented choice: the next drag event re-extends the
/// selection; scroll alone never moves it) and the scroll itself works.
#[test]
fn wheel_during_a_drag_scrolls_without_moving_the_head() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::assistant("hello world"));
    render_for_geometry(&mut app);
    let noop = |_text: &str| {};

    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            27,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            35,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    let head_before = app.selection.expect("live drag").head;

    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollUp, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(app.scroll, WHEEL_STEP, "the wheel still scrolls");
    assert_eq!(
        app.selection.expect("drag survives the wheel").head,
        head_before,
        "scroll does not move the head (document-row space choice)"
    );
}

/// A selection press while a modal owns the screen (delete-confirm
/// popup open) is ignored — the popup owns the pointer too — and a
/// selection made before the popup stays held underneath.
#[test]
fn selection_presses_are_ignored_while_a_modal_is_open() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::assistant("text here"));
    render_for_geometry(&mut app);
    let noop = |_text: &str| {};

    // Make a selection, then open the delete-confirm popup.
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            27,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            31,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    assert!(app.selection.is_some(), "precondition");
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);

    // Press under the popup: ignored (the selection is untouched —
    // the popup branch consumes the mouse like the keyboard).
    handle_mouse_with_copy(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            30,
            2,
        ),
        WHEEL_AREA,
        &noop,
    );
    assert!(
        matches!(app.selection, Some(ChatSelection { dragging: true, .. })),
        "popup press must not start a new selection nor clear"
    );
}

/// Mouse selection under the quit-confirm popup is swallowed: a press
/// must not start a new selection nor clear a held one, and a release
/// must not copy. The popup is a flag (not a Mode), so the mouse
/// `selectable` guard checks it explicitly — same isolation as the
/// delete-confirm popup. The wheel deliberately keeps scrolling (the
/// same routing the error popup gets; scrolling behind a popup is
/// harmless pre-existing behavior).
#[test]
fn selection_under_quit_confirm_is_swallowed() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::assistant("text here"));
    render_for_geometry(&mut app);

    let copied = std::cell::RefCell::new(Vec::<String>::new());
    {
        let sink = &copied;
        let copy = |text: &str| sink.borrow_mut().push(text.to_owned());

        // Make a selection, then open the quit confirm (idle Ctrl+C).
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                27,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                31,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        assert!(app.selection.is_some(), "precondition");
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit_confirm, "precondition: popup open");

        // Press under the popup: no new selection, nothing cleared.
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                34,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        assert!(
            matches!(app.selection, Some(ChatSelection { dragging: true, .. })),
            "press under the popup must not start a selection nor clear"
        );

        // Release under the popup: finalized nowhere, nothing copied.
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                36,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
    }
    assert!(
        copied.borrow().is_empty(),
        "release under the popup must not copy"
    );
    assert!(
        app.selection.is_some(),
        "the held selection stays parked underneath the popup"
    );

    // The wheel still scrolls the chat behind the popup (documented
    // routing choice, mirroring the error popup).
    let scroll_before = app.scroll;
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollUp, 60, 10),
        WHEEL_AREA,
    );
    assert_eq!(
        app.scroll,
        scroll_before + WHEEL_STEP,
        "the wheel keeps its routing under the popup"
    );
    assert!(app.quit_confirm, "the wheel must not disturb the popup");
}

// ---- config editor surface (Mode::ConfigEditor → WheelSurface::Config) --

use chibi_tui::app::Mode;
use chibi_tui::config_screen::ConfigState;

/// An app with the F2 config editor open over a temp settings file (never
/// the real `~/chibi-bot/settings`). Returns the app and the file path.
fn config_app(tag: &str) -> (chibi_tui::app::App, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-mouse-config-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("settings");
    std::fs::write(&path, "DEFAULT_MODEL=glm-5\n").expect("write fixture");
    let mut app = app_with_chats(1);
    app.mode = Mode::ConfigEditor {
        state: ConfigState::load_at(path.clone(), true),
    };
    (app, path)
}

fn config_state(app: &mut chibi_tui::app::App) -> &mut ConfigState {
    match &mut app.mode {
        Mode::ConfigEditor { state } => state,
        _ => panic!("config editor must stay open"),
    }
}

/// Render one full frame (the config editor owns it) and return the buffer
/// as a grid of lines, one per terminal row. Positions are located
/// dynamically because the component's `row` and its geometry fields are
/// private AND the frame may carry the dev banner (`CHIBI_ENV=dev` leaks
/// between parallel tests), which shifts every pane down by one row.
fn render_config_grid(app: &mut chibi_tui::app::App) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(WHEEL_AREA.width, WHEEL_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| chibi_tui::ui::draw(f, app, &chibi_tui::theme::Theme::tokyo_night()))
        .unwrap();
    let cells: Vec<String> = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    cells
        .chunks(WHEEL_AREA.width as usize)
        .map(|row| row.concat())
        .collect()
}

/// Index of the grid row containing `needle` (the pane positions in tests).
fn grid_row(grid: &[String], needle: &str) -> usize {
    grid.iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` not on screen"))
}

#[test]
fn config_mode_wheel_routes_to_the_config_editor_by_pane() {
    let (mut app, _path) = config_app("cfgwheel");
    let grid = render_config_grid(&mut app);
    // Cursor inside the LEFT groups pane, on the selected group's row.
    let gy = grid_row(&grid, "LLM Providers");
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 5, gy as u16),
        WHEEL_AREA,
    );
    assert_eq!(
        config_state(&mut app).group_index(),
        1,
        "one notch = one group"
    );
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollUp, 5, gy as u16),
        WHEEL_AREA,
    );
    assert_eq!(config_state(&mut app).group_index(), 0);
    // The top edge clamps, no rem_euclid wraparound to the last group.
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollUp, 5, gy as u16),
        WHEEL_AREA,
    );
    assert_eq!(config_state(&mut app).group_index(), 0);
}

#[test]
fn config_mode_wheel_over_the_rows_pane_moves_the_selection() {
    let (mut app, _path) = config_app("cfgrows");
    let grid = render_config_grid(&mut app);
    assert!(
        grid.iter().any(|l| l.contains("▸ OPENAI_API_KEY")),
        "row 0 starts selected"
    );
    // Cursor over the RIGHT rows pane, on the currently selected row:
    // ONE notch walks the selection WHEEL_STEP = 3 rows down (OPENAI →
    // row 3, GROK).
    let kr = grid_row(&grid, "OPENAI_API_KEY");
    let kc = grid[kr].find("OPENAI_API_KEY").unwrap() as u16;
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, kc, kr as u16),
        WHEEL_AREA,
    );
    let after = render_config_grid(&mut app);
    assert!(
        after.iter().any(|l| l.contains("▸ GROK_API_KEY")),
        "the selection moved 3 rows"
    );
    assert!(
        !after.iter().any(|l| l.contains("▸ OPENAI_API_KEY")),
        "row 0 is deselected"
    );
}

#[test]
fn config_mode_click_selects_the_group_under_the_cursor() {
    let (mut app, _path) = config_app("cfgclick");
    let grid = render_config_grid(&mut app); // populates the geometry
                                             // Click the "Integrations" group row (group 2) in the LEFT pane.
    let iy = grid_row(&grid, "Integrations") as u16;
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            5,
            iy,
        ),
        WHEEL_AREA,
    );
    assert_eq!(config_state(&mut app).group_index(), 2);
    // Click on the divider column between the panes → no-op.
    let kr = grid_row(&grid, "OPENAI_API_KEY");
    let divider = grid[kr].find("OPENAI_API_KEY").unwrap() as u16 - 3;
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            divider,
            kr as u16,
        ),
        WHEEL_AREA,
    );
    // Click outside both panes (the outer border) → no-op.
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            0,
            0,
        ),
        WHEEL_AREA,
    );
    assert_eq!(config_state(&mut app).group_index(), 2, "no-op clicks");
}

#[test]
fn config_mode_clicks_are_swallowed_under_quit_confirm_wheel_stays_live() {
    let (mut app, _path) = config_app("cfgquit");
    let grid = render_config_grid(&mut app);
    let gy = grid_row(&grid, "LLM Providers") as u16;
    let my = grid_row(&grid, "Model Routing") as u16;
    // Ctrl+C from the config editor opens the quit confirm (a flag, not a
    // Mode; it renders TOPMOST over the editor).
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm, "precondition: popup open");

    // A click that would select group 3 ("Model Routing") is NOT forwarded.
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            5,
            my,
        ),
        WHEEL_AREA,
    );
    assert_eq!(config_state(&mut app).group_index(), 0, "click swallowed");
    assert!(app.quit_confirm, "the click must not disturb the popup");

    // The wheel stays live (the Panels convention).
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 5, gy),
        WHEEL_AREA,
    );
    assert_eq!(config_state(&mut app).group_index(), 1, "wheel forwarded");
    assert!(app.quit_confirm);
}

#[test]
fn config_mode_mouse_is_swallowed_while_a_confirm_modal_is_open() {
    let (mut app, path) = config_app("cfgdiscard");
    let grid = render_config_grid(&mut app);
    let gy = grid_row(&grid, "LLM Providers") as u16;
    let my = grid_row(&grid, "Model Routing") as u16;
    let kr = grid_row(&grid, "OPENAI_API_KEY") as u16;
    // Discard: stage an edit, Esc on the dirty screen.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_in(&mut app, "x");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // stage
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // dirty → confirm
    assert_eq!(
        config_state(&mut app).confirm(),
        chibi_tui::config_screen::Confirm::Discard,
        "precondition"
    );
    // Wheel and clicks anywhere (both panes) are swallowed.
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 5, gy),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            5,
            my,
        ),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 40, kr),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            40,
            kr,
        ),
        WHEEL_AREA,
    );
    assert_eq!(
        config_state(&mut app).confirm(),
        chibi_tui::config_screen::Confirm::Discard,
        "modal owns the screen"
    );
    assert_eq!(config_state(&mut app).group_index(), 0, "groups untouched");
    assert!(
        config_state(&mut app).dirty(),
        "mouse never dismissed nor saved"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // stay
    assert_eq!(
        config_state(&mut app).confirm(),
        chibi_tui::config_screen::Confirm::None
    );

    // Restart: a clean save stages the restart offer (idle variant).
    {
        let state = config_state(&mut app);
        assert_eq!(
            state.perform_save(false),
            chibi_tui::config_screen::SaveOutcome::Saved
        );
        state.after_save(false);
    }
    assert_eq!(
        config_state(&mut app).confirm(),
        chibi_tui::config_screen::Confirm::Restart,
        "precondition"
    );
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 5, gy),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            5,
            my,
        ),
        WHEEL_AREA,
    );
    assert_eq!(
        config_state(&mut app).confirm(),
        chibi_tui::config_screen::Confirm::Restart
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // stay

    // RestartBusy: the same offer with a request in flight.
    config_state(&mut app).after_save(true);
    assert_eq!(
        config_state(&mut app).confirm(),
        chibi_tui::config_screen::Confirm::RestartBusy
    );
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 5, gy),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            5,
            my,
        ),
        WHEEL_AREA,
    );
    assert_eq!(
        config_state(&mut app).confirm(),
        chibi_tui::config_screen::Confirm::RestartBusy
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    // FileChanged: the file changes on disk between load and save: the
    // write guard opens the reload/overwrite modal.
    std::fs::write(&path, "DEFAULT_MODEL=external\n").expect("external edit");
    assert_eq!(
        config_state(&mut app).perform_save(false),
        chibi_tui::config_screen::SaveOutcome::ChangedOnDisk,
        "precondition"
    );
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 5, gy),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            5,
            my,
        ),
        WHEEL_AREA,
    );
    assert_eq!(
        config_state(&mut app).confirm(),
        chibi_tui::config_screen::Confirm::FileChanged,
        "mouse events are swallowed under every confirm variant"
    );
    assert_eq!(config_state(&mut app).group_index(), 0);
}

#[test]
fn config_mode_mouse_is_swallowed_while_the_edit_input_is_open() {
    let (mut app, _path) = config_app("cfgedit");
    let grid = render_config_grid(&mut app);
    let kr = grid_row(&grid, "OPENAI_API_KEY") as u16;
    let kc = grid[kr as usize].find("OPENAI_API_KEY").unwrap() as u16;
    // Enter on the selected OPENAI_API_KEY row (a secret) opens the editor;
    // the footer then advertises the edit hints.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        render_config_grid(&mut app)
            .iter()
            .any(|l| l.contains("v to reveal")),
        "editor open"
    );

    // Wheel and clicks anywhere (including a group row that would switch
    // the group) change nothing while the editor owns the screen.
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, 5, kr),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            5,
            kr,
        ),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        wheel(MouseEventKind::ScrollDown, kc, kr),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            kc,
            kr,
        ),
        WHEEL_AREA,
    );
    assert_eq!(config_state(&mut app).group_index(), 0, "groups untouched");
    assert!(
        !config_state(&mut app).dirty(),
        "the mouse never staged anything"
    );
    assert!(
        render_config_grid(&mut app)
            .iter()
            .any(|l| l.contains("v to reveal")),
        "editor stays open"
    );
}

#[test]
fn config_mode_mouse_never_touches_selection_or_copy() {
    let (mut app, _path) = config_app("cfgnocopy");
    let _ = render_config_grid(&mut app);
    let copied = std::cell::RefCell::new(Vec::<String>::new());
    {
        let sink = &copied;
        let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
        // A full press/drag/release over the (base-surface) chat pane:
        // config mode routes to WheelSurface::Config, which only ever
        // SELECTS rows/groups (no drag head, no release copy.
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                30,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                40,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                42,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
    }
    assert!(app.selection.is_none(), "no selection state in config mode");
    assert!(copied.borrow().is_empty(), "nothing copied in config mode");
    assert!(
        matches!(app.mode, Mode::ConfigEditor { .. }),
        "the editor stays open"
    );
}

// ---- base-surface clicks: sidebar thread select + click-to-focus --------

use chibi_tui::app::Focus;
use chibi_tui::popup::ErrorPopup;

/// Render one frame of the base surface and return the buffer as a grid
/// of lines, one per terminal row. Same dynamic locate-panes technique as
/// the config-screen tests: positions are found in the rendered buffer
/// because the dev banner (`CHIBI_ENV=dev` leaking between parallel
/// tests) shifts every pane down by one row.
fn render_base_grid(app: &mut chibi_tui::app::App) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(WHEEL_AREA.width, WHEEL_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| chibi_tui::ui::draw(f, app, &chibi_tui::theme::Theme::tokyo_night()))
        .unwrap();
    let cells: Vec<String> = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    cells
        .chunks(WHEEL_AREA.width as usize)
        .map(|row| row.concat())
        .collect()
}

/// Clicking a thread row in the sidebar switches to that thread live
/// (unread cleared, scroll reset to follow-bottom, text selection
/// cleared, the exact `select_chat` side effects the arrow keys have)
/// and moves focus to the sidebar, without the sidebar holding focus
/// beforehand.
#[test]
fn sidebar_click_selects_the_thread_and_focuses_the_sidebar() {
    let mut app = app_with_chats(3);
    app.chats[2].unread = true;
    app.scroll_up(6);
    let grid = render_base_grid(&mut app);
    let y = grid_row(&grid, "chat-2");
    let x = grid[y].find("chat-2").unwrap() as u16;

    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            x,
            y as u16,
        ),
        WHEEL_AREA,
    );

    assert_eq!(app.active, 2, "the click switched to the clicked thread");
    assert_eq!(app.focus, Focus::Sidebar, "the click focused the sidebar");
    assert!(
        !app.chats[2].unread,
        "select_chat cleared the unread marker"
    );
    assert_eq!(
        app.scroll, 0,
        "live switching reset the scroll to follow-bottom"
    );
    assert!(
        app.selection.is_none(),
        "no text selection from a sidebar click"
    );
}

/// A click below the last thread (still inside the list block) moves
/// focus to the sidebar but changes no thread selection.
#[test]
fn sidebar_click_below_the_last_thread_only_moves_focus() {
    let mut app = app_with_chats(3);
    let grid = render_base_grid(&mut app);
    let last = grid_row(&grid, "chat-2") as u16;

    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            last + 5,
        ),
        WHEEL_AREA,
    );

    assert_eq!(app.active, 0, "below the last thread: no selection change");
    assert_eq!(app.focus, Focus::Sidebar, "focus still moves");
    assert!(app.selection.is_none());
}

/// Clicks on the list block's border rows or side columns are not list
/// rows: nothing changes at all: no selection, no focus move.
#[test]
fn sidebar_click_on_a_border_or_divider_changes_nothing() {
    let mut app = app_with_chats(3);
    let grid = render_base_grid(&mut app);
    // The ` Chats ` title rides the block's TOP border row; the item row
    // gives us a row to hit the LEFT border column on.
    let top_border = grid_row(&grid, "Chats") as u16;
    let item_row = grid_row(&grid, "chat-1") as u16;

    // Top border row of the block (past the title text).
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            top_border,
        ),
        WHEEL_AREA,
    );
    // Left border column on a thread row.
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            0,
            item_row,
        ),
        WHEEL_AREA,
    );

    assert_eq!(app.active, 0, "border clicks never switch threads");
    assert_eq!(app.focus, Focus::Chat, "border clicks never move focus");
    assert!(app.selection.is_none());
}

/// With more threads than fit the sidebar, ratatui scrolls the list
/// internally; the click must map through the render-fed offset, so the
/// row visually above the selected thread is that thread's neighbour;
/// without the seam offset this click would hit a wrong chat.
#[test]
fn sidebar_click_uses_the_render_fed_offset_when_the_list_scrolls() {
    let mut app = app_with_chats(40);
    app.active = 38; // force the list's internal scroll
    let grid = render_base_grid(&mut app);
    // The chat pane's title line (`#39/40 · chat-38`) also mentions the
    // thread name; the SIDEBAR row is the last occurrence.
    let y38 = grid
        .iter()
        .rposition(|line| line.contains("chat-38"))
        .expect("chat-38 row on screen") as u16;
    assert!(
        y38 >= 2,
        "precondition: found the sidebar row, not the title"
    );

    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            y38 - 1,
        ),
        WHEEL_AREA,
    );

    assert_eq!(
        app.active, 37,
        "the visually adjacent row maps to chat-37 via the seam offset"
    );
    assert_eq!(app.focus, Focus::Sidebar);
}

/// A press in the chat pane moves focus to the chat (click-to-focus) and
/// the drag-selection pipeline flows exactly as before.
#[test]
fn chat_pane_press_moves_focus_to_chat_and_selection_still_works() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::assistant("hello world from selection"));
    app.focus = Focus::Sidebar;
    render_for_geometry(&mut app);

    let copied = std::cell::RefCell::new(Vec::<String>::new());
    {
        let sink = &copied;
        let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                27,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        assert_eq!(app.focus, Focus::Chat, "the press focused the chat");
        assert!(
            app.selection.as_ref().is_some_and(|s| s.dragging),
            "the press still starts the live drag"
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                38,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                40,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
    }
    assert_eq!(
        copied.borrow().as_slice(),
        ["hello world"],
        "the release copied the selected plain text as before"
    );
}

/// A press on the input region (input editor block / status lines) moves
/// focus to the chat (typing goes to the prompt) and never starts a
/// selection.
#[test]
fn input_region_click_moves_focus_to_chat() {
    let mut app = app_with_chats(1);
    app.focus = Focus::Sidebar;
    render_for_geometry(&mut app);

    // Input editor block row (chrome rows 37..39 of the 120x40 frame).
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            60,
            38,
        ),
        WHEEL_AREA,
    );
    assert_eq!(
        app.focus,
        Focus::Chat,
        "clicking the input block focuses the chat"
    );
    assert!(app.selection.is_none());
}

/// Under the quit-confirm popup and in non-Normal modes, clicks change
/// nothing: no thread switch, no focus move: the same `selectable` gate
/// that swallows selection presses swallows the new click targets too.
#[test]
fn base_surface_clicks_are_swallowed_under_popups_and_non_normal_modes() {
    let mut app = app_with_chats(3);
    let grid = render_base_grid(&mut app);
    let y = grid_row(&grid, "chat-2") as u16;

    // Quit-confirm popup (a flag, not a Mode).
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm, "precondition: popup open");
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            y,
        ),
        WHEEL_AREA,
    );
    assert_eq!(app.active, 0, "sidebar click swallowed under the popup");
    assert_eq!(app.focus, Focus::Chat, "focus unchanged under the popup");
    assert!(app.quit_confirm, "the click must not disturb the popup");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.quit_confirm, "popup dismissed for the next phase");

    // Non-Normal mode: the delete-confirm popup owns the screen.
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, Mode::ConfirmDelete, "precondition: modal open");
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            y,
        ),
        WHEEL_AREA,
    );
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            60,
            10,
        ),
        WHEEL_AREA,
    );
    assert_eq!(app.active, 0, "no thread switch under the modal");
    assert_eq!(app.mode, Mode::ConfirmDelete, "the modal stays open");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    // Error popup up: clicks are swallowed here too.
    app.error_popup = Some(ErrorPopup::new("boom"));
    handle_mouse(
        &mut app,
        button(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            y,
        ),
        WHEEL_AREA,
    );
    assert_eq!(app.active, 0, "no thread switch under the error popup");
    assert_eq!(
        app.focus,
        Focus::Chat,
        "focus unchanged under the error popup"
    );
    assert!(
        app.error_popup.is_some(),
        "the click must not dismiss the popup"
    );
}
