use super::support::*;
use crate::app::*;

#[test]
fn ctrl_shift_f_opens_global_search_popup_with_empty_query() {
    let mut app = app_with_chats(2);
    assert_eq!(app.mode, Mode::Normal);
    app.begin_search_all();
    assert!(matches!(app.mode, Mode::SearchingAll { .. }));
    assert_eq!(app.search_all_query(), Some(""));
    assert!(app.search_all_matches().is_empty());
    assert_eq!(app.search_all_selected(), 0);
    // Messages untouched by merely opening the popup.
    assert!(app.chats[0].messages.is_empty());
    assert!(app.chats[1].messages.is_empty());
    assert!(app.pending_global_search_jump.is_none());
}

/// One modal at a time: global search never opens over a rename session,
/// the delete-confirm popup, or an already-open search (in-thread or
/// global — re-press must not clobber the query).
#[test]
fn begin_search_all_noop_with_open_modal() {
    // Rename session open.
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);
    app.begin_search_all();
    assert!(matches!(app.mode, Mode::Renaming { .. }));

    // Delete-confirm popup open.
    let mut app = app_with_chats(1);
    app.begin_delete_confirm();
    app.begin_search_all();
    assert_eq!(app.mode, Mode::ConfirmDelete);

    // In-thread search open.
    let mut app = app_with_chats(1);
    app.begin_search();
    app.begin_search_all();
    assert!(matches!(app.mode, Mode::Searching { .. }));

    // Already searching globally — re-press must not reset the query.
    let mut app = app_with_chats(1);
    app.begin_search_all();
    app.search_all_push('n');
    app.begin_search_all();
    assert_eq!(
        app.search_all_query(),
        Some("n"),
        "re-press must not clobber"
    );
}

/// The all-threads match list is ordered by CHAT ORDER then MESSAGE
/// ORDER within each chat, labeled with each thread's title, and
/// recomputed live (case-insensitive) on every keystroke. Empty query →
/// graceful empty list.
#[test]
fn global_search_typing_recomputes_across_all_chats_in_order() {
    let mut app = app_with_chats(3);
    app.chats[0]
        .messages
        .push(Message::assistant("needle in chat zero"));
    app.chats[1].messages.push(Message::user("unrelated"));
    app.chats[1]
        .messages
        .push(Message::user("needle first in chat one"));
    app.chats[1]
        .messages
        .push(Message::user("needle second in chat one"));
    app.chats[2]
        .messages
        .push(Message::assistant("needle in chat two"));
    app.begin_search_all();

    for ch in "needle".chars() {
        app.search_all_push(ch);
    }
    assert_eq!(app.search_all_query(), Some("needle"));
    let matches = app.search_all_matches();
    assert_eq!(matches.len(), 4);
    // Order = chat order, then message order within each chat.
    assert_eq!(
        matches.iter().map(|m| m.chat_index).collect::<Vec<_>>(),
        vec![0, 1, 1, 2]
    );
    assert_eq!(matches[0].chat_title, "chat-0");
    assert_eq!(matches[1].chat_title, "chat-1");
    assert_eq!((matches[1].message_index, matches[2].message_index), (1, 2));
    assert_eq!(matches[3].chat_index, 2, "single match in chat 2");

    // Empty query → graceful empty state.
    for _ in 0..6 {
        app.search_all_backspace();
    }
    assert_eq!(app.search_all_query(), Some(""));
    assert!(app.search_all_matches().is_empty());
}

