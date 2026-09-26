use super::super::*;
use super::support::*;

// popup rendering -----------------------------

use crate::app::{ModelPickerPhase, ModelPickerState};

/// A Ready picker over the REAL captured listing (104 rows) with the
/// selection parked on `selected` (0-based).
fn picker_app_at(selected: usize) -> App {
    let mut app = App::new(mock::initial_chats());
    let entries = crate::model_picker::parse_model_listing(include_str!(
        "../../../tests/fixtures/model_listing_captured.txt"
    ));
    assert_eq!(entries.len(), 104);
    app.mode = Mode::ModelPicking {
        state: ModelPickerState {
            phase: ModelPickerPhase::Ready,
            entries,
            selected,
        },
    };
    app
}

#[test]
fn model_picker_popup_renders_rows_hint_and_selection_marker() {
    let mut app = picker_app_at(0);
    let rows = render_grid(&mut app);
    let text: String = rows.join("\n");
    assert!(text.contains("Model picker"), "popup title");
    assert!(text.contains("104 models"), "row count in the title");
    assert!(
        text.contains("1. Qwen3.8 Max (Alibaba)"),
        "first parsed row rendered: {text}"
    );
    assert!(
        text.contains("↑↓ navigate · PgUp/PgDn page · Enter switch · Esc close · Ctrl+C quit"),
        "decision hint on the footer row"
    );
    // The highlighted row carries the ▸ marker.
    let marked = rows
        .iter()
        .any(|r| r.contains('▸') && r.contains("1. Qwen3.8 Max"));
    assert!(marked, "selection marker on the highlighted row");
}

#[test]
fn model_picker_list_scrolls_and_the_selection_stays_on_screen() {
    // Selection on the LAST of 104 rows while the body shows only
    // ~12 rows: the ratatui selection-aware list must auto-scroll the
    // highlighted row into view.
    let mut app = picker_app_at(103);
    let rows = render_grid_at_with_buffer(&mut app, 120, 34).0;
    let text: String = rows.join("\n");
    assert!(
        text.contains("104. GLM 4.7 FlashX (ZhipuAI)"),
        "the selected last row auto-scrolled into view"
    );
    assert!(
        !text.contains("1. Qwen3.8 Max (Alibaba)"),
        "early rows scrolled out of the viewport"
    );

    // And back to the top.
    let mut app = picker_app_at(0);
    let rows = render_grid_at_with_buffer(&mut app, 120, 34).0;
    let text: String = rows.join("\n");
    assert!(text.contains("1. Qwen3.8 Max (Alibaba)"));
    assert!(!text.contains("104. GLM 4.7 FlashX (ZhipuAI)"));
}

#[test]
fn model_picker_loading_phase_renders_a_quiet_placeholder() {
    let mut app = App::new(mock::initial_chats());
    app.begin_model_picker();
    assert!(matches!(app.mode, Mode::ModelPicking { .. }));
    let rows = render_grid(&mut app);
    let text: String = rows.join("\n");
    assert!(text.contains("loading models…"), "in-flight placeholder");
    assert!(
        text.contains("Model picker"),
        "the popup family title renders during the fetch too"
    );
    assert!(
        !text.contains("1. Qwen3.8 Max"),
        "no rows exist before the hidden listing resolves"
    );
}

#[test]
fn picker_render_feeds_the_page_size_seam() {
    let mut app = picker_app_at(0);
    render_grid(&mut app);
    assert_eq!(
        app.picker_visible_rows, 12,
        "the page size is the popup's actual visible row count"
    );

    let mut app = picker_app_at(0);
    render_grid_at_with_buffer(&mut app, 120, 8);
    assert_eq!(
        app.picker_visible_rows, 3,
        "a tiny terminal shrinks the page to the real viewport"
    );
}
