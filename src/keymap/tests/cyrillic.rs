use crate::keymap::{cyrillic_latin_counterpart, normalize_cyrillic_ctrl_chord};
use crate::tests::support::*;

use super::stop_reset::busy_app_with_commands;

use crossterm::event::{KeyCode, KeyModifiers};

// ---- Cyrillic Ctrl-chord normalization -----------------------------------

/// The full Russian (ЙЦУКЕН) letter mapping, case-preserving on both
/// sides: lowercase layout char → lowercase Latin twin, uppercase →
/// uppercase (the case-sensitive `Char('L')` reset shape depends on it).
#[test]
fn cyrillic_table_maps_the_full_russian_layout_case_preserving() {
    let ru: &[(char, char)] = &[
        ('й', 'q'),
        ('ц', 'w'),
        ('у', 'e'),
        ('к', 'r'),
        ('е', 't'),
        ('н', 'y'),
        ('г', 'u'),
        ('ш', 'i'),
        ('щ', 'o'),
        ('з', 'p'),
        ('ф', 'a'),
        ('ы', 's'),
        ('в', 'd'),
        ('а', 'f'),
        ('п', 'g'),
        ('р', 'h'),
        ('о', 'j'),
        ('л', 'k'),
        ('д', 'l'),
        ('ь', 'm'),
        ('я', 'z'),
        ('ч', 'x'),
        ('с', 'c'),
        ('м', 'v'),
        ('и', 'b'),
        ('т', 'n'),
    ];
    for &(cyr, lat) in ru {
        assert_eq!(cyrillic_latin_counterpart(cyr), Some(lat), "{cyr}");
        let upper = cyr.to_uppercase().next().unwrap_or(cyr);
        assert_eq!(
            cyrillic_latin_counterpart(upper),
            Some(lat.to_ascii_uppercase()),
            "{upper}"
        );
    }
}

/// Ukrainian-only keys: і shares the Latin `s` key with the Russian ы,
/// ї / є / ґ sit on the `]` / `'` / `\` keys of the layout.
#[test]
fn cyrillic_table_covers_the_ukrainian_only_keys() {
    assert_eq!(cyrillic_latin_counterpart('і'), Some('s'));
    assert_eq!(cyrillic_latin_counterpart('І'), Some('S'));
    assert_eq!(cyrillic_latin_counterpart('ї'), Some(']'));
    assert_eq!(cyrillic_latin_counterpart('Ї'), Some(']'));
    assert_eq!(cyrillic_latin_counterpart('є'), Some('\''));
    assert_eq!(cyrillic_latin_counterpart('Є'), Some('\''));
    assert_eq!(cyrillic_latin_counterpart('ґ'), Some('\\'));
    assert_eq!(cyrillic_latin_counterpart('Ґ'), Some('\\'));
}

