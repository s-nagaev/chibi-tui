use super::support::*;
use crate::app::*;

/// Predicate contract: whitespace-only and pure-marker contents are
/// invisible; anything that also contains other text — even alongside
/// the marker — is a real answer.
#[test]
fn invisible_result_predicate_matches_blank_and_pure_ack_only() {
    for blank in ["", "   ", " \n\t "] {
        assert!(is_invisible_result(blank), "blank {blank:?} must absorb");
    }
    let doubled = format!("{ACK_MARKER}{ACK_MARKER}");
    let spaced = format!(" {ACK_MARKER}  {ACK_MARKER} ");
    for ack in [
        ACK_MARKER,
        "  <chibi>ACK</chibi>  ",
        "\n<chibi>ACK</chibi>\n",
        doubled.as_str(),
        spaced.as_str(),
    ] {
        assert!(is_invisible_result(ack), "pure ack {ack:?} must absorb");
    }
    for mixed in [
        "real answer",
        &format!("{ACK_MARKER} partial text"),
        &format!("text {ACK_MARKER}"),
        &format!("{ACK_MARKER}ish"), // marker must match whole segments only
    ] {
        assert!(!is_invisible_result(mixed), "mixed {mixed:?} must show");
    }
}

/// A blank result leaves NO assistant bubble: the pending placeholder is
/// dropped, the lifecycle resolves to Idle (spinner stops cleanly and
/// re-arms on the next prompt), and no error popup/toast fires.
#[test]
fn blank_result_is_absorbed_without_bubble() {
    for content in ["", "   \n\t "] {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "question");
        assert_eq!(app.chats[0].messages.len(), 2, "user + pending");

        finish_chat_with_content(&mut app, 0, content);

        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 1, "placeholder dropped, no bubble");
        assert_eq!(msgs[0].role, Role::User);
        assert!(msgs.iter().all(|m| !m.pending), "no stuck spinner row");
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        assert!(app.error_popup.is_none(), "blank result is not an error");
    }
}

/// A pure-ACK answer (exact marker, whitespace-wrapped, repeated) behaves
/// exactly like a blank one: absorbed invisibly, clean Idle, no error.
#[test]
fn pure_ack_result_is_absorbed_without_bubble() {
    let wrapped = format!("  {ACK_MARKER}\n");
    let doubled = format!("{ACK_MARKER}{ACK_MARKER}");
    let spaced = format!(" {ACK_MARKER}  {ACK_MARKER} ");
    for content in [
        ACK_MARKER,
        wrapped.as_str(),
        doubled.as_str(),
        spaced.as_str(),
    ] {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "question");
        finish_chat_with_content(&mut app, 0, content);

        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 1, "pure ACK {content:?} leaves no bubble");
        assert!(msgs.iter().all(|m| !m.pending));
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
        assert!(app.error_popup.is_none());
    }
}

/// Content that CONTAINS the marker but also real text is a real answer:
/// shown as-is, raw — the TUI does not clean up partial markers (that is
/// the backend's job).
#[test]
fn mixed_marker_content_is_shown_raw() {
    for content in [
        format!("before {ACK_MARKER} after"),
        format!("{ACK_MARKER} partial text"),
        format!("{ACK_MARKER}ish"),
    ] {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "question");
        finish_chat_with_content(&mut app, 0, &content);

        let msgs = &app.chats[0].messages;
        assert_eq!(msgs.len(), 2, "mixed content renders a bubble");
        assert!(!msgs[1].pending);
        assert_eq!(msgs[1].markdown, content, "shown raw, unmodified");
        assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    }
}

/// Queue interplay: absorbing an ACK result must still hand the FIFO to
/// the drain step — the next queued prompt sends (mirrors the event-loop
/// glue: terminal Result → QueueDrain → dequeue_next_for).
#[test]
fn absorbed_result_still_drains_queued_prompt() {
    let mut app = app_with_chats(1);
    submit_text(&mut app, "one");
    type_in(&mut app, "two");
    assert!(app.take_input().is_none(), "busy chat enqueues");

    finish_chat_with_content(&mut app, 0, ACK_MARKER);

    assert_eq!(app.active_lifecycle(), &ChatLifecycle::Idle);
    assert_eq!(app.active_queue_len(), 1);

    // The drain step owned by the event loop after QueueDrain.
    let thread = app.chats[0].id.clone();
    let next = app
        .dequeue_next_for(&thread)
        .expect("queued prompt must send after ACK absorb");
    assert_eq!(next.prompt, "two");
    assert!(matches!(
        app.chats[0].lifecycle,
        ChatLifecycle::Awaiting { .. }
    ));

    // Shape after the swap: [user(one), user(two), pending(live)] — the
    // absorbed round added no bubble and the queued marker became the
    // live placeholder.
    let msgs = &app.chats[0].messages;
    assert_eq!(msgs.len(), 3);
    assert!(msgs[2].pending && !is_queued_marker(&msgs[2]));
}

/// A background chat's absorbed result stays invisible there too and
/// must not disturb the ACTIVE chat's state or view.
#[test]
fn absorbed_result_in_background_chat_is_invisible() {
    let mut app = app_with_chats(2);
    app.select_chat(idx(&app, "chat-1")); // the "background" one
    submit_text(&mut app, "background question");
    app.select_chat(idx(&app, "chat-0")); // foreground stays empty

    {
        let i = idx(&app, "chat-1");
        finish_chat_with_content(&mut app, i, ACK_MARKER);
    }

    assert_eq!(
        app.chats[idx(&app, "chat-1")].messages.len(),
        1,
        "no bubble in bg chat"
    );
    assert!(app.chats[idx(&app, "chat-1")]
        .messages
        .iter()
        .all(|m| !m.pending));
    assert_eq!(
        app.chats[idx(&app, "chat-1")].lifecycle,
        ChatLifecycle::Idle
    );
    assert!(
        app.chats[idx(&app, "chat-0")].messages.is_empty(),
        "foreground untouched"
    );
    assert!(app.error_popup.is_none());
}

// state --------------------------------
