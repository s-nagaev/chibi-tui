//! End-to-end integration tests: fake-backend frames → glue events →
//! App state → rendered UI, plus the persistence and diagnostics contracts.
//!
//! `tests/fake_backend.py` emits extended frames (usage, thoughts, mid-turn
//! `agent_event` sequences, one unknown-type frame) gated on the client's
//! handshake capabilities; these tests verify the TUI consumes ALL of them
//! through the real `LiveBackend` glue path — the same seam `main.rs` uses.
//!
//! Every await step is wrapped in a generous timeout so a protocol deadlock
//! fails fast in CI instead of hanging the suite.

use std::time::Duration;

use chibi_tui::app::{App, Chat, Submitted};
use chibi_tui::backend::BackendEvent;
use chibi_tui::diag;
use chibi_tui::history::{load_chats_from, new_request_id, save_chat_in};
use chibi_tui::live::LiveBackend;
use chibi_tui::model::ChatLifecycle;
use chibi_tui::protocol::{AgentEventKind, Usage};
use chibi_tui::theme::Theme;
use chibi_tui::ui;
use ratatui::backend::TestBackend;

const TIMEOUT: Duration = Duration::from_secs(10);

const ANSWER: &str = "This function `foo` returns the integer `42`.";
const THOUGHTS_MARKER: &str = "\n[... LLM reasoning truncated: 64 KB limit reached ...]";
const THOUGHTS_SNIPPET: &str = "Checking protocol constraints";

/// Tests always target the fake backend — set `CHIBI_FAKE_BACKEND` explicitly
/// (tests run with CWD = crate root, so the relative path resolves).
fn fake_backend_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("CHIBI_FAKE_BACKEND", "tests/fake_backend.py");
    });
}

async fn connect(extra: &[&str]) -> LiveBackend {
    fake_backend_env();
    let args: Vec<String> = extra.iter().map(|s| (*s).to_owned()).collect();
    tokio::time::timeout(TIMEOUT, LiveBackend::connect_with_args(".", &args))
        .await
        .expect("connect within timeout")
        .expect("handshake ok")
}

fn app_with_one_chat() -> App {
    App::new(vec![Chat::new("wave2")])
}

fn submitted_for(app: &App, prompt: &str) -> Submitted {
    Submitted {
        request_id: new_request_id(),
        thread_id: app.chats[0].id.clone(),
        prompt: prompt.to_owned(),
    }
}

/// Render the full UI offscreen (demo resolution) and return the plain-text
/// cell grid plus a buffer snapshot for per-cell style assertions.
fn render_grid_with_buffer(app: &mut App) -> (Vec<String>, ratatui::buffer::Buffer) {
    let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 34)).unwrap();
    terminal
        .draw(|f| ui::draw(f, app, &Theme::tokyo_night()))
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    let rows = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, buf)
}

fn render_grid(app: &mut App) -> Vec<String> {
    render_grid_with_buffer(app).0
}

/// The spinner line (3rd row from the bottom in the 120×34 grid), with the
/// sidebar's vertical border extension stripped so substring checks are
/// meaningful.
fn spinner_line(app: &mut App) -> String {
    let mut rows = render_grid(app);
    let row = rows.remove(rows.len() - 3);
    row.chars().filter(|&c| c != '\u{2502}').collect()
}

/// Submit a prompt through the real glue path, folding every event into
/// `app` as it arrives (what the event loop does) and returning the events
/// in arrival order, up to and including the per-thread drain signal.
async fn submit_and_fold(
    live: &LiveBackend,
    app: &mut App,
    submitted: Submitted,
) -> Vec<BackendEvent> {
    app.begin_request(&submitted);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    live.submit_encoded(submitted, tx);
    let mut events = Vec::new();
    loop {
        let evt = tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .expect("event in time")
            .expect("channel alive");
        let done = matches!(evt, BackendEvent::QueueDrain { .. });
        app.apply_backend_event(evt.clone());
        events.push(evt);
        if done {
            break;
        }
    }
    events
}

