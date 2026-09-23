use crate::app::*;

/// Regression test for the review finding: PgUp must move one page up
/// from the bottom, not to the top.
#[test]
fn pgup_moves_one_page_up_from_bottom() {
    let mut app = App::new(Vec::new());
    app.scroll_up(20);
    assert_eq!(app.scroll, 20, "first PgUp detaches by one page");
    app.scroll_up(20);
    assert_eq!(app.scroll, 40, "subsequent PgUps move further up");
    app.scroll_down(20);
    assert_eq!(app.scroll, 20, "PgDn moves back down");
    app.scroll_down(20);
    assert_eq!(app.scroll, 0, "returning to 0 re-enables follow-bottom");
    assert!(app.at_bottom());
}

#[test]
fn scroll_up_clamps_at_ceiling() {
    let mut app = App::new(Vec::new());
    app.scroll_up(u16::MAX);
    assert_eq!(app.scroll, u16::MAX / 2);
}

// ---- helpers ---------------------------------------------------------
