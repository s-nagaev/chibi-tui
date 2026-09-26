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