/// Normalization touches ONLY Ctrl-chords: characters outside both
/// layouts pass through unchanged, and events without CONTROL — plain
/// typing, Alt-decorated keys — are never rewritten.
#[test]
fn cyrillic_normalization_touches_ctrl_chords_only_and_passes_unknown_through() {
    let norm = |c: char, mods: KeyModifiers| {
        normalize_cyrillic_ctrl_chord(key_event(KeyCode::Char(c), mods))
    };
    // Lowercase and uppercase shapes, modifiers preserved.
    let key = norm('ф', KeyModifiers::CONTROL);
    assert_eq!(key.code, KeyCode::Char('a'));
    assert!(key.modifiers.contains(KeyModifiers::CONTROL));
    let key = norm('Д', KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    assert_eq!(key.code, KeyCode::Char('L'));
    assert!(key.modifiers.contains(KeyModifiers::SHIFT));
    // Characters outside both layouts pass through unchanged…
    assert_eq!(norm('λ', KeyModifiers::CONTROL).code, KeyCode::Char('λ'));
    assert_eq!(norm('q', KeyModifiers::CONTROL).code, KeyCode::Char('q'));
    // …and so does EVERYTHING without CONTROL.
    assert_eq!(norm('ф', KeyModifiers::NONE).code, KeyCode::Char('ф'));
    assert_eq!(norm('ф', KeyModifiers::ALT).code, KeyCode::Char('ф'));
}

#[test]
fn cyrillic_ctrl_s_and_ukrainian_ctrl_i_toggle_thoughts() {
    let mut app = app_with_chats(1);
    assert!(app.thoughts_visible);
    // ы is the ЙЦУКЕН twin of the Latin `s` key (see the table test)…
    press(&mut app, KeyCode::Char('ы'), KeyModifiers::CONTROL);
    assert!(!app.thoughts_visible, "Ctrl+ы (RU) toggles like Ctrl+S");
    // …і its Ukrainian counterpart.
    press(&mut app, KeyCode::Char('і'), KeyModifiers::CONTROL);
    assert!(app.thoughts_visible, "Ctrl+і (UA) toggles like Ctrl+S too");
}

/// Layout-equivalence: a Cyrillic chord must leave the app in EXACTLY
/// the state its Latin original would — including the uppercase
/// Shift-decorated shapes, which the editor treats as unknown ctrl
/// combos (a no-op here) in BOTH flavors.
#[test]
fn cyrillic_chords_reach_the_identical_state_as_their_latin_originals() {
    // Lowercase Ctrl+A vs Ctrl+ф (the ЙЦУКЕН `a`-position key): caret
    // to the line head in both.
    let mut cyr = app_with_chats(1);
    type_in(&mut cyr, "abcdef");
    press(&mut cyr, KeyCode::Char('ф'), KeyModifiers::CONTROL);
    let mut lat = app_with_chats(1);
    type_in(&mut lat, "abcdef");
    press(&mut lat, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(cyr.input.cursor(), lat.input.cursor(), "lowercase shape");
    assert_eq!(cyr.input.cursor(), (0, 0), "Ctrl+A semantics reached");

    // Uppercase Shift-decorated Ctrl+А vs Ctrl+Shift+A: identical
    // (byte-for-byte the same normalized event, same no-op outcome).
    let mut cyr = app_with_chats(1);
    type_in(&mut cyr, "abcdef");
    press(
        &mut cyr,
        KeyCode::Char('А'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    let mut lat = app_with_chats(1);
    type_in(&mut lat, "abcdef");
    press(
        &mut lat,
        KeyCode::Char('A'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert_eq!(cyr.input.cursor(), lat.input.cursor(), "uppercase shape");
    assert_eq!(
        cyr.input.lines(),
        lat.input.lines(),
        "no keystroke rewritten beyond the layout translation"
    );
}

/// The task's headline example: Ctrl+Ф (the ЙЦУКЕН key on the Latin `a`
/// position) reaches the textarea's readline head-of-line mapping like
/// Ctrl+A does.
#[test]
fn cyrillic_ctrl_f_moves_the_caret_to_line_head_like_ctrl_a() {
    let mut app = app_with_chats(1);
    for ch in "abc".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Char('ф'), KeyModifiers::CONTROL);
    assert_eq!(app.input.cursor(), (0, 0), "Ctrl+ф behaves like Ctrl+A");
}

/// The case-sensitive stop/reset split survives the translation:
/// lowercase д → the plain ^L stop confirm, uppercase Д → the
/// `Char('L')` reset shape (Shift+Ctrl+L).
#[test]
fn cyrillic_stop_and_reset_chords_keep_their_case_semantics() {
    let mut app = busy_app_with_commands(&["/stop", "/reset"]);
    press(&mut app, KeyCode::Char('д'), KeyModifiers::CONTROL);
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Stop
        }
    ));

    let mut app = busy_app_with_commands(&["/stop", "/reset"]);
    press(
        &mut app,
        KeyCode::Char('Д'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(matches!(
        app.mode,
        chibi_tui::app::Mode::ConfirmStopReset {
            action: chibi_tui::app::StopResetAction::Reset
        }
    ));
}

#[test]
fn cyrillic_ctrl_c_cancels_the_inflight_request_like_latin_ctrl_c() {
    let mut app = app_with_chats(1);
    let submitted = submit_text(&mut app, "in flight");
    press(&mut app, KeyCode::Char('с'), KeyModifiers::CONTROL);
    let (request_id, _) = app.pending_cancel.expect("cancel requested");
    assert_eq!(request_id, submitted.request_id);
    assert!(!app.should_quit, "busy Ctrl+с must not quit");
    assert!(!app.quit_confirm, "busy Ctrl+с must not confirm-quit");
}

/// Plain Cyrillic typing must never be rewritten: the textarea receives
/// the layout characters verbatim (normalization is Ctrl-chord-only).
#[test]
fn plain_cyrillic_typing_lands_in_the_draft_untouched() {
    let mut app = app_with_chats(1);
    type_in(&mut app, "привет, мир");
    assert_eq!(app.input.lines().join(""), "привет, мир");
    assert!(!app.should_quit);
    assert!(app.active_request_id().is_none());
}
