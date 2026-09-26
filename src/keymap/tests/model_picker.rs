use crate::tests::support::*;

use crossterm::event::{KeyCode, KeyModifiers};

// ^M chord + modal isolation -----------------

/// Park the injected picker's selection on `selected` (0-based) without
/// touching the rows (paging tests need non-zero start offsets).
fn park_picker_selection(app: &mut chibi_tui::app::App, selected: usize) {
    if let chibi_tui::app::Mode::ModelPicking { state } = &mut app.mode {
        state.selected = selected;
    }
}

#[test]
fn ctrl_m_opens_the_model_picker_in_normal_mode() {
    let mut app = app_with_chats(1);
    assert!(app.mode.is_normal());
    press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ModelPicking { .. }
    ));
    // The event loop drains the staged hidden fetch on the same tick.
    let bundle = app.take_picker_submission().expect("fetch staged");
    assert_eq!(bundle.prompt, "/model");
    // Esc hands the keyboard back.
    press(&mut app, KeyCode::Esc, KeyModifiers::empty());
    assert!(app.mode.is_normal());
}

#[test]
fn ctrl_m_works_from_sidebar_focus_and_returns_focus_on_close() {
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
    press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ModelPicking { .. }
    ));
    press(&mut app, KeyCode::Esc, KeyModifiers::empty());
    assert!(app.mode.is_normal());
    assert_eq!(
        app.focus,
        chibi_tui::app::Focus::Chat,
        "modal closed → editor"
    );
}

#[test]
fn the_picker_modal_swallows_everything_but_nav_confirm_cancel() {
    let mut app = app_with_chats(1);
    for ch in "precious draft".chars() {
        app.input.input(chibi_tui::input::Input {
            key: chibi_tui::input::Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        });
    }
    inject_ready_picker(&mut app, 3);

    // Typing leaks nowhere; global chords and thread switching are
    // swallowed; the mode never moves. PgUp is a picker nav key now:
    // it pages the LIST, never the chat pane.
    press(&mut app, KeyCode::Char('x'), KeyModifiers::empty());
    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
    press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ModelPicking { .. }
    ));
    assert_eq!(app.chats.len(), 1, "no new chat from swallowed ^N");
    assert!(app.error_popup.is_none());
    assert!(
        app.status_strip_visible,
        "swallowed ^O must not toggle the strip"
    );
    assert_eq!(
        app.input.lines().join(""),
        "precious draft",
        "no keystroke leaked into the textarea"
    );
    assert_eq!(
        app.scroll, 0,
        "picker PgUp pages the list, never the chat pane"
    );

    // The ONLY working keys: ↑/↓ and PgUp/PgDn navigate, Enter confirms,
    // Esc cancels, Ctrl+C opens the quit confirmation.
    press(&mut app, KeyCode::Down, KeyModifiers::empty());
    press(&mut app, KeyCode::Down, KeyModifiers::empty());
    press(&mut app, KeyCode::Up, KeyModifiers::empty());
    assert_eq!(app.model_picker_selected(), 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::empty());
    assert!(app.mode.is_normal(), "Enter confirmed and closed the popup");
    let bundle = app.take_picker_submission().expect("selection staged");
    assert_eq!(bundle.prompt, "/model 2");
}

#[test]
fn the_picker_enters_confirm_never_touch_the_message_draft() {
    let mut app = app_with_chats(1);
    for ch in "half typed prompt".chars() {
        app.input.input(chibi_tui::input::Input {
            key: chibi_tui::input::Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        });
    }
    inject_ready_picker(&mut app, 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::empty());
    assert!(
        app.take_picker_submission().is_some(),
        "the picker consumed the Enter"
    );
    assert_eq!(
        app.input.lines().join(""),
        "half typed prompt",
        "the draft must not be submitted or altered"
    );
    assert!(app.chats[0].messages.is_empty());
}

#[test]
fn the_picker_enter_is_a_noop_while_the_listing_is_loading() {
    let mut app = app_with_chats(1);
    app.begin_model_picker();
    // The event loop would drain the staged fetch on the same tick.
    assert!(app.take_picker_submission().is_some(), "precondition");
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ModelPicking { .. }
    ));
    press(&mut app, KeyCode::Enter, KeyModifiers::empty());
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::ModelPicking { .. }),
        "nothing to confirm while Loading — the popup stays open"
    );
    assert!(app.take_picker_submission().is_none());
}

