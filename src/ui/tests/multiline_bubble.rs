use super::super::*;
use super::support::*;

// ---- multi-line user bubble (THE glue regression) ----------------------

/// THE owner-reported regression: a multi-line USER message rendered in
/// the chat bubble GLUED into one physical row (`тест5тест6`). The wire
/// and storage paths are verified correct — this pins the DISPLAY path
/// end to end through `ui::draw` into a TestBackend.
#[test]
fn multiline_user_bubble_renders_each_line_on_its_own_row() {
    let mut app = App::new(vec![Chat::new("ml")]);
    app.chats[0].messages.push(Message::user("тест5\nтест6"));

    let rows = render_grid(&mut app);
    let y5 = rows
        .iter()
        .position(|r| r.contains("тест5"))
        .expect("first draft line must render");
    let y6 = rows
        .iter()
        .position(|r| r.contains("тест6"))
        .expect("second draft line must render");
    assert!(
        !rows.iter().any(|r| r.contains("тест5тест6")),
        "the two lines must not glue onto one row"
    );
    assert_eq!(
        y6,
        y5 + 1,
        "single-\\n lines must occupy ADJACENT rows: y5={y5}, y6={y6}"
    );
}

/// Blank line between prompt lines (`тест1\n\nтест2`): three physical
/// rows — the two lines intact with the blank separation preserved.
#[test]
fn blank_line_user_bubble_keeps_the_blank_row() {
    let mut app = App::new(vec![Chat::new("ml")]);
    app.chats[0].messages.push(Message::user("тест1\n\nтест2"));

    let rows = render_grid(&mut app);
    let y1 = rows
        .iter()
        .position(|r| r.contains("тест1"))
        .expect("first paragraph must render");
    let y2 = rows
        .iter()
        .position(|r| r.contains("тест2"))
        .expect("second paragraph must render");
    assert!(
        !rows.iter().any(|r| r.contains("тест1тест2")),
        "paragraphs must not glue onto one row"
    );
    assert_eq!(
        y2,
        y1 + 2,
        "blank-line paragraphs need an empty row between them: y1={y1}, y2={y2}"
    );
}

/// Assistant answers with blank-line-separated paragraphs go through the
/// same parser — they must not glue either (no regression).
#[test]
fn assistant_blank_line_paragraphs_render_on_separate_rows() {
    let mut app = App::new(vec![Chat::new("ml")]);
    app.chats[0]
        .messages
        .push(Message::assistant("alpha para\n\nbeta para"));

    let rows = render_grid(&mut app);
    let ya = rows
        .iter()
        .position(|r| r.contains("alpha para"))
        .expect("first assistant paragraph must render");
    let yb = rows
        .iter()
        .position(|r| r.contains("beta para"))
        .expect("second assistant paragraph must render");
    assert!(!rows
        .iter()
        .any(|r| r.contains("alphaparabeta") || r.contains("alpha parabeta para")));
    assert_eq!(yb, ya + 2, "assistant paragraphs keep their blank row");
}

/// Geometry consistency: the selection hit-test metadata is built from
/// the renderer's OWN wrap, so its per-row plain text must match the
/// painted cells — the glued row would also glue the SELECTION copy.
#[test]
fn geometry_row_text_matches_the_painted_multiline_bubble() {
    let mut app = App::new(vec![Chat::new("ml")]);
    app.chats[0].messages.push(Message::user("тест5\nтест6"));
    let (rows, _) = render_grid_with_buffer(&mut app);
    let _ = rows; // grid already rendered; geometry cached on the app

    let geom = app.chat_geometry.clone().expect("geometry cached");
    let texts: Vec<&str> = geom.rows.iter().map(|m| m.text.as_str()).collect();
    assert!(
        texts.contains(&"тест5") && texts.contains(&"тест6"),
        "both draft lines must appear as their own geometry rows: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("тест5тест6")),
        "no geometry row may carry the glued text: {texts:?}"
    );

    // Selection copy across the two lines: newline-SEPARATED, never glued.
    let r5 = geom.rows.iter().position(|m| m.text == "тест5").unwrap();
    let r6 = geom.rows.iter().position(|m| m.text == "тест6").unwrap();
    app.selection = Some(crate::app::ChatSelection {
        anchor: crate::app::SelectionPoint { row: r5, col: 0 },
        head: crate::app::SelectionPoint {
            row: r6,
            col: "тест6".chars().count(),
        },
        dragging: false,
    });
    assert_eq!(
        app.selection_text().as_deref(),
        Some("тест5\nтест6"),
        "selection across the bubble's lines must keep the newline"
    );
}
