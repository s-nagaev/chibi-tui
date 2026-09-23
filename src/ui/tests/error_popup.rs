use super::super::*;
use super::support::*;

/// With an error popup set, a bordered box with the message and the
/// recovery hint must be rendered on top of the UI.
#[test]
fn error_popup_renders_message_and_hint_over_ui() {
    let mut app = App::new(mock::initial_chats());
    app.show_error("backend process died unexpectedly");
    let rows = render_grid(&mut app);

    let flat: String = rows.join("\n");
    assert!(
        flat.contains("Backend error"),
        "popup title missing:\n{flat}"
    );
    assert!(
        flat.contains("backend process died"),
        "popup message missing"
    );
    assert!(flat.contains("R reconnect"), "recovery hint missing");

    // Popup is centered and boxed: its top border row exists.
    assert!(
        rows.iter().any(|r| r.contains('┌') && r.contains('┐')),
        "no closed top border found"
    );
}

/// No popup — no overlay artifacts anywhere.
#[test]
fn no_popup_when_dismissed() {
    let mut app = App::new(mock::initial_chats());
    app.show_error("boom");
    app.dismiss_error();
    let flat = render_grid(&mut app).join("\n");
    assert!(!flat.contains("Backend error"));
}
