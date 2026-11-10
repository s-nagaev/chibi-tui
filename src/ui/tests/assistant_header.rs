use super::super::*;
use super::support::*;

// header rendering -------------------------

/// A message without model metadata renders the plain `● Chibi` header —
/// no parentheses, no "unknown" placeholder (old backend / historical
/// rows / fieldless frames).
#[test]
fn assistant_header_without_metadata_is_plain() {
    let theme = Theme::tokyo_night();
    let line = assistant_header_line(None, None, &theme);
    let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
    assert_eq!(text, "\u{25cf} Chibi", "fallback must stay plain: {text:?}");
    assert_eq!(line.spans.len(), 1);
}

/// A labelled message renders `● Chibi (model)`: the parenthetical is a
/// separate span styled with `theme.dim` (not bold, not the header blue).
#[test]
fn assistant_header_renders_dim_parenthetical_model_label() {
    let theme = Theme::tokyo_night();
    let line = assistant_header_line(Some("glm-5.2"), None, &theme);
    let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
    assert_eq!(text, "\u{25cf} Chibi (glm-5.2)");

    assert_eq!(line.spans.len(), 2);
    let header = &line.spans[0];
    assert_eq!(header.content, "\u{25cf} Chibi");
    assert_eq!(header.style.fg, Some(theme.blue), "header keeps its blue");
    assert!(header.style.add_modifier.contains(Modifier::BOLD));

    let paren = &line.spans[1];
    assert_eq!(paren.content, " (glm-5.2)");
    assert_eq!(paren.style.fg, Some(theme.dim), "parenthetical must be dim");
    assert!(
        !paren.style.add_modifier.contains(Modifier::BOLD),
        "parenthetical must not inherit the bold header"
    );
}

/// A stamped message renders a trailing DIM ` HH:MM` in local time after
/// the author name (and after the model label, when present): the
/// messenger convention. The instant is the row's own persisted stamp.
#[test]
fn assistant_header_renders_dim_timestamp_after_the_label() {
    let theme = Theme::tokyo_night();
    let line = assistant_header_line(Some("glm-5.2"), Some(1_700_000_000), &theme);
    let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
    assert!(
        text.starts_with("\u{25cf} Chibi (glm-5.2) "),
        "the label comes first, then the clock: {text:?}"
    );

    // The clock is the LAST span, dim, shaped like a wall clock.
    let clock = line.spans.last().expect("the timestamp span exists");
    assert_eq!(clock.style.fg, Some(theme.dim), "timestamp is dim");
    assert_eq!(
        clock.content.len(),
        7,
        "'  HH:MM' is seven bytes: {clock:?}"
    );
    let hhmm: String = clock.content.chars().skip(2).collect();
    assert!(
        hhmm[..2].parse::<u32>().is_ok_and(|h| h < 24)
            && &hhmm[2..3] == ":"
            && hhmm[3..].parse::<u32>().is_ok_and(|m| m < 60),
        "zero-padded local wall clock: {hhmm:?}"
    );
}

/// A stamp-less row (pre-timestamp snapshot) renders NO clock: no
/// placeholder, no invented time — the shared span helper stays `None`.
#[test]
fn timestamp_span_is_absent_without_a_stamp() {
    let theme = Theme::tokyo_night();
    assert_eq!(timestamp_span(None, &theme), None);
    let span = timestamp_span(Some(1_700_000_000), &theme).expect("stamped");
    assert_eq!(span.style.fg, Some(theme.dim), "the clock is dim");
    assert!(
        span.content.starts_with("  "),
        "two-space separation from the author name: {span:?}"
    );
}

#[test]
fn assistant_header_ignores_blank_model_labels() {
    let theme = Theme::tokyo_night();
    let line = assistant_header_line(Some("   "), None, &theme);
    let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
    assert_eq!(text, "\u{25cf} Chibi");
}

