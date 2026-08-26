//! Integration tests for the request pipeline (task 3b) against the
//! deterministic fake backend (`tests/fake_backend.py`).
//!
//! Every await step is wrapped in a generous timeout so a protocol deadlock
//! fails fast in CI instead of hanging the suite.

use std::time::Duration;

use chibi_tui::backend_client::BackendError;
use chibi_tui::protocol::{CursorPosition, ErrorCode, Selection, StatusState};
use chibi_tui::request_pipeline::{RequestArgs, RequestPipeline};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Connect with extra fake-backend argv flags (tests run with CWD = crate
/// root, so the default `tests/fake_backend.py` resolves).
/// Tests always target the fake backend — set `CHIBI_FAKE_BACKEND` explicitly.
fn fake_pipeline_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("CHIBI_FAKE_BACKEND", "tests/fake_backend.py");
    });
}

async fn connect(extra: &[&str]) -> RequestPipeline {
    fake_pipeline_env();
    let extra: Vec<String> = extra.iter().map(|s| (*s).to_owned()).collect();
    tokio::time::timeout(TIMEOUT, RequestPipeline::connect_with_args(".", 16, &extra))
        .await
        .expect("connect within timeout")
        .expect("connect succeeds")
}

async fn send(
    pipeline: &RequestPipeline,
    args: RequestArgs,
) -> tokio::sync::oneshot::Receiver<Result<chibi_tui::protocol::ServerMessage, BackendError>> {
    tokio::time::timeout(TIMEOUT, pipeline.send_request(args))
        .await
        .expect("send_request within timeout")
        .expect("send_request accepted")
}

/// Wait for the next status for `request_id` on the subscription.
async fn next_status_for(
    sub: &mut tokio::sync::broadcast::Receiver<chibi_tui::request_pipeline::StatusUpdate>,
    request_id: &str,
) -> chibi_tui::request_pipeline::StatusUpdate {
    loop {
        let update = tokio::time::timeout(TIMEOUT, sub.recv())
            .await
            .expect("status within timeout")
            .expect("status channel alive");
        if update.request_id.0 == request_id {
            return update;
        }
    }
}

/// Drain statuses for `request_id` until it reaches `target`. The protocol
/// always emits `queued` before `running`, so waiting for a later state means
/// consuming the earlier ones — never asserting on the first frame.
async fn wait_for_state(
    sub: &mut tokio::sync::broadcast::Receiver<chibi_tui::request_pipeline::StatusUpdate>,
    request_id: &str,
    target: StatusState,
) -> chibi_tui::request_pipeline::StatusUpdate {
    loop {
        let update = next_status_for(sub, request_id).await;
        if update.state == target {
            return update;
        }
    }
}

#[tokio::test]
async fn valid_session_status_then_result_correlated() {
    let pipeline = connect(&[]).await;
    let mut sub = pipeline.subscribe_status();

    let rx = send(
        &pipeline,
        RequestArgs::new("01HXY9K1ABCDEFGH", 3, "Explain what this function does.")
            .active_file("/home/user/projects/demo/src/main.py")
            .selection(Selection {
                start_line: 10,
                end_line: 22,
                text: "def foo():\n    return 42".to_owned(),
            })
            .cursor_position(CursorPosition {
                line: 15,
                character: 8,
            })
            .language_id("python"),
    )
    .await;

    // Exactly Queued then Running, in that order.
    let queued = next_status_for(&mut sub, "01HXY9K1ABCDEFGH").await;
    assert_eq!(queued.state, StatusState::Queued);
    let running = next_status_for(&mut sub, "01HXY9K1ABCDEFGH").await;
    assert_eq!(running.state, StatusState::Running);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), sub.recv())
            .await
            .is_err(),
        "no further statuses expected"
    );

    // Final outcome on the per-request receiver.
    let outcome = tokio::time::timeout(TIMEOUT, rx)
        .await
        .expect("result within timeout")
        .expect("final frame present");
    match outcome.expect("request succeeds") {
        chibi_tui::protocol::ServerMessage::Result {
            request_id,
            content,
            model,
            provider,
        } => {
            assert_eq!(request_id, "01HXY9K1ABCDEFGH");
            assert!(content.contains("42"), "content: {content}");
            assert_eq!(model.as_deref(), Some("gpt-example"));
            assert_eq!(provider.as_deref(), Some("openai"));
        }
        other => panic!("expected Result, got: {other:?}"),
    }

    // Map cleanup proof: the same id is free again and gets served again.
    let rx2 = send(
        &pipeline,
        RequestArgs::new("01HXY9K1ABCDEFGH", 3, "And once more?"),
    )
    .await;
    let _queued2 = next_status_for(&mut sub, "01HXY9K1ABCDEFGH").await;
    let _running2 = next_status_for(&mut sub, "01HXY9K1ABCDEFGH").await;
    let outcome2 = tokio::time::timeout(TIMEOUT, rx2)
        .await
        .expect("second result within timeout")
        .expect("final frame present");
    assert!(outcome2.is_ok());

    pipeline.shutdown().await.unwrap();
}

