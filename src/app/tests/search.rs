use super::support::*;
use crate::app::*;

#[test]
fn ctrl_f_opens_search_popup_with_empty_query() {
    let mut app = app_with_chats(2);
    assert_eq!(app.mode, Mode::Normal);
    app.begin_search();
    assert!(matches!(app.mode, Mode::Searching { .. }));
    assert_eq!(app.search_query(), Some(""));
    assert!(app.search_matches().is_empty());
    assert_eq!(app.search_selected(), 0);
    // Messages untouched by merely opening the popup.
    assert!(app.chats[0].messages.is_empty());
}

#[test]
fn begin_search_noop_without_chats_or_in_other_modes() {
    // No active chat.
    let mut app = App::new(Vec::new());
    app.begin_search();
    assert_eq!(app.mode, Mode::Normal);

    // Rename session open.
    let mut app = app_with_chats(1);
    press_ctrl_r(&mut app);
    app.begin_search();
    assert!(matches!(app.mode, Mode::Renaming { .. }));

    // Delete-confirm popup open.
    let mut app = app_with_chats(1);
    app.begin_delete_confirm();
    app.begin_search();
    assert_eq!(app.mode, Mode::ConfirmDelete);

    // Already searching — re-press must not reset the query.
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::user("needle here"));
    app.begin_search();
    app.search_push('n');
    app.begin_search();
    assert_eq!(app.search_query(), Some("n"), "re-press must not clobber");
}

#[test]
fn search_typing_recomputes_matches_live_case_insensitive() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::assistant("The Quick Brown Fox"));
    app.chats[0].messages.push(Message::user("quick check"));
    app.begin_search();

    app.search_push('q');
    assert_eq!(app.search_query(), Some("q"));
    assert_eq!(
        app.search_matches().len(),
        2,
        "case-insensitive: q in Quick/quick"
    );

    app.search_push('u');
    assert_eq!(app.search_matches().len(), 2);

    app.search_backspace();
    app.search_backspace();
    assert_eq!(app.search_query(), Some(""));
    assert!(
        app.search_matches().is_empty(),
        "empty query ⇒ no match list (graceful empty state)"
    );
}

#[test]
fn search_matches_ignore_role_headers_and_markup_noise() {
    let mut app = app_with_chats(1);
    // `**bold**` renders to `bold` — the asterisks are style markup and
    // must never be matchable; the role header line ("● Chibi") is UI
    // chrome drawn by render_chat, not part of message text.
    app.chats[0]
        .messages
        .push(Message::assistant("use **bold** sparingly"));
    app.begin_search();

    for ch in "**".chars() {
        app.search_push(ch);
    }
    assert!(
        app.search_matches().is_empty(),
        "style markup must not match"
    );

    for _ in 0..2 {
        app.search_backspace();
    }
    for ch in "bold".chars() {
        app.search_push(ch);
    }
    assert_eq!(
        app.search_matches().len(),
        1,
        "rendered text (markup stripped) must match"
    );
    let m = &app.search_matches()[0];
    assert_eq!(m.message_index, 0);
    assert_eq!(m.line_index, 0);
    assert!(
        m.line_text.contains("bold") && !m.line_text.contains('*'),
        "snippet source must be markup-free: {:?}",
        m.line_text
    );
}

#[test]
fn search_selection_navigation_clamps() {
    let mut app = app_with_chats(1);
    for i in 0..5 {
        app.chats[0]
            .messages
            .push(Message::user(format!("hit {i} needle")));
    }
    app.begin_search();
    for ch in "needle".chars() {
        app.search_push(ch);
    }
    assert_eq!(app.search_matches().len(), 5);
    assert_eq!(app.search_selected(), 0);

    for _ in 0..10 {
        app.search_select_next();
    }
    assert_eq!(app.search_selected(), 4, "next clamps at the last match");

    for _ in 0..10 {
        app.search_select_prev();
    }
    assert_eq!(app.search_selected(), 0, "prev clamps at the first");
}

#[test]
fn search_enter_jumps_to_selected_and_closes_popup() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::assistant("line one needle"));
    app.chats[0]
        .messages
        .push(Message::user("line two needle again"));
    app.begin_search();
    for ch in "needle".chars() {
        app.search_push(ch);
    }
    assert_eq!(app.search_matches().len(), 2);

    app.search_select_next();
    assert_eq!(app.search_selected(), 1);

    assert!(app.jump_to_selected());
    assert_eq!(app.mode, Mode::Normal, "Enter closes the popup");
    assert_eq!(
        app.pending_search_jump,
        Some((1, 0, 9)),
        "pending jump targets message 1, rendered line 0, char col 9"
    );
    assert!(app.pending_search_jump.is_some());
}

