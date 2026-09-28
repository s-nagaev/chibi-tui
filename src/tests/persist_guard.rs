use super::support::*;

use crate::*;

// delta persistence guard --------------------------------

/// The guard decision itself: a streaming `Delta` must never schedule a
/// snapshot write (full-transcript serialize + blocking `fs::write` on the
/// UI loop per chunk), while terminal `Result` / `Error` and every other
/// event kind keep persisting as before.
#[test]
fn should_persist_after_rejects_deltas_only() {
    let delta = BackendEvent::Delta {
        request_id: 1,
        thread_id: "t".into(),
        text: "chunk".into(),
    };
    assert!(
        !should_persist_after(&delta),
        "a delta chunk must not trigger persistence"
    );
    let result = BackendEvent::Result {
        request_id: 1,
        markdown: "answer".into(),
        thread_id: "t".into(),
        model: None,
        usage: None,
        thoughts: None,
    };
    assert!(
        should_persist_after(&result),
        "terminal Result keeps persisting"
    );
    assert!(
        should_persist_after(&BackendEvent::Error {
            request_id: 1,
            message: "boom".into(),
            thread_id: Some("t".into()),
        }),
        "terminal Error keeps persisting"
    );
    assert!(
        should_persist_after(&BackendEvent::Running {
            request_id: 1,
            thread_id: "t".into(),
        }),
        "lifecycle progress keeps persisting"
    );
}

/// End-to-end snapshot contract: applying a `Delta` to a running chat
/// leaves the persisted history file byte-for-byte unchanged; the
/// subsequent terminal `Result` (persisted per the guard) rewrites it with
/// the authoritative answer.
#[test]
fn delta_event_does_not_write_a_history_snapshot() {
    let dir = temp_history_dir("delta-no-persist");
    let mut app = app_with_chats(1);

    // Baseline: the running request is persisted once (as the loop does on
    // `Queued`), then a delta chunk arrives.
    let submitted = submit_text(&mut app, "go");
    let request_id = chibi_tui::live::wire_thread_id(&submitted.request_id) as u64;
    let thread_id = app.chats[0].id.clone();
    assert!(should_persist_after(&BackendEvent::Running {
        request_id,
        thread_id: thread_id.clone(),
    }));
    persist_chat(&app.chats[0], Some(&dir));
    let path = dir.join(format!("threads/{}.json", thread_id));
    assert!(path.exists(), "baseline snapshot exists");
    let before = std::fs::read(&path).expect("baseline readable");

    // Delta flood: several chunks applied through the real app path.
    for text in ["par", "tial ", "text"] {
        let delta = BackendEvent::Delta {
            request_id,
            thread_id: thread_id.clone(),
            text: text.into(),
        };
        assert!(!should_persist_after(&delta));
        app.apply_backend_event(delta);
        // The loop would skip persist here; nothing rewrites the file.
    }
    let after_deltas = std::fs::read(&path).expect("snapshot still readable");
    assert_eq!(
        before, after_deltas,
        "delta chunks must not rewrite the history snapshot"
    );

    // Terminal result: persisted as today, snapshot carries the answer.
    let result = BackendEvent::Result {
        request_id,
        markdown: "partial text answer".into(),
        thread_id: thread_id.clone(),
        model: None,
        usage: None,
        thoughts: None,
    };
    assert!(should_persist_after(&result));
    app.apply_backend_event(result);
    persist_chat(&app.chats[0], Some(&dir));
    let after_result = std::fs::read(&path).expect("snapshot readable after result");
    assert_ne!(
        before, after_result,
        "terminal Result rewrites the snapshot with the authoritative text"
    );

    std::fs::remove_dir_all(&dir).ok();
}
