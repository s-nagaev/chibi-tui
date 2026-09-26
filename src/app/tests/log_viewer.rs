use super::support::*;

#[test]
fn log_viewer_leaves_chats_and_draft_untouched() {
    let mut app = app_with_chats(2);
    type_in(&mut app, "precious draft");
    // Message is Serialize; Chat itself is not — snapshot names, ids,
    // lifecycle states and message bytes (the state the viewer could
    // possibly disturb).
    let chats_before: Vec<String> = app
        .chats
        .iter()
        .map(|c| {
            serde_json::to_string(&c.messages).unwrap()
                + &format!("|{}|{}|{:?}", c.name, c.id, c.lifecycle)
        })
        .collect();
    app.scroll_up(12);

    app.begin_log_viewer();
    app.log_cursor_up(3);
    app.close_log_viewer();

    let chats_after: Vec<String> = app
        .chats
        .iter()
        .map(|c| {
            serde_json::to_string(&c.messages).unwrap()
                + &format!("|{}|{}|{:?}", c.name, c.id, c.lifecycle)
        })
        .collect();
    assert_eq!(chats_after, chats_before, "viewer is read-only on chats");
    assert_eq!(app.input.lines().join(""), "precious draft");
    assert_eq!(app.scroll, 12, "chat scroll untouched by the modal");
}
