use crate::*;

// ---- per-thread async: submission routing ------------------------------

/// send_submitted routes a bundle to the mock source; LivePlaceholder is
/// rejected gracefully with an error event instead of panicking.
#[tokio::test]
async fn send_submitted_placeholder_emits_error_not_panic() {
    let submitted = chibi_tui::app::Submitted {
        request_id: "r-1".to_owned(),
        thread_id: "t-1".to_owned(),
        prompt: "hello".to_owned(),
    };
    let (tx, mut rx) = mpsc::channel(4);
    let mut placeholder = Source::LivePlaceholder;
    send_submitted(&mut placeholder, &submitted, tx);
    match rx.recv().await.expect("error event") {
        BackendEvent::Error {
            message, thread_id, ..
        } => {
            assert!(message.contains("offline"), "{message}");
            assert_eq!(thread_id.as_deref(), Some("t-1"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn send_submitted_mock_receives_prompt() {
    let submitted = chibi_tui::app::Submitted {
        request_id: "r-2".to_owned(),
        thread_id: "t-2".to_owned(),
        prompt: "mock me".to_owned(),
    };
    let (tx, mut rx) = mpsc::channel(4);
    let mut mock_source = Source::Mock(Box::new(chibi_tui::backend::MockBackend::new()));
    send_submitted(&mut mock_source, &submitted, tx);
    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("in time")
        .expect("event");
    assert!(matches!(evt, BackendEvent::Queued { .. }));
}