fn terminal_result(events: &[BackendEvent]) -> (String, Option<Usage>, Option<String>) {
    events
        .iter()
        .find_map(|e| match e {
            BackendEvent::Result {
                markdown,
                usage,
                thoughts,
                ..
            } => Some((markdown.clone(), *usage, thoughts.clone())),
            _ => None,
        })
        .expect("terminal Result present")
}

/// The full extended result payload survives the real glue path: usage and
/// thoughts arrive on the terminal event AND are retained in App state; a
/// plain session (no subagent flag) emits no agent progress at all.
#[tokio::test]
async fn result_carries_usage_and_thoughts_end_to_end() {
    let live = connect(&["--with-usage", "--with-thoughts"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "explain wave-2");
    let events = submit_and_fold(&live, &mut app, submitted).await;

    let (markdown, usage, thoughts) = terminal_result(&events);
    assert_eq!(markdown, ANSWER, "content passes through verbatim");
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 18432,
            output_tokens: 512,
            context_window: Some(131072),
        }),
        "usage payload arrives intact"
    );
    let thoughts = thoughts.expect("thoughts present");
    assert!(
        thoughts.contains(THOUGHTS_SNIPPET),
        "reasoning text arrives: {thoughts}"
    );

    assert_eq!(app.last_turn_usage, usage, "usage retained in App state");
    assert_eq!(
        app.chats[0].last_thoughts.as_deref(),
        Some(thoughts.as_str()),
        "thoughts retained in App state"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, BackendEvent::AgentProgress { .. })),
        "no AgentProgress without --with-subagents"
    );

    let _ = live.shutdown().await;
}

