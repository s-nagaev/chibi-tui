use super::super::*;
use super::support::*;

// ---- per-thread async: render-level lifecycle checks -----------------

/// Render the full UI and return ONLY the spinner line (3rd row from the
/// bottom in the 120×34 demo grid: above input + status hotkey line).
/// NOTE: the sidebar's vertical border extension crosses every bottom
/// row at its column — strip it so emptiness checks are meaningful.
fn spinner_row_of(app: &mut App) -> String {
    let mut rows = render_grid(app);
    rows.remove(rows.len() - 3)
}

fn spinner_line_of(app: &mut App) -> String {
    spinner_row_of(app)
        .chars()
        .filter(|&c| c != '\u{2502}')
        .collect()
}

/// Sidebar dots must encode the PER-CHAT lifecycle: ○ idle (dim),
/// ◆ awaiting/queued (yellow), ● running (green) at column 0 of each
/// chat's sidebar row. The active chat here is the queued one, which
/// also proves selection still works while busy.
#[test]
fn sidebar_dots_distinguish_lifecycle_states() {
    let theme = Theme::tokyo_night();
    let idle = Chat::new("idle");
    let mut queued = Chat::new("queued");
    queued.lifecycle = ChatLifecycle::Awaiting {
        request_id: "req-queued".into(),
    };
    let mut running = Chat::new("running");
    running.lifecycle = ChatLifecycle::Running {
        request_id: "req-running".into(),
    };
    let mut app = App::new(vec![idle, queued, running]);
    app.active = 1;

    let (rows, buf) = render_grid_with_buffer(&mut app);

    // Sidebar list starts at the top-left corner: one row per chat, the
    // lifecycle dot occupying column 0.
    let expected = [
        ("\u{25cb}", theme.dim),    // idle → hollow dim dot
        ("\u{25c6}", theme.yellow), // awaiting → yellow diamond
        ("\u{25cf}", theme.green),  // running → green filled dot
    ];
    let names = ["idle", "queued", "running"];
    for (i, (glyph, color)) in expected.iter().enumerate() {
        // Sidebar block renders its " Chats " title on grid row 0; the
        // per-chat list rows start one row below, and the lifecycle dot
        // occupies column 1 (right of the outline's left border).
        let y = i + 1;
        assert!(
            rows[y].contains(names[i]),
            "sidebar row {y} must list chat {}, got {:?}",
            names[i],
            rows[y]
        );
        let cell = &buf[(1, y as u16)];
        assert_eq!(
            cell.symbol(),
            *glyph,
            "sidebar row {y}: wrong lifecycle glyph"
        );
        assert_eq!(cell.fg, *color, "sidebar row {y}: wrong dot color");
    }
}

/// The spinner line mirrors ONLY the active chat's lifecycle: visible
/// with its label while THIS chat is queued/running, hidden when the
/// active chat idles — even if a background chat is still working.
#[test]
fn spinner_line_shows_and_hides_per_active_lifecycle() {
    let mut app = App::new(vec![Chat::new("solo")]);

    // Idle: line empty.
    assert!(
        spinner_line_of(&mut app).trim().is_empty(),
        "idle chat must keep the spinner line empty"
    );

    // Awaiting: queued… visible.
    app.chats[0].lifecycle = ChatLifecycle::Awaiting {
        request_id: "req-a".into(),
    };
    let line = spinner_line_of(&mut app);
    assert!(
        line.contains("queued") && line.contains(app.spinner_char()),
        "awaiting chat shows spinner + queued…, got {line:?}"
    );

    // Running: thinking… replaces queued….
    app.chats[0].lifecycle = ChatLifecycle::Running {
        request_id: "req-a".into(),
    };
    let line = spinner_line_of(&mut app);
    assert!(
        line.contains("thinking"),
        "running chat shows thinking…, got {line:?}"
    );

    // Back to idle: hidden again.
    app.chats[0].lifecycle = ChatLifecycle::Idle;
    assert!(spinner_line_of(&mut app).trim().is_empty());
}

