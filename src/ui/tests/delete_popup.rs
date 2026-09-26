use super::super::*;
use super::support::*;

// render-level checks -------------------------

/// The Ctrl+D confirm popup renders the destructive title, the active
/// thread's title and the decision hint inside a bordered box.
#[test]
fn delete_popup_renders_title_thread_and_hint() {
    let mut app = App::new(vec![Chat::new("Deep Dive")]);
    app.begin_delete_confirm();
    let rows = render_grid(&mut app);
    let flat: String = rows.join("\n");

    assert!(
        flat.contains("Delete thread"),
        "popup title missing:\n{flat}"
    );
    assert!(flat.contains("Deep Dive"), "thread title missing:\n{flat}");
    assert!(flat.contains("y/Enter confirm"), "confirm hint missing");
    assert!(flat.contains("Esc/n cancel"), "cancel hint missing");
    // Boxed: a closed top border row exists.
    assert!(rows.iter().any(|r| r.contains('┌') && r.contains('┐')));
}

/// The popup is centered: identical left/right margins on its top row.
#[test]
fn delete_popup_is_centered() {
    let mut app = App::new(vec![Chat::new("chat")]);
    app.begin_delete_confirm();
    let (_, buf) = render_grid_with_buffer(&mut app);

    let top = (0..buf.area.height)
        .find(|&y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "┌"))
        .expect("popup top border");
    let left = (0..buf.area.width)
        .position(|x| buf[(x, top)].symbol() == "┌")
        .unwrap() as u16;
    let right = (0..buf.area.width)
        .rposition(|x| buf[(x, top)].symbol() == "┐")
        .unwrap() as u16;
    assert_eq!(left, buf.area.width - 1 - right, "popup not centered");
    // ~50% width at the demo resolution: ≥ half the frame.
    let width = right - left + 1;
    assert!(
        width >= buf.area.width / 2,
        "popup narrower than half the frame"
    );
}

/// No popup in Normal mode — no overlay artifacts.
#[test]
fn no_delete_popup_when_closed() {
    let mut app = App::new(vec![Chat::new("chat")]);
    let flat = render_grid(&mut app).join("\n");
    assert!(!flat.contains("Delete thread"));
}

/// Deleting the last chat renders the clean empty state without
/// panicking: header, sidebar and placeholder input all survive.
#[test]
fn empty_state_after_deleting_last_chat_renders_cleanly() {
    let mut app = App::new(vec![Chat::new("solo")]);
    app.begin_delete_confirm();
    app.confirm_delete();
    assert!(app.chats.is_empty());

    let rows = render_grid(&mut app);
    let input_row = &rows[rows.len() - 2];
    assert!(
        input_row.contains("Type a message"),
        "placeholder input must show in the empty state: {input_row:?}"
    );
    assert!(rows.last().unwrap().contains("^N"), "hints intact");
}