/// THE wrap regression: a long model name can wrap the header line in a
/// narrow viewport. (1) The renderer's row total must grow with the
/// wrapped header (row-accurate math counts display rows). (2) In
/// follow-bottom mode the newest reply must still be fully visible —
/// a miscounted total would hide its tail behind the input block
/// (the wrapping-math class of failure).
#[test]
fn long_model_name_wrap_is_absorbed_by_row_math() {
    const FRAME_W: u16 = 60;
    const FRAME_H: u16 = 24;
    // Chat pane inner width = frame - DEFAULT_SIDEBAR_WIDTH - the pane's two
    // rounded border columns.
    let pane_width = (FRAME_W - DEFAULT_SIDEBAR_WIDTH - 2) as usize;

    let model_name = "super-long-model-name-ultra-extended-edition-v42";
    let tail_word = "tail42";

    let mut app = App::new(vec![Chat::new("wrap")]);
    app.chats[0].messages.push(Message::user("question"));
    app.chats[0].messages.push(Message::assistant_with_model(
        format!("answer one-liner {tail_word}"),
        model_name,
    ));

    // (1) Row math: the labelled header must occupy MORE display rows
    // than the plain header at this width (the wrap is absorbed by the
    // same totals the renderer consumes).
    let theme = Theme::tokyo_night();
    let build_lines = |label: Option<&str>| {
        let mut lines: Vec<markdown::MdLine> = Vec::new();
        for msg in [
            Message::user("question"),
            Message::assistant_with_model(
                format!("answer one-liner {tail_word}"),
                label.unwrap_or(""),
            ),
        ] {
            match msg.role {
                Role::User => lines.push(Line::from(Span::styled("\u{25cf} You", Style::new()))),
                Role::Assistant => {
                    lines.push(assistant_header_line(msg.model_label(), None, &theme))
                }
            }
            lines.extend(markdown::render(&msg.markdown, &theme));
            lines.push(Line::from(""));
        }
        lines
    };
    let plain_rows = wrap_message_rows_indexed(&build_lines(None), pane_width)
        .0
        .len();
    let labelled_rows = wrap_message_rows_indexed(&build_lines(Some(model_name)), pane_width)
        .0
        .len();
    assert!(
        labelled_rows > plain_rows,
        "long model name must wrap and add display rows: {labelled_rows} vs {plain_rows}"
    );

    // (2) Full render, follow-bottom: the labelled header START (it
    // wrapped) may scroll off, but the newest reply's tail must remain
    // visible right above the input row.
    let (rows, _) = render_grid_at_with_buffer(&mut app, FRAME_W, FRAME_H);
    let flat = rows.join("\n");
    assert!(
        flat.contains(tail_word),
        "newest reply tail hidden — wrap row math regressed:\n{flat}"
    );
    // The wrapped label itself renders across rows: its head chunk and
    // tail chunk must both be present (split-point-agnostic — the exact
    // break column depends on the pane width, not on the label's
    // integrity, which is precisely what the row math absorbs).
    assert!(
        flat.contains("(super-long-model-name") && flat.contains("edition-v42"),
        "wrapped label rows themselves must render:\n{flat}"
    );
}

/// End-to-end render: labelled and unlabelled messages coexist in one
/// thread, each rendering exactly its own header shape.
#[test]
fn mixed_labelled_and_plain_messages_render_per_message() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0].messages.push(Message::user("q1"));
    app.chats[0]
        .messages
        .push(Message::assistant_with_model("old answer", "glm-5.2"));
    app.chats[0].messages.push(Message::user("q2"));
    app.chats[0].messages.push(Message::assistant("new answer"));

    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains("(glm-5.2)"),
        "labelled header missing:\n{flat}"
    );
    let lines_with_label = flat
        .lines()
        .filter(|r| r.contains("\u{25cf} Chibi"))
        .count();
    assert!(
        lines_with_label >= 2,
        "both assistant headers must render: {flat}"
    );
}

/// THE backfill regression: switching the
/// model mid-chat must not re-label answers produced earlier. A row
/// without its own per-message label — restored pre-label history or a
/// fieldless result frame — keeps the plain `● Chibi` header even when
/// the thread's last-known model (the status strip source) names a
/// different model.
#[test]
fn model_switch_never_relabels_unlabeled_answers() {
    // Post-switch state: the picker stamped the thread's last-known
    // model with the NEW selection (Ctrl+M confirm), while the earlier
    // answer predates the label and carries no annotation of its own.
    let mut chat = Chat::new("switched");
    chat.messages
        .push(Message::assistant("answer made before the switch"));
    chat.last_model = Some("kimi-k3".to_string());

    let mut app = App::new(vec![chat]);
    assert_eq!(
        app.active_model_label(),
        Some("kimi-k3"),
        "panel reflects the switched model"
    );

    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.lines()
            .any(|r| r.contains("\u{25cf} Chibi") && !r.contains('(')),
        "the unlabeled answer must keep the plain header:\n{flat}"
    );
    assert!(
        !flat.contains("(kimi-k3)"),
        "the switched model must not backfill old answers:\n{flat}"
    );
}

/// Per-answer independence: each header renders the model that produced
/// ITS message, captured at answer time — never the thread's current
/// selection, never a neighbor's label.
#[test]
fn each_answer_keeps_its_own_captured_label() {
    let mut chat = Chat::new("multi");
    chat.messages
        .push(Message::assistant_with_model("first answer", "glm-5.2"));
    chat.messages
        .push(Message::assistant_with_model("second answer", "kimi-k3"));
    chat.last_model = Some("qwen-flash".to_string());

    let mut app = App::new(vec![chat]);
    let flat = render_grid(&mut app).join("\n");

    assert!(
        flat.lines()
            .any(|r| r.contains("\u{25cf} Chibi") && r.contains("(glm-5.2)")),
        "the first answer keeps its own model:\n{flat}"
    );
    assert!(
        flat.lines()
            .any(|r| r.contains("\u{25cf} Chibi") && r.contains("(kimi-k3)")),
        "the second answer keeps its own model:\n{flat}"
    );
    assert!(
        !flat.contains("(qwen-flash)"),
        "the panel selection must not leak into the transcript:\n{flat}"
    );
}