#[test]
fn global_search_navigation_clamps() {
    let mut app = app_with_chats(2);
    for i in 0..3 {
        app.chats[0]
            .messages
            .push(Message::user(format!("hit {i} needle")));
    }
    for i in 0..2 {
        app.chats[1]
            .messages
            .push(Message::user(format!("hit {i} needle")));
    }
    app.begin_search_all();
    for ch in "needle".chars() {
        app.search_all_push(ch);
    }
    assert_eq!(app.search_all_matches().len(), 5);
    assert_eq!(app.search_all_selected(), 0);

    for _ in 0..10 {
        app.search_all_select_next();
    }
    assert_eq!(
        app.search_all_selected(),
        4,
        "next clamps at the last match"
    );

    for _ in 0..10 {
        app.search_all_select_prev();
    }
    assert_eq!(app.search_all_selected(), 0, "prev clamps at the first");
}

/// THE activation criterion (state layer): Enter activates the TARGET
/// thread (even a NON-active one) with Ctrl+↑/↓ semantics — index set +
/// scroll reset — and records the pending jump carrying that chat.
#[test]
fn global_search_enter_activates_target_thread_and_records_jump() {
    let mut app = app_with_chats(3);
    app.chats[0]
        .messages
        .push(Message::user("needle in chat zero"));
    app.chats[2]
        .messages
        .push(Message::assistant("needle deep in chat two"));
    app.active = 1; // searching from chat 1; hits live in chats 0 and 2
    app.begin_search_all();
    for ch in "needle".chars() {
        app.search_all_push(ch);
    }
    assert_eq!(app.search_all_matches()[0].chat_index, 0);
    // Select the SECOND match — the NON-active thread.
    app.search_all_select_next();
    assert_eq!(
        app.search_all_matches()[app.search_all_selected()].chat_index,
        2
    );

    assert!(app.jump_to_selected_all());
    assert_eq!(app.mode, Mode::Normal, "Enter closes the popup");
    assert_eq!(app.active, 2, "target thread activated");
    assert_eq!(
        app.pending_global_search_jump,
        Some((2, 0, 0, 0)),
        "pending jump carries chat 2, message 0, rendered line 0, col 0"
    );
    assert!(
        app.pending_search_jump.is_none(),
        "in-thread jump state untouched"
    );
    assert_eq!(app.scroll, 0, "thread switch resets scroll (follow-bottom)");
}

#[test]
fn global_search_enter_without_matches_is_noop() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::user("nothing to see"));
    app.begin_search_all();
    // Empty query.
    assert!(!app.jump_to_selected_all());
    assert!(
        matches!(app.mode, Mode::SearchingAll { .. }),
        "popup stays open on an empty-query Enter"
    );
    assert!(app.pending_global_search_jump.is_none());
    // Non-matching query.
    app.search_all_push('z');
    assert!(!app.jump_to_selected_all());
    assert!(app.pending_global_search_jump.is_none());
    assert_eq!(app.active, 0, "no thread switch without a match");
    // Esc still closes cleanly afterwards.
    assert!(app.cancel_search_all());
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn global_search_esc_cancels_keeps_threads_and_messages() {
    let mut app = app_with_chats(2);
    app.chats[1].messages.push(Message::user("needle"));
    app.scroll_up(15); // detach from bottom
    let scroll_before = app.scroll;
    let active_before = app.active;

    app.begin_search_all();
    app.search_all_push('n');
    assert!(app.cancel_search_all());
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.active, active_before, "no thread switch on Esc");
    assert_eq!(app.scroll, scroll_before, "view unchanged by Esc");
    assert!(
        app.pending_global_search_jump.is_none(),
        "Esc must never request a jump"
    );
    assert_eq!(app.chats.len(), 2, "chats untouched");
    assert_eq!(app.chats[1].messages.len(), 1, "messages untouched");
    assert!(!app.cancel_search_all(), "cancelling again is a no-op");
}

