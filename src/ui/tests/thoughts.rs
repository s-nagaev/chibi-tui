use super::super::*;
use super::support::*;

// dim reasoning block above the answer ------------

/// With thoughts retained and the toggle ON, the block renders in the
/// dim slot directly ABOVE the latest answer; ^S hides it while the
/// answer itself stays untouched.
#[test]
fn thoughts_block_renders_dim_above_answer_and_toggle_hides_it() {
    let mut app = App::new(vec![Chat::new("t")]);
    app.chats[0].messages.push(Message::user("question"));
    app.chats[0].messages.push(Message::assistant("the answer"));
    app.chats[0].last_thoughts = Some("reasoning trace line".into());

    let (rows, buf) = render_grid_with_buffer(&mut app);
    let thought_row = rows
        .iter()
        .position(|r| r.contains("reasoning trace line"))
        .expect("thoughts line must render");
    let answer_row = rows
        .iter()
        .position(|r| r.contains("the answer"))
        .expect("answer must render");
    assert!(
        thought_row < answer_row,
        "thoughts block must sit ABOVE the answer"
    );
    // The thought text paints in the dim slot.
    let col = rows[thought_row].find("reasoning").unwrap();
    assert_eq!(
        buf[(col as u16, thought_row as u16)].fg,
        Theme::tokyo_night().dim,
        "thoughts text must be dim"
    );

    app.toggle_thoughts();
    let flat = render_grid(&mut app).join("\n");
    assert!(
        !flat.contains("reasoning trace line"),
        "toggle OFF must hide the block"
    );
    assert!(
        flat.contains("the answer"),
        "answer must survive the toggle"
    );
}

/// A CHAIN of thoughts (terminal result + background continuation
/// deltas) accumulates in the dim block: driving the REAL event path
/// with three payloads, all three thoughts render above the latest
/// answer. Regression for the owner report: continuation thoughts used
/// to overwrite the block, so only one payload of the chain was ever
/// visible.
#[test]
fn thought_chain_renders_all_members_above_the_latest_answer() {
    let mut app = App::new(vec![Chat::new("t")]);
    let thread_id = app.chats[0].id.clone();
    app.chats[0].messages.push(Message::user("do the thing"));
    app.chats[0].messages.push(Message::assistant_pending());
    app.chats[0].lifecycle = ChatLifecycle::Awaiting {
        request_id: "req-chain".into(),
    };

    app.apply_backend_event(crate::backend::BackendEvent::Result {
        request_id: crate::live::wire_thread_id("req-chain") as u64,
        markdown: "step one".into(),
        thread_id: thread_id.clone(),
        model: None,
        usage: None,
        thoughts: Some("first thought".into()),
    });
    for (markdown, thought) in [
        ("step two", "second thought"),
        ("step three", "third thought"),
    ] {
        app.apply_backend_event(crate::backend::BackendEvent::BackgroundMessage {
            wire_thread_id: crate::live::wire_thread_id(&thread_id),
            markdown: markdown.into(),
            model: None,
            thoughts: Some(thought.into()),
        });
    }

    let rows = render_grid(&mut app);
    let flat = rows.join("\n");
    for thought in ["first thought", "second thought", "third thought"] {
        assert!(flat.contains(thought), "chain member missing: {thought}");
    }
    let last_answer_row = rows
        .iter()
        .position(|r| r.contains("step three"))
        .expect("latest answer renders");
    let first_thought_row = rows
        .iter()
        .position(|r| r.contains("first thought"))
        .expect("first chain member renders");
    assert!(
        first_thought_row < last_answer_row,
        "the accumulated chain must sit ABOVE the latest answer"
    );
}

