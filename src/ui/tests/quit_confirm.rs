use super::super::*;
use super::support::*;

/// The quit-confirmation banner: compact rounded box, title embedded in
/// the top border, question + amber hints inside, exactly 4 rows at
/// demo resolution, width hugging its content (no 50%-frame stretch).
#[test]
fn quit_confirm_renders_as_compact_rounded_banner() {
    let theme = Theme::tokyo_night();
    let mut app = App::new(vec![Chat::new("quit")]);
    app.quit_confirm = true;
    let (rows, buf) = render_grid_at_with_buffer(&mut app, 80, 24);

    // Geometry: width 30 (max content line 25 + padding/borders, above
    // the 30-column floor), height 4, centered at 80×24. The chat pane
    // is a rounded rectangle too, so the banner rows are identified by
    // the banner's own " Quit " title, not by the ╭ glyph alone.
    let banner: Vec<&String> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| (r.contains('╭') && r.contains(" Quit ")) || r.contains("Quit chibi-tui?"))
        .map(|(_, r)| r)
        .collect();
    assert_eq!(banner.len(), 2, "banner spans top border + question rows");
    let top_y = rows
        .iter()
        .position(|r| r.contains('╭') && r.contains(" Quit "))
        .expect("rounded top border");
    assert_eq!(top_y, 10, "vertically centered");
    // The banner is centered, so the row carries 25 columns of the UI
    // beneath it before the border — slice the banner off the row by
    // chars (the rounded corners are multi-byte).
    let chars: Vec<char> = rows[top_y].chars().collect();
    let bx = chars.iter().position(|&c| c == '╭').unwrap();
    let top: String = chars[bx..bx + 30].iter().collect();
    assert_eq!(top.chars().count(), 30, "banner hugs its content width");
    assert!(
        top.starts_with('╭') && top.ends_with('╮'),
        "rounded corners"
    );
    assert!(top.contains(" Quit "), "title embedded in the top border");
    let bottom: String = rows[top_y + 3].chars().skip(bx).take(30).collect();
    assert_eq!(bottom.chars().count(), 30);
    assert!(
        bottom.starts_with('╰') && bottom.ends_with('╯'),
        "rounded bottom corners"
    );
    assert!(
        rows[top_y + 1].contains("│ Quit chibi-tui?"),
        "question row with 1-space padding"
    );
    assert!(
        rows[top_y + 2].contains("y/Enter quit · Esc/n stay"),
        "hints row unchanged"
    );

    // Title + borders red (bold title), question off-white, hints amber.
    let title_x = rows[top_y].find(" Quit ").unwrap() as u16 + 1;
    let title_cell = buf[(title_x, top_y as u16)].clone();
    assert_eq!(title_cell.fg, theme.red, "title in theme.red");
    assert!(title_cell.modifier.contains(Modifier::BOLD), "bold title");
    let border_cell = buf[(bx as u16, top_y as u16)].clone();
    assert_eq!(border_cell.fg, theme.red, "border in theme.red");
    let question_x = rows[top_y + 1].find("Quit chibi-tui?").unwrap() as u16;
    let question_cell = buf[(question_x, (top_y + 1) as u16)].clone();
    assert_eq!(question_cell.fg, theme.fg, "question in theme.fg");
    let hint_x = rows[top_y + 2].find("y/Enter quit").unwrap() as u16;
    let hint_cell = buf[(hint_x, (top_y + 2) as u16)].clone();
    assert_eq!(hint_cell.fg, theme.yellow, "hints in theme.yellow");
}