/// Rendered transcript: the live thoughts land in the dim block ABOVE the
/// answer, and the session toggle hides the block without touching the
/// answer.
#[tokio::test]
async fn thoughts_render_dim_above_the_live_answer() {
    let live = connect(&["--with-usage", "--with-thoughts"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "explain wave-2");
    submit_and_fold(&live, &mut app, submitted).await;

    let (rows, buf) = render_grid_with_buffer(&mut app);
    let thought_row = rows
        .iter()
        .position(|r| r.contains(THOUGHTS_SNIPPET))
        .expect("thoughts line must render");
    let answer_row = rows
        .iter()
        .position(|r| r.contains("returns the integer"))
        .expect("answer must render");
    assert!(
        thought_row < answer_row,
        "thoughts block must sit ABOVE the answer"
    );
    let col = rows[thought_row].find("Checking").unwrap() as u16;
    assert_eq!(
        buf[(col, thought_row as u16)].fg,
        Theme::tokyo_night().dim,
        "thoughts text must be dim"
    );

    app.toggle_thoughts();
    let flat = render_grid(&mut app).join("\n");
    assert!(
        !flat.contains(THOUGHTS_SNIPPET),
        "toggle OFF must hide the block"
    );
    assert!(
        flat.contains("returns the integer"),
        "answer must survive the toggle"
    );

    let _ = live.shutdown().await;
}

/// Status strip: after a live result the ctx segment renders with the
/// floored percentage and human token counts; the windowless fake variant
/// (`context_window: null`) degrades to the absolute count, no percent sign.
#[tokio::test]
async fn usage_ctx_segment_renders_in_status_strip() {
    let live = connect(&["--with-usage"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "count tokens");
    submit_and_fold(&live, &mut app, submitted).await;

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains(" \u{00b7} ctx 14% (18.4k/131.0k)"),
        "ctx segment missing or malformed: {:?}",
        rows[0]
    );
    let _ = live.shutdown().await;

    let live = connect(&["--usage-windowless"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "count tokens again");
    submit_and_fold(&live, &mut app, submitted).await;

    let rows = render_grid(&mut app);
    assert!(
        rows[0].contains(" \u{00b7} ctx 18.4k"),
        "absolute usage missing: {:?}",
        rows[0]
    );
    assert!(
        !rows[0].contains('%'),
        "no pct without a window: {:?}",
        rows[0]
    );
    let _ = live.shutdown().await;
}

/// Live subagent progress: while the tracked request reports live subagents
/// the spinner line carries `· subagents working: n` following the frame
/// values; after the terminal result the counter is gone.
#[tokio::test]
async fn subagent_counter_appears_on_spinner_line_and_clears_after_result() {
    let live = connect(&["--with-subagents"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "spawn helpers");
    app.begin_request(&submitted);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    live.submit_encoded(submitted, tx);

    let mut saw_live_counter = false;
    let terminal_markdown = loop {
        let evt = tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .expect("event in time")
            .expect("channel alive");
        match evt.clone() {
            BackendEvent::AgentProgress {
                event: AgentEventKind::Started,
                active,
                ..
            } => {
                app.apply_backend_event(evt);
                let line = spinner_line(&mut app);
                assert!(
                    line.contains(&format!("subagents working: {active}")),
                    "spinner must track live subagents ({active}): {line}"
                );
                saw_live_counter = true;
            }
            BackendEvent::AgentProgress {
                event: AgentEventKind::Finished,
                active: 0,
                ..
            } => {
                app.apply_backend_event(evt);
                assert_eq!(
                    app.active_chat_subagents(),
                    None,
                    "final finish (active == 0) removes the counter entry"
                );
            }
            BackendEvent::AgentProgress { .. } => {
                app.apply_backend_event(evt);
            }
            BackendEvent::Result { markdown, .. } => {
                app.apply_backend_event(evt);
                break markdown;
            }
            other => app.apply_backend_event(other),
        }
    };
    assert_eq!(
        terminal_markdown, ANSWER,
        "request must complete with its answer"
    );
    assert!(saw_live_counter, "started frames must surface the counter");

    let drain = tokio::time::timeout(TIMEOUT, rx.recv())
        .await
        .expect("drain in time")
        .expect("channel alive");
    assert!(matches!(drain, BackendEvent::QueueDrain { .. }));

    let line = spinner_line(&mut app);
    assert!(
        !line.contains("subagents"),
        "counter must clear after the result: {line}"
    );
    assert_eq!(app.active_chat_subagents(), None);
    assert!(matches!(app.chats[0].lifecycle, ChatLifecycle::Idle));

    let _ = live.shutdown().await;
}

/// Background subagents outliving their turn (`--late-finish`): the two
/// `finished` frames arrive strictly AFTER the result frame and must still
/// reach the per-chat counter through the real glue path — decrements per
/// finish, hidden at zero — while the request lifecycle stays terminal.
/// Folds every event as it arrives, continuing past the drain signal (what
/// the event loop does for late frames).
#[tokio::test]
async fn post_result_subagent_finishes_update_the_counter() {
    let live = connect(&["--late-finish"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "background helpers");
    app.begin_request(&submitted);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    live.submit_encoded(submitted, tx);

    let mut tags: Vec<String> = Vec::new();
    let mut finished_count = 0usize;
    let mut drained = false;
    while !drained || finished_count < 2 {
        let evt = tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .expect("event in time")
            .expect("channel alive");
        match evt.clone() {
            BackendEvent::Queued { .. } => tags.push("queued".into()),
            BackendEvent::Running { .. } => tags.push("running".into()),
            BackendEvent::AgentProgress {
                event: AgentEventKind::Started,
                active,
                ..
            } => {
                tags.push("started".into());
                app.apply_backend_event(evt);
                let line = spinner_line(&mut app);
                assert!(
                    line.contains(&format!("subagents working: {active}")),
                    "started frames surface the live counter ({active}): {line}"
                );
            }
            BackendEvent::AgentProgress {
                event: AgentEventKind::Finished,
                active,
                ..
            } => {
                finished_count += 1;
                tags.push(format!("finished:{active}"));
                app.apply_backend_event(evt);
                let line = spinner_line(&mut app);
                if active == 1 {
                    assert!(
                        line.contains("subagents working: 1"),
                        "counter must decrement on the post-result finish: {line}"
                    );
                } else {
                    assert!(
                        !line.contains("subagents"),
                        "counter must hide at zero after the last post-result finish: {line}"
                    );
                    assert_eq!(
                        app.active_chat_subagents(),
                        None,
                        "zero-active finish removes the counter entry"
                    );
                }
            }
            BackendEvent::Result { markdown, .. } => {
                tags.push("result".into());
                app.apply_backend_event(evt);
                assert_eq!(markdown, ANSWER, "request completes with its answer");
            }
            BackendEvent::QueueDrain { .. } => {
                tags.push("drain".into());
                drained = true;
            }
            other => app.apply_backend_event(other),
        }
    }

    // The finishes are genuinely post-result: strictly after the terminal
    // frame AND after the per-thread drain signal.
    assert_eq!(
        tags,
        [
            "queued",
            "running",
            "started",
            "started",
            "result",
            "drain",
            "finished:1",
            "finished:0"
        ],
        "late finished frames must flow in wire order after the result"
    );
    assert!(matches!(app.chats[0].lifecycle, ChatLifecycle::Idle));

    let _ = live.shutdown().await;
}

/// Thoughts are session-only: a full live turn with reasoning, saved and
/// reloaded as a fresh session (restart), shows the answer but never the
/// reasoning — and replaying the same result frame into the fresh app
/// (replaced backend) cannot resurrect it. The thread's last-known usage
/// rides the same restart the other way: it IS restored into the ctx
/// segment seed, and an ignored replay cannot wipe it.
#[tokio::test]
async fn thoughts_never_persist_across_restart_or_replay() {
    let live = connect(&["--with-usage", "--with-thoughts"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "think hard");
    let events = submit_and_fold(&live, &mut app, submitted).await;
    assert!(
        app.chats[0].last_thoughts.is_some(),
        "precondition: thoughts retained during the session"
    );
    let _ = live.shutdown().await;

    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-wave2-e2e-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("temp root created");

    let saved_path = save_chat_in(Some(&dir), &app.chats[0]).expect("chat saved");
    let raw = std::fs::read_to_string(&saved_path).expect("history file readable");
    assert!(
        !raw.contains(THOUGHTS_SNIPPET),
        "persisted history must not contain thoughts: {raw}"
    );

    let chats = load_chats_from(Some(&dir));
    assert_eq!(chats.len(), 1, "round trip preserves the thread");
    let mut fresh = App::new(chats);
    let (_, usage, _) = terminal_result(&events);
    assert_eq!(
        fresh.last_turn_usage, usage,
        "ctx segment is seeded from the persisted thread usage"
    );
    assert_eq!(
        fresh.chats[0].last_thoughts, None,
        "thoughts are session-only"
    );

    let flat = render_grid(&mut fresh).join("\n");
    assert!(
        !flat.contains(THOUGHTS_SNIPPET),
        "no thoughts strip after restart"
    );
    assert!(
        flat.contains("returns the integer"),
        "the answer itself survives the restart"
    );

    // Replace-backend replay: the same result frame again — the fresh chat
    // tracks no request, so the frame is ignored and thoughts stay absent.
    let replay = events
        .iter()
        .find(|e| matches!(e, BackendEvent::Result { .. }))
        .cloned()
        .expect("terminal Result present");
    fresh.apply_backend_event(replay);
    assert_eq!(
        fresh.chats[0].last_thoughts, None,
        "replay must not resurrect thoughts"
    );
    assert_eq!(
        fresh.last_turn_usage, usage,
        "a replay ignored by the idle chat cannot wipe the restored usage"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A mid-turn unknown frame type is tolerated: the request still completes
/// with its real answer, and the diagnostics stream names the unknown tag.
#[tokio::test]
async fn unknown_frame_mid_request_is_tolerated_and_traced() {
    let (_, before_total) = diag::view();

    let live = connect(&["--with-unknown-frame"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "tolerate noise");
    let events = submit_and_fold(&live, &mut app, submitted).await;
    let _ = live.shutdown().await;

    let (markdown, _, _) = terminal_result(&events);
    assert_eq!(markdown, ANSWER, "the request completes despite the noise");
    assert!(
        matches!(app.chats[0].lifecycle, ChatLifecycle::Idle),
        "lifecycle resolved back to idle"
    );

    let (lines, after_total) = diag::view();
    assert!(
        after_total > before_total,
        "diagnostics recorded the unknown frame"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.text.contains("unknown frame type: holo_deck")),
        "diag trace must name the unknown tag; got {lines:?}"
    );
}

/// Oversized reasoning arrives backend-capped at 64KB with the exact
/// truncation marker, and the dim block still renders over the answer.
#[tokio::test]
async fn huge_thoughts_arrive_capped_with_truncation_marker() {
    let live = connect(&["--with-thoughts", "--thoughts-huge"]).await;
    let mut app = app_with_one_chat();
    let submitted = submitted_for(&app, "think enormously");
    let events = submit_and_fold(&live, &mut app, submitted).await;
    let _ = live.shutdown().await;

    let (_, _, thoughts) = terminal_result(&events);
    let thoughts = thoughts.expect("thoughts present");
    assert!(
        thoughts.len() <= 64 * 1024,
        "payload must respect the 64KB cap: {} bytes",
        thoughts.len()
    );
    assert!(
        thoughts.ends_with(THOUGHTS_MARKER),
        "cap must append the exact truncation marker"
    );

    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains("returns the integer"),
        "answer still renders under the huge dim block"
    );
}

/// The fake backend's capability gating at the raw wire level: without client
/// capabilities no thoughts/agent frames are emitted; with the capability
/// flags set they appear. Usage is NOT capability-gated and rides in both
/// sessions.
#[test]
fn fake_backend_gates_wave2_frames_on_client_capabilities() {
    let off = raw_fake_session(None);
    assert!(
        !off.iter().any(|f| f["type"] == "agent_event"),
        "no agent frames without capabilities.subagents: {off:?}"
    );
    let off_result = off
        .iter()
        .find(|f| f["type"] == "result")
        .expect("result frame");
    assert!(
        off_result.get("thoughts").is_none(),
        "no thoughts without capabilities.thoughts"
    );
    assert!(
        off_result.get("usage").is_some(),
        "usage is not capability-gated"
    );

    let on = raw_fake_session(Some(serde_json::json!({
        "thoughts": true,
        "subagents": true
    })));
    let agent: Vec<_> = on.iter().filter(|f| f["type"] == "agent_event").collect();
    assert_eq!(
        agent.len(),
        4,
        "started→started→finished→finished sequence: {agent:?}"
    );
    assert_eq!(agent[0]["event"], "started");
    assert_eq!(agent[0]["active"], 1);
    assert_eq!(agent[0]["total"], 1);
    assert_eq!(agent.last().unwrap()["event"], "finished");
    assert_eq!(agent.last().unwrap()["active"], 0, "active back to 0");
    let on_result = on
        .iter()
        .find(|f| f["type"] == "result")
        .expect("result frame");
    assert!(
        on_result["thoughts"].is_string(),
        "thoughts ride when opted in"
    );
    assert_eq!(on_result["usage"]["context_window"], 131072);
}

/// Drive one scripted session against the fake backend over raw stdio
/// (initialize → ready, request → result, shutdown → exit) and return every
/// parsed frame in arrival order. Reading runs on a helper thread with a
/// deadline so a protocol bug fails the test instead of hanging the suite.
fn raw_fake_session(caps: Option<serde_json::Value>) -> Vec<serde_json::Value> {
    raw_fake_session_with(caps, &[], None)
}

/// [`raw_fake_session`] with extra fake-backend behaviour flags (e.g.
/// `--with-background-message`) and an optional frame kind to wait for
/// AFTER the result (the session stays open until it arrives, so timers
/// get their chance to fire before the shutdown).
fn raw_fake_session_with(
    caps: Option<serde_json::Value>,
    extra_flags: &[&str],
    wait_after_result: Option<&str>,
) -> Vec<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{channel, RecvTimeoutError};

    let mut child = Command::new("python3")
        .arg("tests/fake_backend.py")
        .arg("--workspace")
        .arg(".")
        .args(["--with-usage", "--with-thoughts", "--with-subagents"])
        .args(extra_flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("fake backend spawns");

    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let (tx, rx) = channel::<serde_json::Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) {
                if tx.send(frame).is_err() {
                    break;
                }
            }
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut frames = Vec::new();

    let expect = |frames: &mut Vec<serde_json::Value>, kind: &str| {
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(frame) => {
                    let hit = frame["type"] == kind;
                    frames.push(frame);
                    if hit {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("fake backend never sent a {kind} frame; got {frames:?}");
    };

    let init = match caps {
        Some(c) => serde_json::json!({
            "type": "initialize",
            "protocol_version": 1,
            "capabilities": c
        }),
        None => serde_json::json!({"type": "initialize", "protocol_version": 1}),
    };
    writeln!(stdin, "{init}").expect("initialize written");
    expect(&mut frames, "ready");

    writeln!(
        stdin,
        r#"{{"type":"request","request_id":"raw-1","thread_id":42,"prompt":"gating probe","workspace_root":"."}}"#
    )
    .expect("request written");
    expect(&mut frames, "result");
    if let Some(kind) = wait_after_result {
        expect(&mut frames, kind);
    }

    writeln!(stdin, r#"{{"type":"shutdown"}}"#).expect("shutdown written");
    drop(stdin);
    let _ = child.wait();
    frames
}

/// Post-subagent continuation: the background tool result comes back after
/// the parent request's result frame, and the model's follow-up answer must
/// render as a NEW assistant message in the owning chat (a live-session
/// bug: backend logs proved the answer was produced, yet it never
/// appeared). The fake backend delivers it as an out-of-band `message`
/// frame gated on `capabilities.background_messages`; the session pump
/// forwards it and the chat folds it without touching the request
/// lifecycle, sticky ctx state, or the per-chat thoughts rules.
#[tokio::test]
async fn background_message_frame_renders_continuation_answer() {
    let live = connect(&["--with-background-message", "--with-thoughts"]).await;
    let mut app = app_with_one_chat();
    let (bg_tx, mut bg_rx) = tokio::sync::mpsc::channel(16);
    live.pump_background_messages(bg_tx);

    let submitted = submitted_for(&app, "launch 3 subagents that sleep and report back");
    let events = submit_and_fold(&live, &mut app, submitted).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::Result { .. })),
        "parent turn must complete normally first"
    );
    assert!(
        app.chats[0].lifecycle.request_id().is_none(),
        "parent turn must be fully resolved before the continuation"
    );

    // The continuation arrives ~0.4 s after the request, well after the
    // parent result: the pump must still deliver it to an IDLE chat.
    let bg = tokio::time::timeout(TIMEOUT, bg_rx.recv())
        .await
        .expect("continuation event in time")
        .expect("background pump alive");
    let (markdown, model, thoughts) = match &bg {
        BackendEvent::BackgroundMessage {
            markdown,
            model,
            thoughts,
            ..
        } => (markdown.clone(), model.clone(), thoughts.clone()),
        other => panic!("expected BackgroundMessage, got {other:?}"),
    };
    assert_eq!(
        markdown, "Subagents finished: all three reports are in.",
        "continuation content passes through verbatim"
    );
    assert_eq!(
        model.as_deref(),
        Some("gpt-example"),
        "continuation label resolves from the frame's model field"
    );
    let thoughts = thoughts.expect("continuation thoughts present");
    assert!(
        thoughts.contains("Continuation reasoning trace"),
        "continuation reasoning arrives: {thoughts}"
    );

    let messages_before = app.chats[0].messages.len();
    app.apply_backend_event(bg);

    // A NEW assistant bubble (the parent answer stays untouched), the
    // per-chat thoughts chain EXTENDED by the continuation's reasoning
    // (append, never overwrite), the model label stamped, and NO lifecycle
    // or sticky-usage side effects.
    assert_eq!(
        app.chats[0].messages.len(),
        messages_before + 1,
        "continuation must append a new message, not resolve the old bubble"
    );
    assert_eq!(
        app.chats[0].messages.last().unwrap().markdown,
        "Subagents finished: all three reports are in."
    );
    assert_eq!(
        app.chats[0].last_thoughts.as_deref(),
        Some(
            format!(
                "{}\n{}",
                events
                    .iter()
                    .find_map(|e| match e {
                        BackendEvent::Result {
                            thoughts: Some(t), ..
                        } => Some(t.clone()),
                        _ => None,
                    })
                    .expect("parent turn carries thoughts"),
                thoughts
            )
            .as_str()
        ),
        "continuation reasoning APPENDS to the owning chat's thoughts chain"
    );
    assert_eq!(app.chats[0].last_model.as_deref(), Some("gpt-example"));
    assert_eq!(
        app.last_turn_usage, None,
        "no usage on the frame: ctx untouched"
    );
    assert_eq!(
        app.chats[0].lifecycle.request_id(),
        None,
        "continuation must never resurrect a request lifecycle"
    );

    let flat = render_grid(&mut app).join("\n");
    assert!(
        flat.contains("Subagents finished"),
        "continuation answer must render as a new assistant message:\n{flat}"
    );
    assert!(
        flat.contains("Continuation reasoning trace"),
        "continuation reasoning must render dim above the answer"
    );

    let _ = live.shutdown().await;
}

/// The `message` frame is opt-in: absent without
/// `capabilities.background_messages`, delivered after the result when the
/// capability is declared.
#[test]
fn background_message_frame_is_gated_on_client_capability() {
    let off = raw_fake_session_with(
        Some(serde_json::json!({"thoughts": true, "subagents": true})),
        &["--with-background-message"],
        None,
    );
    assert!(
        !off.iter().any(|f| f["type"] == "message"),
        "no message frame without capabilities.background_messages: {off:?}"
    );

    let on = raw_fake_session_with(
        Some(serde_json::json!({
            "thoughts": true,
            "subagents": true,
            "background_messages": true
        })),
        &["--with-background-message"],
        Some("message"),
    );
    let msg = on
        .iter()
        .find(|f| f["type"] == "message")
        .expect("message frame with the capability declared");
    assert_eq!(
        msg["thread_id"], 42,
        "frame routes by the request's thread id"
    );
    assert_eq!(
        msg["content"],
        "Subagents finished: all three reports are in."
    );
    assert_eq!(msg["model"], "gpt-example");
    assert_eq!(msg["provider"], "openai");
    assert!(
        msg["thoughts"].is_string(),
        "continuation thoughts ride when thoughts are opted in"
    );
    // Wire order: the continuation strictly follows the parent result frame.
    let result_pos = on.iter().position(|f| f["type"] == "result").unwrap();
    let msg_pos = on.iter().position(|f| f["type"] == "message").unwrap();
    assert!(
        result_pos < msg_pos,
        "continuation must arrive after the parent result"
    );
}

// /stop and /reset through the real glue ----

use chibi_tui::app::{Mode, StopResetAction};

/// Fold events from a SHARED channel until both terminal kinds arrived (the
/// killed request's error and the control request's result), applying each
/// through App exactly like the event loop does.
async fn fold_until_both_terminals(
    rx: &mut tokio::sync::mpsc::Receiver<BackendEvent>,
    app: &mut App,
) -> Vec<BackendEvent> {
    let mut events = Vec::new();
    let mut saw_result = false;
    let mut saw_error = false;
    for _ in 0..40 {
        let evt = tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .expect("event in time")
            .expect("channel alive");
        match &evt {
            BackendEvent::Result { .. } => saw_result = true,
            BackendEvent::Error { .. } => saw_error = true,
            _ => {}
        }
        app.apply_backend_event(evt.clone());
        events.push(evt);
        if saw_result && saw_error {
            return events;
        }
    }
    panic!("terminal events never both arrived: {events:?}");
}

/// /stop issued while a request runs: the killed request resolves through
/// its own clean cancelled error, the control request resolves with the
/// backend's feedback text, the dialog STAYS (stop never clears) and the
/// lifecycle returns to idle with no zombie placeholder.
#[tokio::test]
async fn stop_command_kills_running_request_end_to_end() {
    let live = connect(&["--result-delay", "5"]).await;
    let mut app = app_with_one_chat();
    app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let submitted = submitted_for(&app, "long running");
    app.begin_request(&submitted);
    live.submit_encoded(submitted, tx.clone());

    // Drive to the running state before stopping.
    loop {
        let evt = tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .expect("event in time")
            .expect("channel alive");
        let running = matches!(&evt, BackendEvent::Running { .. });
        app.apply_backend_event(evt.clone());
        if running {
            break;
        }
    }
    assert!(app.chats[0].is_busy(), "precondition: request in flight");

    // Stage and send the control request through the App seam (the same
    // out-of-band path the event loop uses — never the busy FIFO).
    app.begin_stop_confirm();
    assert!(matches!(
        app.mode,
        Mode::ConfirmStopReset {
            action: StopResetAction::Stop
        }
    ));
    app.confirm_stop_reset();
    let control = app
        .take_control_submission()
        .expect("stop staged on confirm");
    assert_eq!(control.prompt, "/stop");
    live.submit_encoded(control, tx);

    let events = fold_until_both_terminals(&mut rx, &mut app).await;

    // The chat resolved to idle: no zombie pending state, no spinner leak.
    assert_eq!(
        app.chats[0].lifecycle,
        ChatLifecycle::Idle,
        "the killed request's cancelled error resolved the lifecycle"
    );
    // The dialog STAYS: the user bubble plus the cancelled error bubble.
    let transcript: Vec<&str> = app.chats[0]
        .messages
        .iter()
        .map(|m| m.markdown.as_str())
        .collect();
    assert!(
        transcript.iter().any(|t| t.contains("Cancelled")),
        "the killed request surfaced its cancellation: {transcript:?}"
    );
    assert!(
        !transcript.iter().any(|t| t.contains("Everything stopped.")),
        "the control request's own answer never leaks into the transcript"
    );
    assert!(
        app.status_message
            .as_ref()
            .is_some_and(|(m, _)| m.contains("Everything stopped.")),
        "the backend's feedback text arrived as the toast"
    );
    assert!(
        !events.is_empty(),
        "the fake answered both the kill and the stop"
    );

    let _ = live.shutdown().await;
}

/// /reset confirmed while a request runs: the killed request resolves,
/// the control ack clears the dialog (messages, queue, thoughts, counters)
/// and the lifecycle returns to idle — the local mirror of the backend's
/// thread-scoped history drop.
#[tokio::test]
async fn reset_command_clears_dialog_end_to_end() {
    let live = connect(&["--result-delay", "5"]).await;
    let mut app = app_with_one_chat();
    app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let submitted = submitted_for(&app, "doomed turn");
    app.begin_request(&submitted);
    live.submit_encoded(submitted, tx.clone());
    loop {
        let evt = tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .expect("event in time")
            .expect("channel alive");
        let running = matches!(&evt, BackendEvent::Running { .. });
        app.apply_backend_event(evt.clone());
        if running {
            break;
        }
    }

    // A queued prompt waits behind the running turn; the reset must clear it.
    app.chats[0].queue.push_back("queued survivor?".to_owned());

    app.begin_reset_confirm();
    assert!(matches!(
        app.mode,
        Mode::ConfirmStopReset {
            action: StopResetAction::Reset
        }
    ));
    app.confirm_stop_reset();
    let control = app
        .take_control_submission()
        .expect("reset staged on confirm");
    assert_eq!(control.prompt, "/reset");
    live.submit_encoded(control, tx);

    fold_until_both_terminals(&mut rx, &mut app).await;

    assert_eq!(
        app.chats[0].lifecycle,
        ChatLifecycle::Idle,
        "reset leaves the chat idle"
    );
    assert!(
        app.chats[0].messages.is_empty(),
        "the dialog view is cleared on the reset ack"
    );
    assert!(
        app.chats[0].queue.is_empty(),
        "queued prompts do not survive a reset"
    );
    assert!(app.chats[0].last_thoughts.is_none());
    assert!(
        app.status_message
            .as_ref()
            .is_some_and(|(m, _)| m.contains("Done!")),
        "the backend's ack text arrived as the toast"
    );

    let _ = live.shutdown().await;
}