/// Absent or whitespace-only thoughts render NOTHING: the transcript is
/// byte-identical to the no-thoughts baseline (zero layout impact), and
/// toggle OFF reproduces that baseline even with thoughts retained. The
/// STATUS line is excluded from the comparison: the `^S on/off` state
/// token legitimately tracks the toggle there.
#[test]
fn thoughts_block_absent_or_blank_renders_nothing() {
    let mut app = App::new(vec![Chat::new("t")]);
    app.chats[0].messages.push(Message::assistant("answer"));

    assert_eq!(app.chats[0].last_thoughts, None);
    let baseline = render_grid(&mut app);
    let baseline = baseline[..baseline.len() - 1].join("\n");
    assert!(baseline.contains("answer"));

    app.chats[0].last_thoughts = Some("  \n  ".into());
    let blank = render_grid(&mut app);
    let blank = blank[..blank.len() - 1].join("\n");
    assert_eq!(
        blank, baseline,
        "whitespace-only thoughts must not change the transcript"
    );

    app.toggle_thoughts();
    app.chats[0].last_thoughts = Some("reasoning trace line".into());
    let hidden = render_grid(&mut app);
    let hidden = hidden[..hidden.len() - 1].join("\n");
    assert_eq!(
        hidden, baseline,
        "toggle OFF must reproduce the no-thoughts layout exactly"
    );
}

/// Long traces keep only the LAST 10 lines, with the `…` head marker on
/// the first kept line; exactly-10 lines render unmarked.
#[test]
fn thoughts_block_caps_last_ten_lines_with_ellipsis_head() {
    let mut app = App::new(vec![Chat::new("t")]);
    app.chats[0].messages.push(Message::assistant("answer"));
    app.chats[0].last_thoughts = Some(
        (1..=15)
            .map(|i| format!("thought line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let flat = render_grid(&mut app).join("\n");
    assert!(!flat.contains("thought line 01"), "pre-cap head is dropped");
    assert!(!flat.contains("thought line 05"));
    assert!(
        flat.contains("thought line 06"),
        "first kept (last-10) line"
    );
    assert!(
        flat.contains("thought line 15"),
        "line closest to the answer"
    );
    assert!(
        flat.contains("\u{2026}thought line 06"),
        "ellipsis head marker on the first kept line"
    );
    assert!(
        !flat.contains("\u{2026}thought line 07"),
        "marker only on the head line"
    );

    // Exactly 10 lines: everything visible, no marker.
    app.chats[0].last_thoughts = Some(
        (1..=10)
            .map(|i| format!("thought line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let flat = render_grid(&mut app).join("\n");
    assert!(flat.contains("thought line 01"));
    assert!(
        !flat.contains("\u{2026}thought line 01"),
        "no marker when untruncated"
    );
}

/// Each chat renders its OWN reasoning: switching threads swaps the
/// block to the entered chat's trace and never leaks the other chat's
/// reasoning into the view (the renderer reads the active chat's
/// `Chat::last_thoughts` directly).
#[test]
fn thoughts_follow_the_active_chat_across_thread_switches() {
    let mut app = App::new(vec![Chat::new("alpha"), Chat::new("beta")]);
    app.chats[0]
        .messages
        .push(Message::assistant("alpha answer"));
    app.chats[1]
        .messages
        .push(Message::assistant("beta answer"));
    app.chats[0].last_thoughts = Some("alpha reasoning line".into());
    app.chats[1].last_thoughts = Some("beta reasoning line".into());

    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains("alpha reasoning line"),
        "the active chat's own trace renders: {flat:?}"
    );
    assert!(
        !flat.contains("beta reasoning line"),
        "another chat's reasoning must not leak into the view"
    );

    app.select_next();
    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains("beta reasoning line"),
        "switching threads shows the entered chat's trace"
    );
    assert!(
        !flat.contains("alpha reasoning line"),
        "the chat left behind must not bleed into the new view"
    );

    app.select_prev();
    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains("alpha reasoning line"),
        "switching back restores the chat's own sticky trace"
    );
    assert!(!flat.contains("beta reasoning line"));
}

/// the status line carries the toggle state —
/// `^S on` by default, `^S off` after the toggle — so Ctrl+S always has
/// visible feedback (the chord doubles as the row's hint; the F1 modal
/// spells the action out).
#[test]
fn status_line_shows_the_thoughts_toggle_state() {
    let mut app = App::new(vec![Chat::new("t")]);

    let last = render_grid(&mut app).last().unwrap().clone();
    assert!(
        last.contains("^S on"),
        "default state must show on the status line: {last:?}"
    );
    assert!(!last.contains("^S off"));

    app.toggle_thoughts();
    let last = render_grid(&mut app).last().unwrap().clone();
    assert!(
        last.contains("^S off"),
        "the toggle must flip the status indicator: {last:?}"
    );
    assert!(
        !last.contains("^S on"),
        "the stale state must not linger next to the new one: {last:?}"
    );
}