#[test]
fn search_enter_without_matches_is_noop() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::user("nothing to see"));
    app.begin_search();
    // Empty query.
    assert!(!app.jump_to_selected());
    assert!(
        matches!(app.mode, Mode::Searching { .. }),
        "popup stays open on an empty-query Enter"
    );
    assert!(app.pending_search_jump.is_none());
    // Non-matching query.
    app.search_push('z');
    assert!(!app.jump_to_selected());
    assert!(app.pending_search_jump.is_none());
    // Esc still closes cleanly afterwards.
    assert!(app.cancel_search());
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn search_esc_cancels_keeps_view_and_messages() {
    let mut app = app_with_chats(1);
    app.chats[0].messages.push(Message::user("needle"));
    app.scroll_up(15); // detach from bottom
    let scroll_before = app.scroll;
    let msgs_before: Vec<String> = app.chats[0]
        .messages
        .iter()
        .map(|m| m.markdown.clone())
        .collect();

    app.begin_search();
    app.search_push('n');
    assert!(app.cancel_search());
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.scroll, scroll_before, "view unchanged by Esc");
    assert!(
        app.pending_search_jump.is_none(),
        "Esc must never request a jump"
    );
    let msgs_after: Vec<String> = app.chats[0]
        .messages
        .iter()
        .map(|m| m.markdown.clone())
        .collect();
    assert_eq!(msgs_before, msgs_after, "messages untouched");
    assert!(!app.cancel_search(), "cancelling again is a no-op");
}

/// The search cycle — open, type, navigate, jump, close — must leave
/// every message byte-for-byte identical (search is strictly read-only).
#[test]
fn search_cycle_is_read_only_on_messages() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::user("first question with needle"));
    app.chats[0]
        .messages
        .push(Message::assistant("answer with needle too"));
    let before = serde_json::to_string(&app.chats[0].messages).unwrap();

    app.begin_search();
    for ch in "needle".chars() {
        app.search_push(ch);
    }
    app.search_select_next();
    app.jump_to_selected();
    // Reopen and cancel (second cycle, close path).
    app.begin_search();
    app.search_push('n');
    app.cancel_search();

    let after = serde_json::to_string(&app.chats[0].messages).unwrap();
    assert_eq!(before, after, "message bytes must be unchanged");
}

/// Search works while the ACTIVE chat is busy (read-only by design) —
/// the popup opens, matches are found, and the lifecycle stays untouched.
#[test]
fn search_opens_and_finds_while_chat_is_busy() {
    let mut app = app_with_chats(1);
    app.chats[0]
        .messages
        .push(Message::user("busy needle question"));
    let submitted = submit_text(&mut app, "in flight");
    assert!(app.is_busy());

    app.begin_search();
    assert!(matches!(app.mode, Mode::Searching { .. }));
    for ch in "needle".chars() {
        app.search_push(ch);
    }
    assert_eq!(app.search_matches().len(), 1);
    assert!(app.jump_to_selected());
    assert_eq!(
        app.chats[0].lifecycle.request_id(),
        Some(submitted.request_id.as_str()),
        "lifecycle untouched by search"
    );
}

/// `collect_search_matches` takes an ITERATOR over messages — the task-6
/// hook: an all-threads search chains every chat's messages through the
/// same function additively.
#[test]
fn collect_search_matches_chains_any_iterator_scope() {
    let chat_a = {
        let mut c = Chat::new("a");
        c.messages.push(Message::user("needle in chat a"));
        c
    };
    let chat_b = {
        let mut c = Chat::new("b");
        c.messages.push(Message::assistant("unrelated"));
        c.messages.push(Message::user("needle in chat b too"));
        c
    };
    let hits = collect_search_matches(
        chat_a.messages.iter().chain(chat_b.messages.iter()),
        "needle",
    );
    assert_eq!(hits.len(), 2);
    assert_eq!((hits[0].message_index, hits[1].message_index), (0, 2));
}

/// Byte-slicing on a lowercased copy would panic on multi-byte glyphs
/// whose lowercase form expands (`İ` → `i̇`); the char-window matcher
/// must be byte-agnostic and still find ASCII hits around wide chars.
#[test]
fn collect_search_matches_is_unicode_safe_and_finds_ascii() {
    let mut chat = Chat::new("wide");
    chat.messages
        .push(Message::assistant("İstanbul needle İzmir"));
    let hits = collect_search_matches(chat.messages.iter(), "needle");
    assert_eq!(hits.len(), 1, "ASCII hit next to multi-byte glyphs found");
    assert_eq!(hits[0].col, 9, "char offset into the original line");
    // A hit on the wide-glyph word itself is found without panicking.
    let hits = collect_search_matches(chat.messages.iter(), "zmir");
    assert_eq!(hits.len(), 1, "trailing ASCII of a wide-prefixed word");
}

/// Multiple occurrences on one rendered line yield one match each.
#[test]
fn collect_search_matches_reports_every_occurrence() {
    let mut chat = Chat::new("multi");
    chat.messages
        .push(Message::assistant("x needle y needle z"));
    let hits = collect_search_matches(chat.messages.iter(), "needle");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].col, 2);
    assert_eq!(hits[1].col, 11);
    assert_eq!(hits[0].line_text, "x needle y needle z");
}