#[test]
fn the_picker_esc_closes_without_staging_anything() {
    let mut app = app_with_chats(1);
    inject_ready_picker(&mut app, 2);
    press(&mut app, KeyCode::Esc, KeyModifiers::empty());
    assert!(app.mode.is_normal());
    assert!(app.take_picker_submission().is_none());
    assert!(!app.should_quit);
}

#[test]
fn the_picker_ctrl_c_opens_the_quit_confirmation_like_the_other_popups() {
    let mut app = app_with_chats(1);
    inject_ready_picker(&mut app, 2);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_confirm);
    assert!(!app.should_quit, "the confirm itself must not quit");
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(app.should_quit);
}

#[test]
fn picker_page_down_jumps_a_full_page_from_the_top() {
    let mut app = app_with_chats(1);
    inject_ready_picker(&mut app, 104);
    app.picker_visible_rows = 12;
    press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
    assert_eq!(
        app.model_picker_selected(),
        12,
        "one page of visible rows from the top"
    );
    press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
    assert_eq!(app.model_picker_selected(), 24, "each press steps one page");
    press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
    assert_eq!(
        app.model_picker_selected(),
        12,
        "page up steps back one page"
    );
}

#[test]
fn picker_page_navigation_steps_one_page_from_the_middle() {
    let mut app = app_with_chats(1);
    inject_ready_picker(&mut app, 104);
    app.picker_visible_rows = 12;
    for _ in 0..5 {
        press(&mut app, KeyCode::Down, KeyModifiers::empty());
    }
    assert_eq!(app.model_picker_selected(), 5, "mid-page starting offset");
    press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
    assert_eq!(
        app.model_picker_selected(),
        17,
        "page down from mid-page steps a full page"
    );
    press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
    assert_eq!(
        app.model_picker_selected(),
        5,
        "page up returns to the row a page above"
    );
}

#[test]
fn picker_page_down_clamps_at_the_bottom_edge() {
    let mut app = app_with_chats(1);
    inject_ready_picker(&mut app, 20);
    app.picker_visible_rows = 12;
    park_picker_selection(&mut app, 15);
    press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
    assert_eq!(
        app.model_picker_selected(),
        19,
        "clamped to the last row, no wraparound"
    );
    press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
    assert_eq!(
        app.model_picker_selected(),
        19,
        "paging past the edge stays clamped"
    );
    press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
    assert_eq!(app.model_picker_selected(), 7);
    for _ in 0..2 {
        press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
    }
    assert_eq!(
        app.model_picker_selected(),
        0,
        "page up clamps at the top edge too"
    );
}

#[test]
fn picker_paging_applies_to_the_filtered_entries_list() {
    let mut app = app_with_chats(1);
    inject_ready_picker(&mut app, 104);
    // The picker has no query input today; `entries` IS the navigable
    // list (a future filter would prune it the same way). Paging must
    // measure against this list, never a hardcoded 104-row listing.
    if let chibi_tui::app::Mode::ModelPicking { state } = &mut app.mode {
        state.entries.truncate(15);
    }
    app.picker_visible_rows = 12;
    press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
    assert_eq!(app.model_picker_selected(), 12);
    press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
    assert_eq!(
        app.model_picker_selected(),
        14,
        "clamped to the filtered list's last row"
    );
}

#[test]
fn ctrl_m_is_swallowed_while_other_modals_own_the_keyboard() {
    // ^G log viewer open: ^M must NOT open the picker on top.
    let mut app = app_with_chats(1);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
    press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert!(
        matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }),
        "the viewer keeps the keyboard"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::empty());

    // ^D confirm popup open: ^M swallowed as well.
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
    press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
    press(&mut app, KeyCode::Esc, KeyModifiers::empty());
    assert!(app.mode.is_normal());
    assert!(
        app.take_picker_submission().is_none(),
        "no picker fetch ever staged through the modals"
    );
}
