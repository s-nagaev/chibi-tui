use super::super::*;
use super::support::*;

//------------------------------------------------------------------------

/// The help modal renders the picker-family visual language (centered
/// bordered box, blue border, bold title, yellow hint footer) and, swept
/// across its scroll pages, EVERY row of the const table. One frame
/// cannot hold all rows by design — the body is a scrolled window — so
/// the sweep walks the clamped page offsets and unions the frames.
#[test]
fn help_modal_renders_every_table_row_across_scroll_pages() {
    let mut app = App::new(mock::initial_chats());
    app.begin_help_modal();
    let total = help_modal_total_lines();
    let page = {
        render_grid(&mut app);
        app.help_visible_rows as usize
    };
    assert!(page > 0 && page < total, "small viewport must paginate");

    let mut seen = String::new();
    let mut scroll = 0;
    loop {
        if let Mode::HelpViewing { state } = &mut app.mode {
            state.scroll = scroll;
        }
        let rows = render_grid(&mut app).join("\n");
        seen.push_str(&rows);
        seen.push('\n');
        if scroll + page >= total {
            break;
        }
        scroll += page;
    }

    assert!(seen.contains(" Keybindings "), "title missing");
    assert!(seen.contains("F1/Esc close"), "hint footer missing");
    for row in HOTKEY_ROWS {
        assert!(
            seen.contains(row.chord),
            "chord {:?} never rendered",
            row.chord
        );
        assert!(
            seen.contains(row.action),
            "action {:?} never rendered",
            row.action
        );
    }
    // Group headers render once per group at the group's first row.
    assert!(seen.contains("Global"));
    assert!(seen.contains("Help (F1)"));
}

/// On a small frame the modal is a scrolled window: the top row is only
/// visible at scroll 0 and the table's last row only after paging to the
/// bottom clamp, with the render-fed viewport reported back to the App
/// seam that PgUp/PgDn page by.
#[test]
fn help_modal_scrolls_when_the_table_exceeds_the_viewport() {
    let mut app = App::new(mock::initial_chats());
    app.begin_help_modal();
    // 14 rows tall: body = 14 - 4 (hint+borders+title slack) leaves
    // a handful of visible rows — far fewer than the table.
    let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 14);
    let joined = rows.join("\n");
    let page = app.help_visible_rows as usize;
    assert!(page < help_modal_total_lines(), "must paginate");
    assert!(
        joined.contains("open / close this keybindings help"),
        "top row visible at scroll 0: {joined}"
    );
    assert!(
        !joined.contains("copy the cursor line"),
        "bottom rows hidden at scroll 0"
    );

    // Page to the bottom clamp: the last row surfaces, the first row
    // scrolls away.
    let total = help_modal_total_lines();
    for _ in 0..(total / page + 2) {
        app.help_page_down();
    }
    if let Mode::HelpViewing { state } = &mut app.mode {
        assert_eq!(state.scroll, total - page, "bottom clamp");
    }
    let (rows, _) = render_grid_at_with_buffer(&mut app, 120, 14);
    let joined = rows.join("\n");
    assert!(
        joined.contains("Help (F1)") && joined.contains("scroll the list"),
        "last rows visible at the bottom clamp: {joined}"
    );
    assert!(
        !joined.contains("open / close this keybindings help"),
        "top row scrolled away"
    );
}

/// The renderer opens a group header on every group CHANGE; duplicated
/// or interleaved groups would render phantom headers and skew the
/// total-line count the scroll clamps use.
#[test]
fn help_table_groups_are_contiguous() {
    let mut seen = std::collections::HashSet::new();
    let mut prev = "";
    for row in HOTKEY_ROWS {
        if row.group != prev {
            assert!(
                seen.insert(row.group),
                "group {:?} appears twice (non-contiguous)",
                row.group
            );
            prev = row.group;
        }
    }
}
