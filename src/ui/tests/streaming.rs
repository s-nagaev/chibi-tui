use super::super::*;
use super::support::*;
use crate::model::ChatLifecycle;

// ---- streaming render (plan D6) ------------------------------------------

/// A chat mid-stream: user prompt + live pending row carrying partial text,
/// plus a queued marker for a prompt waiting in the FIFO.
fn streaming_app(partial: &str) -> App {
    let mut app = App::new(vec![Chat::new("s")]);
    let chat = &mut app.chats[0];
    chat.messages.push(Message::user("prompt"));
    chat.messages.push(Message::assistant_pending());
    chat.messages.push(Message::assistant_queued(1));
    chat.messages[1].markdown.push_str(partial);
    chat.lifecycle = ChatLifecycle::Awaiting {
        request_id: "req-1".into(),
    };
    chat.streaming = true;
    app
}

/// THE D6 contract: the actively-streaming row paints its partial text
/// as PLAIN text — raw markdown syntax must stay visible (no markdown
/// parse per delta frame), while the queued placeholder stays invisible.
#[test]
fn streaming_row_renders_plain_text_and_queued_marker_stays_hidden() {
    let mut app = streaming_app("**partial** and `code`");

    let rows = render_grid(&mut app);

    assert!(
        !rows.iter().any(|r| r.contains("queued (#1)")),
        "queued placeholders stay invisible (spinner's `queued…` label is not the marker)"
    );
    assert!(
        rows.iter().any(|r| r.contains("**partial**")),
        "markdown syntax must NOT be interpreted mid-stream"
    );
    assert!(
        rows.iter().any(|r| r.contains("`code`")),
        "inline code must paint raw while streaming"
    );
}

/// The terminal result restores the full markdown render: the partial
/// plain text is gone, the result content paints without its markers.
#[test]
fn terminal_result_restores_full_markdown_render() {
    let mut app = streaming_app("**partial**");
    let chat = &mut app.chats[0];
    let row = chat
        .messages
        .iter_mut()
        .rev()
        .find(|m| m.pending && !m.is_queued_marker())
        .unwrap();
    row.pending = false;
    row.markdown = "**done**".into();
    chat.streaming = false;
    chat.lifecycle = ChatLifecycle::Idle;

    let rows = render_grid(&mut app);

    assert!(
        rows.iter().any(|r| r.contains("done")),
        "result content renders"
    );
    assert!(
        !rows.iter().any(|r| r.contains("**")),
        "markdown must be interpreted again on the terminal frame"
    );
}

/// With the streaming flag off (no deltas accepted yet), the pending
/// placeholder renders empty exactly as before this feature — even when
/// its markdown holds text (pre-existing placeholder contract).
#[test]
fn non_streaming_pending_row_stays_an_empty_line() {
    let mut app = streaming_app("latent text");
    app.chats[0].streaming = false;

    let rows = render_grid(&mut app);

    assert!(
        !rows.iter().any(|r| r.contains("queued (#1)")),
        "queued placeholders stay invisible"
    );
    assert!(
        !rows.iter().any(|r| r.contains("latent text")),
        "a non-streaming pending row paints no content"
    );
}

/// The markdown render cache (plan D1) only serves FINALIZED rows. Even
/// when an identical finalized message already sits in the history — i.e.
/// the same content is cached from an earlier frame — the actively
/// streaming pending row must keep painting its raw partial text (no
/// markdown pass, no cache consult) while the finalized row is
/// markdown-interpreted as usual.
#[test]
fn streaming_row_stays_plain_beside_cached_history_content() {
    let mut app = App::new(vec![Chat::new("s")]);
    let chat = &mut app.chats[0];
    chat.messages.push(Message::user("prompt"));
    // A finalized historical message whose markdown equals the live
    // partial's prefix: markdown syntax must vanish from THIS row...
    chat.messages.push(Message::assistant("**shared** body"));
    // ...while the live pending row paints the very same markers raw.
    chat.messages.push(Message::assistant_pending());
    chat.messages[2].markdown.push_str("**shared** live");
    chat.lifecycle = ChatLifecycle::Awaiting {
        request_id: "req-1".into(),
    };
    chat.streaming = true;

    let rows = render_grid(&mut app);

    assert!(
        rows.iter().any(|r| r.contains("**shared** live")),
        "streaming row paints its partial raw, even next to cached content"
    );
    assert!(
        rows.iter().any(|r| r.contains("shared body")),
        "the finalized history row is markdown-interpreted (cache path)"
    );
}
