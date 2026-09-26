use super::support::*;
use crate::app::*;

/// THE round-trip criterion: Ctrl+T's state flip is a pure pane switch —
/// toggling twice lands back on Chat; nothing about the chat list
/// (selection, scroll, count) moves along.
#[test]
fn toggle_focus_round_trips_chat_and_sidebar() {
    let mut app = app_with_chats(3);
    app.scroll = 42;
    assert_eq!(app.focus, Focus::Chat, "default focus");

    app.toggle_focus();
    assert_eq!(app.focus, Focus::Sidebar);

    app.toggle_focus();
    assert_eq!(app.focus, Focus::Chat, "toggle twice round-trips");
    assert_eq!(app.active, 0, "selection untouched by focus flips");
    assert_eq!(app.scroll, 42, "scroll untouched by focus flips");
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.chats.len(), 3);
}

/// Focus is pane-level state, independent of the list contents: zero and
/// single-chat lists toggle too (no navigation semantics involved — no
/// toast, popup or quit ever fires).
#[test]
fn toggle_focus_is_independent_of_list_size() {
    for n in [0usize, 1usize] {
        let mut app = App::new((0..n).map(|i| Chat::new(format!("chat-{i}"))).collect());
        app.toggle_focus();
        assert_eq!(app.focus, Focus::Sidebar, "n={n}");
        app.toggle_focus();
        assert_eq!(app.focus, Focus::Chat, "n={n}");
        assert!(!app.should_quit);
        assert!(app.status_message.is_none());
        assert!(app.error_popup.is_none());
    }
}

/// Arrows keep CLAMPING after the Ctrl+T rewrite — focus toggling never
/// leaks into select_next/select_prev (regression pinned when the old
/// wrap-cycling method was removed).
#[test]
fn toggle_focus_does_not_change_arrow_clamping() {
    let mut app = app_with_chats(3);
    app.active = 2;
    app.select_next();
    assert_eq!(app.active, 2, "select_next clamps at the last thread");
    app.select_prev();
    app.select_prev();
    assert_eq!(app.active, 0, "select_prev clamps at the first thread");
    app.select_prev();
    assert_eq!(app.active, 0, "select_prev stays clamped at the top");

    // And the toggle itself still works from anywhere on the list.
    app.toggle_focus();
    assert_eq!(app.focus, Focus::Sidebar);
    app.active = 2;
    app.select_next();
    assert_eq!(app.active, 2, "clamping holds while Sidebar focused");
}

/// Modal closers reset focus to Chat: opening a modal above a Sidebar-
/// focused UI and closing it hands the keyboard back to the editor,
/// whichever close path was taken.
#[test]
fn closing_rename_resets_focus_to_chat_on_commit_and_cancel() {
    // Commit path.
    let mut app = app_with_chats(1);
    app.focus = Focus::Sidebar;
    press_ctrl_r(&mut app);
    assert!(matches!(app.mode, Mode::Renaming { .. }));
    app.rename_push('x');
    assert!(app.commit_rename());
    assert_eq!(app.focus, Focus::Chat);
    assert!(app.mode.is_normal());

    // Cancel path.
    let mut app = app_with_chats(1);
    app.focus = Focus::Sidebar;
    press_ctrl_r(&mut app);
    assert!(matches!(app.mode, Mode::Renaming { .. }));
    assert!(app.cancel_rename());
    assert_eq!(app.focus, Focus::Chat);
}

#[test]
fn closing_search_popups_resets_focus_to_chat() {
    // In-thread search: cancel and jump paths.
    let mut app = app_with_chats(1);
    submit_text(&mut app, "hello world");
    app.focus = Focus::Sidebar;
    press_ctrl_r_cancel_search_all_paths_helper(&mut app);
    assert_eq!(app.focus, Focus::Chat);

    // Global search: cancel path.
    let mut app = app_with_chats(1);
    app.focus = Focus::Sidebar;
    app.begin_search_all();
    assert!(matches!(app.mode, Mode::SearchingAll { .. }));
    assert!(app.cancel_search_all());
    assert_eq!(app.focus, Focus::Chat);

    // Global search: jump path (activate thread + close).
    let mut app = app_with_chats(2);
    submit_text(&mut app, "target needle");
    finish_chat(&mut app, 0);
    app.focus = Focus::Sidebar;
    app.begin_search_all();
    app.search_all_push('n');
    app.search_all_push('e');
    assert!(
        !app.search_all_matches().is_empty(),
        "precondition: a match exists"
    );
    assert!(app.jump_to_selected_all());
    assert_eq!(app.focus, Focus::Chat);
}

/// Helper shared by `closing_search_popups_resets_focus_to_chat`: walks
/// the in-thread search cancel + jump paths from a Sidebar-focused start.
fn press_ctrl_r_cancel_search_all_paths_helper(app: &mut App) {
    app.begin_search();
    assert!(matches!(app.mode, Mode::Searching { .. }));
    for ch in "hello".chars() {
        app.search_push(ch);
    }
    assert!(!app.search_matches().is_empty(), "precondition: matches");
    // Jump path…
    assert!(app.jump_to_selected());
    assert_eq!(app.focus, Focus::Chat);
    // …then reopen for the cancel path.
    app.focus = Focus::Sidebar;
    app.begin_search();
    assert!(matches!(app.mode, Mode::Searching { .. }));
    assert!(app.cancel_search());
    assert_eq!(app.focus, Focus::Chat);
}

#[test]
fn closing_delete_confirm_resets_focus_to_chat() {
    // Cancel path.
    let mut app = app_with_chats(2);
    app.focus = Focus::Sidebar;
    app.begin_delete_confirm();
    assert_eq!(app.mode, Mode::ConfirmDelete);
    assert!(app.cancel_delete());
    assert_eq!(app.focus, Focus::Chat);

    // Confirm path (delete leaves ≥1 chat behind).
    let mut app = app_with_chats(2);
    app.focus = Focus::Sidebar;
    app.begin_delete_confirm();
    assert_eq!(app.mode, Mode::ConfirmDelete);
    assert!(app.confirm_delete().is_some());
    assert_eq!(app.focus, Focus::Chat);

    // Confirm path into the clean EMPTY state (deleted last chat) —
    // focus still returns to the editor even with no chats left.
    let mut app = app_with_chats(1);
    app.focus = Focus::Sidebar;
    app.begin_delete_confirm();
    assert!(app.confirm_delete().is_some());
    assert!(app.chats.is_empty());
    assert_eq!(app.focus, Focus::Chat);
}

#[test]
fn dismissing_error_popup_resets_focus_to_chat() {
    let mut app = app_with_chats(1);
    app.focus = Focus::Sidebar;
    app.show_error("boom");
    assert!(app.error_popup.is_some());
    app.dismiss_error();
    assert!(app.error_popup.is_none());
    assert_eq!(app.focus, Focus::Chat);
}

/// ^N new-chat is editor-bound: creating a thread
/// always lands focus back on Chat, whether the chord came from either
/// pane.
#[test]
fn new_chat_lands_focus_on_chat() {
    let mut app = app_with_chats(2);
    app.focus = Focus::Sidebar;
    app.new_chat();
    assert_eq!(app.focus, Focus::Chat);
    assert_eq!(app.active, 0, "new chat is selected (and on top)");
}

// ---- lifecycle (per chat) --------------------------------------------