#[tokio::test]
async fn valid_cancel_resolves_only_target() {
    let pipeline = connect(&[]).await;

    let rx = send(
        &pipeline,
        RequestArgs::new("01HXY9K2CANCELL0", 1, "Long running task"),
    )
    .await;

    // Prove it is actually in flight before cancelling. `queued` precedes
    // `running` by protocol design — drain until the target state.
    let mut sub = pipeline.subscribe_status();
    let running = wait_for_state(&mut sub, "01HXY9K2CANCELL0", StatusState::Running).await;
    assert_eq!(running.state, StatusState::Running);

    tokio::time::timeout(TIMEOUT, pipeline.cancel("01HXY9K2CANCELL0"))
        .await
        .expect("cancel call within timeout")
        .expect("cancel accepted");

    let outcome = tokio::time::timeout(TIMEOUT, rx)
        .await
        .expect("cancelled answer within timeout")
        .expect("final frame present");
    match outcome {
        Err(BackendError::RequestFailed { code, .. }) => {
            assert_eq!(code, ErrorCode::Cancelled)
        }
        other => panic!("expected Cancelled RequestFailed, got: {other:?}"),
    }

    // Negative control: unknown id rejected locally without a wire round trip.
    let err = tokio::time::timeout(TIMEOUT, pipeline.cancel("01HXY9K6NOPE0000"))
        .await
        .expect("negative cancel within timeout")
        .expect_err("unknown cancel must fail");
    assert!(
        matches!(
            err,
            BackendError::UnknownRequest { ref request_id } if request_id == "01HXY9K6NOPE0000"
        ),
        "got: {err:?}"
    );

    pipeline.shutdown().await.unwrap();
}

#[tokio::test]
async fn valid_shutdown_graceful_exit() {
    let pipeline = connect(&[]).await;
    tokio::time::timeout(TIMEOUT, pipeline.shutdown())
        .await
        .expect("shutdown within timeout")
        .expect("graceful exit 0");
}

#[tokio::test]
async fn garbage_server_output_does_not_kill_pipeline() {
    let pipeline = connect(&["--garbage-on-start"]).await;
    let mut sub = pipeline.subscribe_status();

    let rx = send(
        &pipeline,
        RequestArgs::new("01GARBAGEPROOF001", 7, "short prompt"),
    )
    .await;

    let queued = next_status_for(&mut sub, "01GARBAGEPROOF001").await;
    assert_eq!(queued.state, StatusState::Queued);
    let running = next_status_for(&mut sub, "01GARBAGEPROOF001").await;
    assert_eq!(running.state, StatusState::Running);

    let outcome = tokio::time::timeout(TIMEOUT, rx)
        .await
        .expect("result within timeout despite garbage line")
        .expect("final frame present");
    assert!(outcome.is_ok(), "got: {outcome:?}");

    pipeline.shutdown().await.unwrap();
}

#[tokio::test]
async fn broken_pipe_fails_pending_and_reconnect_restores() {
    let mut pipeline = connect(&["--crash-after-running"]).await;
    let mut sub = pipeline.subscribe_status();

    let rx = send(
        &pipeline,
        RequestArgs::new("01BRKPIPE00000001", 1, "this will crash the backend"),
    )
    .await;

    let running = wait_for_state(&mut sub, "01BRKPIPE00000001", StatusState::Running).await;
    assert_eq!(running.state, StatusState::Running);

    // Backend os._exit(69)ed right after `running`: the reader must notice
    // EOF/Died and fail all pending with Broken.
    let outcome = tokio::time::timeout(TIMEOUT, rx)
        .await
        .expect("broken-pipe failure within timeout")
        .expect("final frame present");
    assert!(
        matches!(&outcome, Err(BackendError::Broken(_))),
        "expected Broken, got: {outcome:?}"
    );

    // New requests are refused while broken (and their receivers resolved).
    let err = tokio::time::timeout(
        TIMEOUT,
        pipeline.send_request(RequestArgs::new("01BRKPIPEDENIED01", 1, "nope")),
    )
    .await
    .expect("send while broken within timeout")
    .expect_err("broken pipeline refuses new requests");
    assert!(matches!(err, BackendError::Broken(_)), "got: {err:?}");

    // Reconnect restores full service.
    tokio::time::timeout(TIMEOUT, pipeline.reconnect())
        .await
        .expect("reconnect within timeout")
        .expect("reconnect succeeds");

    let rx2 = send(
        &pipeline,
        RequestArgs::new("01BRKPIPE00000002", 2, "after reconnect"),
    )
    .await;
    let queued = next_status_for(&mut sub, "01BRKPIPE00000002").await;
    assert_eq!(queued.state, StatusState::Queued);
    let running2 = next_status_for(&mut sub, "01BRKPIPE00000002").await;
    assert_eq!(running2.state, StatusState::Running);

    let outcome2 = tokio::time::timeout(TIMEOUT, rx2)
        .await
        .expect("post-reconnect result within timeout")
        .expect("final frame present");
    assert!(outcome2.is_ok(), "got: {outcome2:?}");

    tokio::time::timeout(TIMEOUT, pipeline.shutdown())
        .await
        .expect("cleanup shutdown within timeout")
        .expect("graceful shutdown after reconnect");
}

#[tokio::test]
async fn status_channel_is_broadcast_not_oneshot() {
    let pipeline = connect(&[]).await;
    let mut sub_a = pipeline.subscribe_status();
    let mut sub_b = pipeline.subscribe_status();

    let rx = send(
        &pipeline,
        RequestArgs::new("01FANOUTCHECK0001", 4, "fan-out check"),
    )
    .await;

    for sub in [&mut sub_a, &mut sub_b] {
        let queued = next_status_for(sub, "01FANOUTCHECK0001").await;
        assert_eq!(queued.state, StatusState::Queued);
        let running = next_status_for(sub, "01FANOUTCHECK0001").await;
        assert_eq!(running.state, StatusState::Running);
    }

    let outcome = tokio::time::timeout(TIMEOUT, rx)
        .await
        .expect("result within timeout")
        .expect("final frame present");
    // The statuses were pure fan-out; the final outcome still lands here.
    assert!(matches!(
        outcome,
        Ok(chibi_tui::protocol::ServerMessage::Result { .. })
    ));

    pipeline.shutdown().await.unwrap();
}