/// Restart seam: restored rows render ONLY their own persisted label.
/// Rows saved before labels were persisted (no `model` key) reload
/// plain and are never backfilled from the thread's last-known model;
/// the panel readout (status strip source) still names that model.
#[test]
fn restored_rows_render_own_persisted_label_and_legacy_rows_stay_plain() {
    let mut chat = Chat::new("restored");
    chat.messages
        .push(Message::assistant_with_model("old answer", "some/model"));
    chat.messages.push(Message::assistant("pre-label answer"));
    chat.last_model = Some("some/model".to_string());

    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-annotation-{}-{}",
        std::process::id(),
        crate::history::new_thread_id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    crate::history::save_chat_in(Some(&dir), &chat).expect("save");
    let restored = crate::history::load_chats_from(Some(&dir));
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        restored[0].last_model.as_deref(),
        Some("some/model"),
        "last-known model persists with the thread"
    );
    assert_eq!(
        restored[0].messages[0].model_label(),
        Some("some/model"),
        "the per-answer label persists with its message"
    );
    assert!(
        restored[0].messages[1].model_label().is_none(),
        "pre-label rows stay label-less on disk and in memory"
    );

    let mut app = App::new(restored);
    assert_eq!(
        app.active_model_label(),
        Some("some/model"),
        "status strip source seeded from the snapshot"
    );

    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.lines()
            .any(|r| r.contains("\u{25cf} Chibi") && r.contains("(some/model)")),
        "the labeled row keeps its persisted label after restart:\n{flat}"
    );
    let labeled_headers = flat
        .lines()
        .filter(|r| r.contains("\u{25cf} Chibi ("))
        .count();
    assert_eq!(
        labeled_headers, 1,
        "no backfill: only the row with its own label is labeled:\n{flat}"
    );
}

// per-message creation stamp in the header -------------------------

/// A stamped assistant header renders an extra dim span carrying the
/// local `HH:MM`; an unstamped row renders exactly as before (no third
/// span, no placeholder time).
#[test]
fn assistant_header_appends_a_dim_timestamp_span_only_when_stamped() {
    let theme = Theme::tokyo_night();

    let unstamped = assistant_header_line(None, None, &theme);
    assert_eq!(
        unstamped.spans.len(),
        1,
        "no stamp ⇒ no timestamp span: {unstamped:?}"
    );

    let clock = crate::localtime::fmt_hhmm(1_700_000_000);
    let stamped = assistant_header_line(None, Some(1_700_000_000), &theme);
    assert_eq!(
        stamped.spans.len(),
        2,
        "a stamped row gains exactly one extra span: {stamped:?}"
    );
    let ts = &stamped.spans[1];
    assert_eq!(ts.content, format!("  {clock}"), "renders the local HH:MM");
    assert_eq!(ts.style.fg, Some(theme.dim), "the timestamp is dim");
    assert!(
        !ts.style.add_modifier.contains(Modifier::BOLD),
        "the timestamp must not inherit the bold header"
    );

    // A labelled AND stamped header carries both suffixes, model label
    // first, timestamp last.
    let both = assistant_header_line(Some("glm-5.2"), Some(1_700_000_000), &theme);
    assert_eq!(both.spans.len(), 3);
    assert_eq!(both.spans[1].content, " (glm-5.2)");
    assert_eq!(both.spans[2].content, format!("  {clock}"));
}

/// End-to-end: the user header (`● You`) renders the same dim `HH:MM`
/// suffix for a stamped message and nothing extra for an unstamped one,
/// so old chats (no `created_at` on disk) render unaffected.
#[test]
fn user_header_renders_timestamp_only_when_the_message_is_stamped() {
    let mut app = App::new(vec![Chat::new("ts")]);
    app.chats[0].messages.push(Message::user("no stamp"));
    app.chats[0]
        .messages
        .push(Message::user("stamped").at(1_700_000_000));

    let clock = crate::localtime::fmt_hhmm(1_700_000_000);
    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains(&clock),
        "the stamped user message renders its local time:\n{flat}"
    );
    let you_lines: Vec<&str> = flat
        .lines()
        .filter(|r| r.contains("\u{25cf} You"))
        .collect();
    assert_eq!(you_lines.len(), 2, "both user headers render: {flat}");
    assert!(
        !you_lines[0].contains(':'),
        "the unstamped header carries no clock: {}",
        you_lines[0]
    );
}