/// The global search cycle — open, type, navigate, jump, close — must
/// leave every chat's messages byte-for-byte identical (read-only).
#[test]
fn global_search_cycle_is_read_only_on_messages() {
    let mut app = app_with_chats(2);
    app.chats[0].messages.push(Message::user("needle in zero"));
    app.chats[1]
        .messages
        .push(Message::assistant("needle in one"));
    let before: Vec<String> = app
        .chats
        .iter()
        .map(|c| serde_json::to_string(&c.messages).unwrap())
        .collect();

    app.begin_search_all();
    for ch in "needle".chars() {
        app.search_all_push(ch);
    }
    app.search_all_select_next();
    app.jump_to_selected_all();
    // Reopen and cancel (second cycle, close path).
    app.begin_search_all();
    app.search_all_push('n');
    app.cancel_search_all();

    let after: Vec<String> = app
        .chats
        .iter()
        .map(|c| serde_json::to_string(&c.messages).unwrap())
        .collect();
    assert_eq!(before, after, "message bytes must be unchanged");
}

/// Global search works while ANY chat is busy (read-only by design) —
/// and its jump may freely switch to a busy background chat.
#[test]
fn global_search_works_while_chats_are_busy() {
    let mut app = app_with_chats(2);
    app.chats[1]
        .messages
        .push(Message::user("needle in background"));
    let submitted = submit_text(&mut app, "in flight"); // chat 0 busy
    assert!(app.is_busy());

    app.begin_search_all();
    for ch in "needle".chars() {
        app.search_all_push(ch);
    }
    assert_eq!(app.search_all_matches().len(), 1);
    assert_eq!(app.search_all_matches()[0].chat_index, 1);
    assert!(app.jump_to_selected_all());
    assert_eq!(app.active, 1, "busy background chat activated");
    assert_eq!(
        app.chats[0].lifecycle.request_id(),
        Some(submitted.request_id.as_str()),
        "lifecycle untouched by search"
    );
}

/// Zero chats: the popup opens and shows a graceful empty state; Enter
/// is a no-op and Esc closes cleanly.
#[test]
fn global_search_works_with_zero_chats_gracefully() {
    let mut app = App::new(Vec::new());
    app.begin_search_all();
    assert!(matches!(app.mode, Mode::SearchingAll { .. }));
    app.search_all_push('n');
    assert!(app.search_all_matches().is_empty());
    assert!(!app.jump_to_selected_all());
    assert!(app.cancel_search_all());
    assert_eq!(app.mode, Mode::Normal);
}

/// A single-thread dataset behaves exactly like the in-thread search —
/// no special casing needed, the global path just works.
#[test]
fn global_search_single_thread_dataset_works() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::assistant("sole needle here"));
    app.begin_search_all();
    for ch in "needle".chars() {
        app.search_all_push(ch);
    }
    assert_eq!(app.search_all_matches().len(), 1);
    let m = &app.search_all_matches()[0];
    assert_eq!((m.chat_index, m.chat_title.as_str()), (0, "chat-0"));
    assert!(app.jump_to_selected_all());
    assert_eq!(app.active, 0);
    assert_eq!(app.pending_global_search_jump, Some((0, 0, 0, 5)));
}

/// Rendering-wise the popup must stay consistent with its own state:
/// role headers and markdown markup never match (the per-chat
/// [`collect_search_matches`] guarantees carry over verbatim).
#[test]
fn global_search_matches_ignore_role_headers_and_markup_noise() {
    let mut app = app_with_chats(2);
    app.chats[1]
        .messages
        .push(Message::assistant("use **bold** sparingly"));
    app.begin_search_all();
    for ch in "**".chars() {
        app.search_all_push(ch);
    }
    assert!(app.search_all_matches().is_empty(), "markup must not match");
    for _ in 0..2 {
        app.search_all_backspace();
    }
    for ch in "bold".chars() {
        app.search_all_push(ch);
    }
    assert_eq!(app.search_all_matches().len(), 1);
    let m = &app.search_all_matches()[0];
    assert_eq!((m.chat_index, m.message_index, m.line_index), (1, 0, 0));
    assert!(
        m.line_text.contains("bold") && !m.line_text.contains('*'),
        "snippet source must be markup-free: {:?}",
        m.line_text
    );
}

// log viewer state ---------------------------