/// Background work must NOT light up the spinner line of an idle active
/// chat (per-thread isolation at RENDER level, not just state level).
#[test]
fn spinner_line_ignores_background_chat_activity() {
    let mut busy = Chat::new("busy");
    busy.lifecycle = ChatLifecycle::Running {
        request_id: "req-bg".into(),
    };
    let mut app = App::new(vec![Chat::new("focused"), busy]);
    app.active = 0;

    let line = spinner_line_of(&mut app);
    assert!(
        line.trim().is_empty(),
        "background chat must not spin the active chat's line, got {line:?}"
    );
}

// subagent counter in the spinner line -----------

/// While the active chat reports live subagents, the spinner line gains
/// a ` · subagents working: n` segment; without one the line stays
/// byte-for-byte unchanged.
#[test]
fn spinner_line_appends_subagents_working_counter() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0].lifecycle = ChatLifecycle::Running {
        request_id: "req-sub".into(),
    };

    let base = spinner_line_of(&mut app);
    assert!(base.contains("thinking"), "precondition: spinner visible");
    assert!(
        !base.contains("subagents"),
        "no counter before any agent_event, got {base:?}"
    );

    app.chats[0].apply_subagent_event(11, AgentEventKind::Started, 2, 5);
    let line = spinner_line_of(&mut app);
    assert_eq!(
        line.trim_end(),
        format!("{} \u{00b7} subagents working: 2", base.trim_end()),
        "counter must append to the unchanged spinner text, got {line:?}"
    );

    // Fewer live subagents → the rendered count follows the frame values.
    app.chats[0].apply_subagent_event(11, AgentEventKind::Finished, 1, 5);
    let line = spinner_line_of(&mut app);
    assert!(
        line.contains("subagents working: 1"),
        "counter must track the frame values, got {line:?}"
    );
}

/// The counter is independent of the request lifecycle: an IDLE chat
/// whose turn's subagents still run renders the counter alone — no
/// spinner, no lifecycle label — and hides it once the count hits 0.
#[test]
fn spinner_line_shows_subagents_without_a_request() {
    let mut app = App::new(vec![Chat::new("chat")]);
    assert!(
        spinner_line_of(&mut app).trim().is_empty(),
        "precondition: idle chat renders an empty line"
    );

    app.chats[0].apply_subagent_event(11, AgentEventKind::Started, 2, 5);
    let line = spinner_line_of(&mut app);
    assert!(
        line.contains("subagents working: 2"),
        "idle chat must keep the subagent counter visible, got {line:?}"
    );
    assert!(
        !line.contains("thinking") && !line.contains("queued"),
        "no request indicator while idle, got {line:?}"
    );
    assert!(
        !line.contains(app.spinner_char()),
        "the spinner belongs to the request lifecycle only, got {line:?}"
    );

    app.chats[0].apply_subagent_event(11, AgentEventKind::Finished, 0, 5);
    assert!(
        spinner_line_of(&mut app).trim().is_empty(),
        "count 0 must hide the counter again"
    );
}

/// A finished (active == 0) entry renders nothing, even while the chat
/// is busy again.
#[test]
fn spinner_line_hides_zero_subagent_counters() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.chats[0].lifecycle = ChatLifecycle::Running {
        request_id: "req-sub".into(),
    };

    app.chats[0].apply_subagent_event(11, AgentEventKind::Started, 3, 3);
    app.chats[0].apply_subagent_event(11, AgentEventKind::Finished, 0, 3);
    assert!(
        !spinner_line_of(&mut app).contains("subagents"),
        "finished (active == 0) entry must not render"
    );
}

/// Background chats' subagents never leak into the active chat's
/// spinner line (render-level per-thread isolation, like the lifecycle
/// dots above).
#[test]
fn spinner_line_ignores_background_chat_subagents() {
    let mut focused = Chat::new("focused");
    focused.lifecycle = ChatLifecycle::Running {
        request_id: "req-front".into(),
    };
    let busy = Chat::new("busy");
    let mut app = App::new(vec![focused, busy]);
    app.active = 0;

    app.chats[1].apply_subagent_event(9, AgentEventKind::Started, 4, 4);
    let line = spinner_line_of(&mut app);
    assert!(
        line.contains("thinking") && !line.contains("subagents"),
        "background counter must not leak, got {line:?}"
    );
}
