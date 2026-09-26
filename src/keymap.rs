//! Keyboard dispatch: the ordered early-return key handler for the TUI
//! event loop, plus Cyrillic (ЙЦУКЕН / Ukrainian) Ctrl-chord normalization.
//!
//! Extracted verbatim from `main.rs` (pure move): `handle_key` is the thin
//! clipboard-default wrapper the run loop calls, `handle_key_with_copy` is
//! the testable seam shared with the mouse dispatch, and the two Cyrillic
//! helpers translate layout characters onto their Latin twins before any
//! chord matching. Tests live in the sibling `keymap/tests.rs` subtree.

use crossterm::event::{KeyCode, KeyModifiers};

use chibi_tui::app::{Focus, Mode, ReconnectRequest};

/// Map one Cyrillic character onto its Latin counterpart for the Russian
/// (ЙЦУКЕН) and Ukrainian keyboard layouts, preserving case for letters
/// (uppercase Cyrillic → uppercase Latin, so the case-sensitive
/// `Char('L')` reset shape keeps working). Characters outside both layouts
/// return `None` and pass through unchanged.
pub fn cyrillic_latin_counterpart(c: char) -> Option<char> {
    let lower = c.to_lowercase().next().unwrap_or(c);
    let mapped = match lower {
        // Shared ЙЦУКЕН top row (the Latin q..p keys).
        'й' => 'q',
        'ц' => 'w',
        'у' => 'e',
        'к' => 'r',
        'е' => 't',
        'н' => 'y',
        'г' => 'u',
        'ш' => 'i',
        'щ' => 'o',
        'з' => 'p',
        // Home row. ы (Russian) and і (Ukrainian) share the Latin `s` key;
        // ї / є / ґ are the Ukrainian-only keys on `]` / `'` / `\`.
        'ф' => 'a',
        'ы' | 'і' => 's',
        'в' => 'd',
        'а' => 'f',
        'п' => 'g',
        'р' => 'h',
        'о' => 'j',
        'л' => 'k',
        'д' => 'l',
        'ь' => 'm',
        // Bottom row (the Latin z..m keys).
        'я' => 'z',
        'ч' => 'x',
        'с' => 'c',
        'м' => 'v',
        'и' => 'b',
        'т' => 'n',
        // Non-letter keys keep their physical Latin twins so the input
        // behavior matches a Latin keyboard key-for-key.
        'х' => '[',
        'ъ' | 'ї' => ']',
        'ж' => ';',
        'э' | 'є' => '\'',
        'б' => ',',
        'ю' => '.',
        'ё' => '`',
        'ґ' => '\\',
        _ => return None,
    };
    if c.is_uppercase() && mapped.is_ascii_alphabetic() {
        Some(mapped.to_ascii_uppercase())
    } else {
        Some(mapped)
    }
}

/// Normalize a Ctrl-chord reported under a Cyrillic keyboard layout onto
/// the Latin chord the key dispatch matches.
///
/// Crossterm reports the LAYOUT character for modified keys, so under
/// ЙЦУКЕН `Ctrl+A` arrives as `Ctrl+Ф` and every chord match silently
/// failed until the user switched layouts. Applied at the very top of
/// [`handle_key`], BEFORE any chord matching, and ONLY to events carrying
/// CONTROL: plain typing (the textarea, the rename and search editors) is
/// returned untouched, characters outside both layouts pass through
/// unchanged, and every modifier rides along — so a normalized chord
/// behaves byte-for-byte like its Latin original, including the
/// case-sensitive `Char('L')` / `Char('l')` stop/reset split and the
/// Ctrl+Shift+F global-search shape.
pub fn normalize_cyrillic_ctrl_chord(
    mut key: crossterm::event::KeyEvent,
) -> crossterm::event::KeyEvent {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return key;
    }
    if let KeyCode::Char(c) = key.code {
        if let Some(mapped) = cyrillic_latin_counterpart(c) {
            key.code = KeyCode::Char(mapped);
        }
    }
    key
}

/// Apply key handling to app state. Backend interactions (submit, cancel,
/// reconnect) happen in the event loop by observing state changes, keeping
/// this function synchronous and testable.
pub fn handle_key(app: &mut chibi_tui::app::App, key: crossterm::event::KeyEvent) {
    handle_key_with_copy(app, key, &|text| {
        // Same clipboard transport as the selection release copy below
        // (OSC 52 + the env fallback); the outcome is deliberately
        // ignored — a failed write must not disturb the UI.
        let _ = chibi_tui::clipboard::copy_text(text);
    });
}

/// Testable form of [`handle_key`]: the clipboard write arrives as a
/// closure (production passes [`chibi_tui::clipboard::copy_text`], tests
/// capture into a buffer), the same seam [`handle_mouse_with_copy`] uses.
pub fn handle_key_with_copy(
    app: &mut chibi_tui::app::App,
    key: crossterm::event::KeyEvent,
    copy: &dyn Fn(&str),
) {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return;
    }
    // Ctrl-chords arrive with the LAYOUT character under the Russian
    // (ЙЦУКЕН) and Ukrainian keyboard layouts (Ctrl+Ф instead of Ctrl+A),
    // which left every chord dead until the layout was switched. Rewrite
    // the char to its Latin counterpart before ANY matching; events without
    // CONTROL are returned untouched, so plain typing into the textarea
    // never sees a changed keystroke.
    let key = normalize_cyrillic_ctrl_chord(key);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Alt+↑/↓ are a full synonym of Ctrl+↑/↓ thread
    // switching (see the match below); macOS Mission Control hijacks
    // Ctrl+arrows system-wide before they ever reach the terminal.
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // ---- modal quit-confirmation popup captures everything ----
    //
    // While the "Quit chibi-tui?" popup is open, ONLY the decision keys
    // work (the shared grammar `app::quit_decision`, also used by the
    // splash / setup loops): y/Enter quit, Esc/n stay, `q` stays (deliberate —
    // `q` can never confirm a quit; the same unbound-ish treatment the
    // destructive popups give it), a SECOND Ctrl+C confirms (the first one
    // only opened the popup). Everything else is swallowed so no keystroke
    // leaks into the textarea and no global binding fires. The isolation is
    // complete across ALL input sinks: keys (this branch), bracketed pastes
    // (`handle_paste` checks the flag), the loop-level submit gate
    // (`enter_consumed_by_quit_confirm` — the confirming Enter must never
    // also send the draft underneath) and pointer selection (the mouse
    // `selectable` guard). The popup is a
    // standalone flag, not a Mode: it may sit above any state (another
    // popup, a rename session, the sidebar) and dismissing restores that
    // state exactly, because opening it never changed anything. It is
    // cancel-safe by construction: an in-flight request keeps streaming
    // while the popup is open — cancellation still goes through its own
    // Ctrl+C path (busy Ctrl+C), which never opens this popup.
    if app.quit_confirm {
        match chibi_tui::app::quit_decision(key) {
            chibi_tui::app::QuitDecision::Confirm => app.confirm_quit(),
            chibi_tui::app::QuitDecision::Dismiss => {
                app.cancel_quit_confirm();
            }
            chibi_tui::app::QuitDecision::Swallow => {}
        }
        return;
    }

    // ---- modal error popup captures everything ----
    // (Ctrl+R rename is intentionally unreachable while the popup is open:
    // the popup branch returns before any mode handling.)
    if app.error_popup.is_some() {
        match key.code {
            // Reconnect request: executed by the event loop.
            KeyCode::Char('r') | KeyCode::Char('R') => {
                app.reconnect_requested = Some(ReconnectRequest {});
            }
            KeyCode::Esc => app.dismiss_error(),
            // q / Ctrl+C open the quit confirmation (same flow as the
            // idle Ctrl+C; the exit itself still needs the confirm).
            KeyCode::Char('q') => app.begin_quit_confirm(),
            KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
            // Any other key just dismisses the popup (stay in the app).
            _ => app.dismiss_error(),
        }
        return;
    }

    // diagnostics log viewer modal ----
    //
    // While the ^G log viewer is open, ONLY viewer keys work: the cursor
    // walks logical lines (↑/↓ and k/j by one line, PgUp/PgDn by a page,
    // g/G to the ends, where G returns to the live tail), `w` toggles wrap.
    // The viewer adds `/` (opens the search prompt), n/N
    // (next/prev match) and `y` (copy the cursor line). While the search
    // prompt is open it owns the keyboard completely: typing edits the
    // pattern, Enter commits, Esc cancels, everything else is swallowed.
    // While the cursor rests on the newest line the view keeps streaming
    // new arrivals in; one step up pins it to the cursor. Esc closes,
    // Ctrl+C opens the quit confirmation (same class as the other
    // popups). Everything else
    // (typing, global chords (^N/^R/^D/^T/^L/^F), thread switching) is
    // swallowed so no keystroke leaks into the textarea and no global
    // binding fires. There is no Enter action: the viewer is strictly
    // read-only, so Enter is swallowed and the loop's submit gates need no
    // extra snapshot flag.
    if matches!(app.mode, Mode::LogViewer { .. }) {
        // The open search prompt takes the keyboard before anything else.
        let search_prompt_open = match &app.mode {
            Mode::LogViewer { state } => state.search_buf.is_some(),
            _ => false,
        };
        if search_prompt_open {
            match key.code {
                KeyCode::Char(c) if !ctrl && !alt => app.log_search_push(c),
                KeyCode::Backspace if !ctrl && !alt => app.log_search_pop(),
                KeyCode::Enter => app.log_commit_search(),
                KeyCode::Esc => app.log_cancel_search(),
                KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::PageUp => app.log_page_up(),
            KeyCode::PageDown => app.log_page_down(),
            // One-line flavor of the same navigation (plain arrows and the
            // vim letters move the cursor, the viewport follows it). The
            // ctrl/alt flavors stay swallowed, same as before.
            KeyCode::Up | KeyCode::Char('k') if !ctrl && !alt => app.log_cursor_up(1),
            KeyCode::Down | KeyCode::Char('j') if !ctrl && !alt => app.log_cursor_down(1),
            KeyCode::Char('g') if !ctrl && !alt => app.log_jump_top(),
            KeyCode::Char('G') => app.log_jump_bottom(),
            KeyCode::Char('w') if !ctrl && !alt => app.log_toggle_wrap(),
            // Search round-trip: `/` opens the prompt (see above), n/N walk
            // the committed matches with wraparound on both ends.
            KeyCode::Char('/') if !ctrl && !alt => app.log_open_search(),
            KeyCode::Char('n') if !ctrl && !alt => app.log_search_next(),
            KeyCode::Char('N') if !ctrl && !alt => app.log_search_prev(),
            // Copy the full cursor line (OSC 52, with the env fallback).
            KeyCode::Char('y') if !ctrl && !alt => app.log_copy_selected(),
            KeyCode::Esc => {
                app.close_log_viewer();
            }
            KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
            _ => {}
        }
        return;
    }

    // model-picker modal captures everything ----
    //
    // While the ^M picker is open, ONLY picker keys work: ↑/↓ move the
    // selection (clamped at the list edges), PgUp/PgDn page the selection
    // by one viewport of the popup's visible rows (same clamp rule; the
    // page size is the rendered list height fed back by the renderer),
    // Enter confirms the highlighted
    // row (stages the hidden `/model <n>` request, a no-op until the
    // listing arrives), Esc closes without acting (a parked fetch is
    // dropped; an already-confirmed selection stays queued), Ctrl+C opens the
    // quit confirmation
    // (same class as the other popups). Everything else (typing, global
    // chords (^N/^R/^D/^T/^L/^F/^G/^O), thread switching) is swallowed so
    // no keystroke leaks into the textarea and no global binding fires.
    // There is no text input here: the listing is short enough to navigate
    // directly (task scope). The picker's Enter is additionally gated in the
    // event loop (`enter_consumed_by_picker`) so it can never ALSO submit
    // the message draft.
    if matches!(app.mode, Mode::ModelPicking { .. }) {
        match key.code {
            KeyCode::Up => app.model_picker_select_prev(),
            KeyCode::Down => app.model_picker_select_next(),
            KeyCode::PageUp => app.model_picker_page_up(),
            KeyCode::PageDown => app.model_picker_page_down(),
            KeyCode::Enter => app.confirm_model_picker(),
            KeyCode::Esc => {
                app.close_model_picker();
            }
            KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
            _ => {}
        }
        return;
    }

    // modal confirm popup captures everything ----
    //
    // While the Ctrl+D confirmation is open, ONLY the destructive decision
    // keys work: Enter/`y` confirm, Esc/`n` cancel, Ctrl+C opens the quit
    // confirmation (same
    // class as the error popup's Ctrl+C). Everything else (typing, arrows,
    // Ctrl+N/R/L/F) is swallowed so no keystroke leaks into the textarea
    // and no global binding fires. `q` is deliberately left UNBOUND here
    // (unlike the error popup): a stray `q` must never quit while a
    // destructive confirmation is on screen.
    if matches!(app.mode, Mode::ConfirmDelete) {
        match key.code {
            KeyCode::Enter => {
                app.confirm_delete();
            }
            // Plain y/Y confirm (Ctrl+Y is swallowed like any other combo).
            KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl => {
                app.confirm_delete();
            }
            KeyCode::Esc => {
                app.cancel_delete();
            }
            // Plain n/N cancel (Ctrl+N stays suspended: new-chat must not
            // fire and must not cancel the popup either).
            KeyCode::Char('n') | KeyCode::Char('N') if !ctrl => {
                app.cancel_delete();
            }
            KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
            _ => {}
        }
        return;
    }

    // stop/reset confirm popup captures ----
    // everything ----
    //
    // While the ^L (stop) or ⇧^L (reset) confirmation is open, ONLY the
    // destructive decision keys work: Enter/`y` confirm, Esc/`n`/same-chord
    // cancel (the chord that opened the popup: ^L for stop, ⇧^L for reset),
    // Ctrl+C opens the quit confirmation — the exact grammar of the delete
    // confirm. Everything
    // else (typing, arrows, global chords) is swallowed so no keystroke
    // leaks into the textarea and no global binding fires. `q` is
    // deliberately unbound here (same destructive-popup rule as the delete
    // confirm).
    if matches!(app.mode, Mode::ConfirmStopReset { .. }) {
        match key.code {
            KeyCode::Enter => {
                app.confirm_stop_reset();
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl => {
                app.confirm_stop_reset();
            }
            KeyCode::Esc => {
                app.cancel_stop_reset();
            }
            KeyCode::Char('n') | KeyCode::Char('N') if !ctrl => {
                app.cancel_stop_reset();
            }
            KeyCode::Char('l') | KeyCode::Char('L')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // Same-chord cancel: ^L (stop) and ⇧^L in both kitty
                // variants (reset) close the popup without staging anything.
                app.cancel_stop_reset();
            }
            KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
            _ => {}
        }
        return;
    }

    // modal search popup captures everything ----
    //
    // While the Ctrl+F search popup is open, ONLY search keys work: plain
    // chars edit the query (live recompute), Backspace edits backwards,
    // ↑/↓ navigate matches, Enter jumps to the selected match and closes,
    // Esc closes without jumping, Ctrl+C opens the quit confirmation (same
    // class as the other
    // popups). Everything else (PgUp/PgDn, arrows, Ctrl+N/R/L/D, q) is
    // swallowed so no keystroke leaks into the textarea and no global
    // binding fires. Chat scroll is driven ONLY by the jump, never by
    // PgUp/PgDn while the popup is open.
    if matches!(app.mode, Mode::Searching { .. }) {
        match key.code {
            KeyCode::Up => app.search_select_prev(),
            KeyCode::Down => app.search_select_next(),
            KeyCode::Enter => {
                app.jump_to_selected();
            }
            KeyCode::Esc => {
                app.cancel_search();
            }
            KeyCode::Backspace => app.search_backspace(),
            KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
            KeyCode::Char(ch) if !ctrl => app.search_push(ch),
            _ => {}
        }
        return;
    }

    // modal GLOBAL search popup captures ----
    // everything ----
    //
    // Same modal-ish isolation as the in-thread search popup: while the
    // Ctrl+Shift+F popup is open, ONLY search keys work. The one behavioral
    // difference is Enter: it ACTIVATES the match's thread (switching the
    // active chat, same mechanics as Ctrl+↑/↓) before recording the jump.
    // Everything else (PgUp/PgDn, arrows, Ctrl+N/R/L/D/F) is swallowed so
    // no keystroke leaks into the textarea and no global binding fires.
    if matches!(app.mode, Mode::SearchingAll { .. }) {
        match key.code {
            KeyCode::Up => app.search_all_select_prev(),
            KeyCode::Down => app.search_all_select_next(),
            KeyCode::Enter => {
                app.jump_to_selected_all();
            }
            KeyCode::Esc => {
                app.cancel_search_all();
            }
            KeyCode::Backspace => app.search_all_backspace(),
            KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
            KeyCode::Char(ch) if !ctrl => app.search_all_push(ch),
            _ => {}
        }
        return;
    }

    // keybindings help modal captures everything ----
    //
    // While the F1 help modal is open, ONLY viewer keys work: ↑/↓ scroll the
    // static keybindings table one line and PgUp/PgDn one page (both clamped
    // at the table's edges over the render-fed viewport — the same seam the
    // picker and the log viewer page by). F1 toggles closed (same-chord
    // semantics) and Esc closes; Ctrl+C opens the quit confirmation (same
    // class as the other
    // popups). There is no Enter action: the table is strictly read-only,
    // so Enter is swallowed and the loop's submit gate carries the same
    // help-modal snapshot the other popups have. Everything else (typing,
    // global chords, thread switching) is swallowed so no keystroke leaks
    // into the textarea and no global binding fires.
    if matches!(app.mode, Mode::HelpViewing { .. }) {
        match key.code {
            KeyCode::Up => app.help_scroll_up(),
            KeyCode::Down => app.help_scroll_down(),
            KeyCode::PageUp => app.help_page_up(),
            KeyCode::PageDown => app.help_page_down(),
            KeyCode::Esc | KeyCode::F(1) => {
                app.close_help_modal();
            }
            KeyCode::Char('c') if ctrl => app.begin_quit_confirm(),
            _ => {}
        }
        return;
    }

    // ---- global quit / cancel semantics ----
    //
    // Ctrl+C cancels the in-flight request when one exists, otherwise opens
    // the quit confirmation ("Quit chibi-tui?"), in EVERY mode (an open
    // rename session does not trap Ctrl+C; it cancels the draft implicitly
    // via the normal quit/cancel path). The two meanings never mix: a busy
    // Ctrl+C is a cancellation ONLY — it never opens the popup — so a
    // second Ctrl+C after the cancellation (request gone) is what asks
    // before quitting.
    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        if app.cancel_rename() {
            return; // Esc-equivalent: drop the draft first, stay consistent.
        }
        if let Some((request_id, thread_id)) = app.cancel_active() {
            app.pending_cancel = Some((request_id, thread_id));
        } else {
            app.begin_quit_confirm();
        }
        return;
    }

    // ---- inline thread rename mode ----
    //
    // Checked BEFORE normal input handling so keystrokes never leak into the
    // prompt textarea while a rename session is open. Entered with Ctrl+R in
    // Normal mode only (re-entry is a no-op inside App::begin_rename).
    if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) && ctrl && app.mode.is_normal() {
        app.begin_rename();
        return;
    }
    if let Mode::Renaming { .. } = app.mode {
        match key.code {
            // Shift+Enter / Alt+Enter insert a
            // newline into the rename draft instead of saving. The sidebar
            // renders `\n` as a space and persistence round-trips it, so
            // multi-line titles are safe end to end.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                if let Mode::Renaming { buf } = &mut app.mode {
                    buf.push('\n');
                }
            }
            KeyCode::Enter => {
                // Save handled here; persistence happens right after in the
                // event loop (same pattern as message submission).
                app.commit_rename();
            }
            KeyCode::Esc => {
                app.cancel_rename();
            }
            KeyCode::Backspace => app.rename_backspace(),
            KeyCode::Up | KeyCode::Down => {
                // Thread navigation stays blocked mid-rename, in ALL
                // modifier flavors: Ctrl+↑/↓ AND Alt+↑/↓ switch threads in
                // Normal mode,
                // but never under an open editor; plain ↑/↓ are consumed by
                // the block as caret no-ops). Switching the active chat
                // under an open rename would be confusing.
            }
            _ => {
                if let KeyCode::Char(ch) = key.code {
                    // Plain characters go straight into the draft…
                    app.rename_push(ch);
                } else {
                    // …everything else (arrows/Home/End/Ctrl+A/E/U/L word ops)
                    // is ignored: the draft is a plain single-line string, so
                    // only typing + backspace exist. Ctrl combos never reach
                    // here except modifiers-only presses, which are no-ops.
                    let _ = chibi_tui::input::Input::from(key);
                }
            }
        }
        return;
    }

    // the SIDEBAR owns the keyboard --------------------
    //
    // While the sidebar holds focus ONLY navigation + service keys work;
    // everything else is swallowed so no keystroke can ever leak into the
    // prompt textarea (Shift+Enter / Alt+Enter newlines, readline edits,
    // paste, all included). This branch sits AFTER the modal popup and
    // rename branches (those always capture first) and BEFORE the editor
    // fall-through:
    //
    // * ↑/↓ move the selection with LIVE active-chat switching: the same
    //   clamp + follow-bottom mechanics as Ctrl+↑/↓ in Normal mode; every
    //   arrow flavor routes here identically.
    // * bare Enter applies and returns focus to Chat (active chat stays
    //   highlighted; the event loop suppresses submission for it via
    //   `enter_consumed_by_sidebar`); Esc returns WITHOUT touching the
    //   draft, deliberately different from Normal-mode Esc (clears input).
    // * PgUp/PgDn STILL scroll the CHAT pane: reading works regardless of
    //   focus (documented choice).
    // * Global service chords stay live with their exact Normal-mode
    //   semantics (parity by contract with the chord match below): ^T
    //   toggles back to Chat, ^F / ^⇧F open the search popups (each close
    //   resets focus to Chat via App), ^N creates a chat (App lands focus
    //   on Chat), ^D opens the guarded confirm popup, ^G opens the
    //   diagnostics log viewer, ^L clears input + screen. ^C quit/cancel
    //   is serviced even earlier (global section).
    //   ^R rename also never reaches this branch: its entry guard sits
    //   above, so renaming from a Sidebar-focused UI works and closing the
    //   session returns focus to Chat (App::commit/cancel_rename).
    if app.focus == Focus::Sidebar {
        match key.code {
            KeyCode::Char('f') if ctrl && key.modifiers.contains(KeyModifiers::SHIFT) => {
                app.begin_search_all();
            }
            KeyCode::Char('t') if ctrl => app.toggle_focus(),
            KeyCode::Char('n') if ctrl => app.new_chat(),
            KeyCode::Char('d') if ctrl => app.begin_delete_confirm(),
            // read-only viewer = service chord; same
            // Normal-mode ^G semantics under sidebar focus (parity contract).
            KeyCode::Char('g') if ctrl => app.begin_log_viewer(),
            // same Normal-mode ^O semantics under sidebar
            // focus (parity contract with the chord match below).
            KeyCode::Char('o') if ctrl => app.toggle_status_strip(),
            // same Normal-mode ^S semantics under sidebar
            // focus (parity contract with the chord match below).
            KeyCode::Char('s') if ctrl => app.toggle_thoughts(),
            // same Normal-mode ^M semantics under
            // sidebar focus (parity contract with the chord match below).
            KeyCode::Char('m') if ctrl => app.begin_model_picker(),
            // same Normal-mode F1 semantics under
            // sidebar focus (parity contract with the chord match below).
            KeyCode::F(1) => app.begin_help_modal(),
            // same Normal-mode ^P semantics under sidebar
            // focus (parity contract with the chord match below).
            KeyCode::Char('p') if ctrl => app.begin_clone_thread(),
            // same pair as the Normal-mode ^L /
            // ⇧^L arms below (guarded confirm popups; idle ^L is a no-op).
            KeyCode::Char('L') if ctrl => app.begin_reset_confirm(),
            KeyCode::Char('l') if ctrl => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    app.begin_reset_confirm();
                } else {
                    app.begin_stop_confirm();
                }
            }
            KeyCode::Char('f') if ctrl => app.begin_search(),
            // Chat-pane scrolling regardless of focus.
            KeyCode::PageUp => app.scroll_up(app.chat_visible_rows),
            KeyCode::PageDown => app.scroll_down(app.chat_visible_rows),
            // Selection navigation with live active-chat switching (all
            // modifier flavors behave identically here).
            KeyCode::Up => app.select_prev(),
            KeyCode::Down => app.select_next(),
            // Apply & return to the editor, with no submit side effect.
            KeyCode::Enter if key.modifiers.is_empty() => app.focus = Focus::Chat,
            // Return without touching the draft (never clears input);
            // a mouse selection is cleared like in Normal mode.
            KeyCode::Esc => {
                app.clear_selection();
                app.focus = Focus::Chat;
            }
            // Everything else is swallowed while the sidebar is focused.
            _ => {}
        }
        return;
    }

    // ---- extended input keybindings (readline-style) ----
    // Paste accepts Ctrl+V and macOS Cmd+V (crossterm reports the Command
    // key as META).
    if matches!(key.code, KeyCode::Char('v'))
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::META)
    {
        crate::paste_clipboard(app);
        return;
    }
    match (key.code, ctrl) {
        // F1: TOGGLE THE KEYBINDINGS HELP MODAL,
        // the centered popup listing every chord the dispatch handles,
        // built from the single-source table in `ui.rs` that the tests pin
        // against the real handlers.
        //
        // Chord verification (feat task discipline, same audit class as the
        // ^G/^O/^S/^M/^P entries): the first candidate `?` was REJECTED —
        // a plain char falls through to the textarea's `(_, _)` arm and
        // inserts into the draft, so binding it would make a literal
        // question mark untypable in prompts. Ctrl+H was REJECTED too:
        // the editor maps Char('h') + CONTROL to backspace (the same
        // arm as Backspace, kept from tui-textarea 0.7), and several terminal
        // setups deliver the physical Backspace key as ASCII BS, i.e.
        // Char('h') + CONTROL — the chord IS the editor's backspace today.
        // F(1) is verified FREE: no binding anywhere in src/ (no
        // KeyCode::F hit at all), no editor mapping (unknown keys are
        // inert in the readline fall-through), and
        // no degradation cliff — F1 has a dedicated escape sequence on
        // legacy terminals as well, so the chord works with or without the
        // kitty keyboard protocol. Mnemonic: the universal help key.
        (KeyCode::F(1), _) => {
            app.begin_help_modal();
            return;
        }
        // Ctrl+L: stop the RUNNING request via
        // the guarded confirm popup; the backend intercepts the staged
        // `/stop` prompt pre-LLM and reuses the telegram handler core
        // (task cancel + subagent counter kill-flush). Idle is a silent
        // no-op: nothing to stop, and the retired screen-wipe semantics
        // taught that a visible idle action invites accidental clears.
        // Chord verification (same audit class as ^F/^G/^O/^S/^M): ^L never
        // reaches the editor — the dispatch claims the chord before the
        // readline fall-through ever sees it, so the chord is free.
        (KeyCode::Char('l'), true) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            // Kitty-protocol variant that reports the unshifted char with
            // the SHIFT flag: reset, same as the ⇧^L arm below.
            app.begin_reset_confirm();
            return;
        }
        (KeyCode::Char('l'), true) => {
            app.begin_stop_confirm();
            return;
        }
        // Shift+Ctrl+L: reset the thread via the
        // guarded confirm popup; the staged `/reset` prompt reaches the
        // backend out-of-band and a confirmed ack clears the local dialog.
        // Kitty protocol delivers the chord as Char('L') + CONTROL (the
        // SHIFT flag may ride along); terminals WITHOUT it degrade to plain
        // ^L (stop) — documented in the README, same class as ^⇧F.
        (KeyCode::Char('L'), true) => {
            app.begin_reset_confirm();
            return;
        }
        (KeyCode::Char('u'), true) => {
            // Delete from cursor to start of line. The old tui-textarea
            // engine mapped Ctrl+U to undo, which surprised readline
            // users; the in-house editor keeps readline semantics.
            app.input.delete_line_by_head();
            return;
        }
        // Ctrl+N: new chat.
        (KeyCode::Char('n'), true) => {
            app.new_chat();
            return;
        }
        // Ctrl+D: delete the active thread (idle only) via the confirm
        // popup. Busy/queued chats are refused with a status toast inside
        // App::begin_delete_confirm: the popup never opens there.
        (KeyCode::Char('d'), true) => {
            app.begin_delete_confirm();
            return;
        }
        // Ctrl+G: open the diagnostics log viewer.
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): the first-choice ^Y candidate was REJECTED:
        // the readline family binds ^Y to paste-from-kill-ring (kept
        // from tui-textarea 0.7, src/textarea.rs:589), and the kill ring
        // IS populated in this app:
        // our own ^U override calls delete_line_by_head() → delete_piece()
        // which stores the killed text (textarea.rs:1022), so ^U→^Y (kill
        // line, paste it back) is live behavior today. Taking ^Y would break
        // that readline kill/yank family (^U/^K/^W/^Y). ^G is verified FREE:
        // no app binding anywhere in src/, no editor mapping, no
        // macOS system hijack, no flow-control semantics, and its readline
        // meaning (abort) has no function in this TUI. Mnemonic: loG.
        (KeyCode::Char('g'), true) => {
            app.begin_log_viewer();
            return;
        }
        // Ctrl+O: TOGGLE THE STATUS STRIP, the dim
        // one-row `cwd: <workspace> · <model>` readout on the chat pane's
        // top border. Visible by default; pure view state (like ^T's
        // Focus), modals swallow the chord like every other one.
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): ^G was already taken by the log viewer, so
        // the other task candidate ^O was verified FREE: no app binding
        // anywhere in src/ (only `Char('o')` hits are plain typing), no
        // editor shortcut (the readline table covers a/b/d/e/f/h/j/k/n/p/
        // u/w/y/<>/[], no 'o'), not part of this app's readline
        // family (^A/^E/^U/^K/^W/^Y/^L; GNU readline's operate-and-get-
        // next is a shell-side binding that never fires inside the TUI), no
        // macOS system hijack (Mission Control only takes ^arrows), and the
        // legacy tty VDISCARD semantics of ^O are inert under raw mode plus
        // the kitty keyboard protocol. Mnemonic: infO.
        (KeyCode::Char('o'), true) => {
            app.toggle_status_strip();
            return;
        }
        // Ctrl+S: TOGGLE THE THOUGHTS BLOCK, the dim
        // reasoning trace rendered above the latest answer. Session-only
        // view state (default ON) like the ^O strip: the flip only changes
        // rendering — nothing is cleared and reasoning is never persisted.
        //
        // Chord verification (feat task discipline, same audit class as the
        // ^G/^O/^M/^P entries): no app binding anywhere in src/ (the only
        // `Char('s')` hits are plain typing), no editor shortcut (the
        // readline table covers a/b/d/e/f/h/j/k/n/p/u/w/y/<>/[], no 's'),
        // not part of this app's readline family (^A/^E/^U/^K/^W/
        // ^Y/^L), no macOS system hijack (Mission Control only takes
        // ^arrows), and the legacy tty IXON flow-control meaning of ^S is
        // inert under raw mode (crossterm enables raw at startup).
        // Mnemonic: thoughtS.
        (KeyCode::Char('s'), true) => {
            app.toggle_thoughts();
            return;
        }
        // Ctrl+M: OPEN THE MODEL PICKER, the
        // centered popup that fetches the bare `/model` listing as a hidden
        // exchange (no transcript bubbles) and switches models by sending
        // `/model <n>` the same hidden way.
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): the readline editor maps Ctrl+M to
        // insert_newline() (the same arm as Enter, kept from tui-textarea
        // 0.7),
        // which this binding deliberately OVERRIDES exactly like the
        // existing ^U undo override: the global match claims the chord and
        // returns before the textarea ever sees it, and no app flow relies
        // on a ^M newline (bare Enter already inserts one). The kitty
        // keyboard protocol pushed at startup (DISAMBIGUATE_ESCAPE_CODES)
        // makes capable terminals deliver ^M as an event distinct from
        // Enter; on legacy terminals ^M degrades to bare Enter, with a
        // non-empty draft that SUBMITS it (README-documented caveat, same
        // class as the Shift+Enter degradation). No macOS system hijack
        // (Mission Control only takes ^arrows); GNU readline's C-M
        // accept-line is a shell-side binding that never fires inside the
        // TUI (same argument as the ^O audit). Mnemonic: Model.
        (KeyCode::Char('m'), true) => {
            app.begin_model_picker();
            return;
        }
        // Ctrl+P: CLONE THE ACTIVE THREAD, the backend
        // command /new_thread_with_current_context sent on the NEW thread's
        // identity so the clone inherits the source's full conversation
        // context. Gated by feature detection: without the command in the
        // handshake capabilities the chord shows an informative popup
        // instead of acting (App::begin_clone_thread owns all the guards).
        //
        // Chord verification (feat task discipline, same audit class as the
        // ^G/^O/^M entries): no app binding anywhere in src/ (the only `p`
        // hits are the splash art color table), and the readline editor maps
        // Ctrl+P to move-cursor-up (kept from tui-textarea 0.7), which this
        // binding deliberately
        // OVERRIDES exactly like the ^U undo override: the global match
        // claims the chord and returns before the textarea ever sees it, and
        // no app flow relies on a ^P caret move (plain ↑ is the caret
        // movement here). No macOS system hijack (Mission Control only takes
        // ^arrows); the legacy tty VDISCARD-style meaning of ^P is inert
        // under raw mode plus the kitty keyboard protocol. Mnemonic: P for
        // photocopy.
        (KeyCode::Char('p'), true) => {
            app.begin_clone_thread();
            return;
        }
        // Ctrl+Shift+F: GLOBAL search across ALL threads. With the
        // kitty keyboard protocol
        // (pushed at startup) Ctrl+Shift+F arrives as Char('f') +
        // CONTROL|SHIFT; on terminals WITHOUT it the Shift modifier is
        // lost and the chord degrades to plain Ctrl+F (in-thread search),
        // documented in the README. Guarded inside App::begin_search_all
        // (Normal mode only), so it can never fire over the confirm/rename/
        // search popups: those branches return before this match runs.
        (KeyCode::Char('f'), true) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.begin_search_all();
            return;
        }
        // Ctrl+F: open the in-thread search popup.
        // Guarded inside App::begin_search (Normal mode + active chat
        // only), so this can never fire over the confirm/rename popups;
        // those branches return before this match runs.
        (KeyCode::Char('f'), true) => {
            app.begin_search();
            return;
        }
        // Ctrl+T: TOGGLE PANE FOCUS, which flips the keyboard
        // between Chat (editor) and Sidebar. Replaces the old wrap-cycling
        // thread switcher, which live-check feedback rejected ("just moves
        // the selection down"). Deliberately a plain Ctrl+letter chord so it
        // works in ANY terminal. Modal modes swallow this like every other
        // chord: the error/confirm/search branches and the rename branch
        // return before this match runs.
        (KeyCode::Char('t'), true) => {
            app.toggle_focus();
            return;
        }
        // Ctrl+A / Ctrl+E reach the editor's built-in readline mappings
        // (head/end of line); they fall through untouched below.
        _ => {}
    }

    // ---- vertical arrows & thread switching --------------------------------
    // * Ctrl+↑ / Ctrl+↓ AND Alt+↑ / Alt+↓ switch the ACTIVE THREAD, carrying
    //   over EXACTLY the semantics plain ↑/↓ had before this rework:
    //   App::select_prev/next bounds-clamp the index and reset the chat
    //   scroll to 0; sidebar focus/dot refresh derives from `active` at draw
    //   time, so it follows for free. Alt is a FULL SYNONYM (not a fallback):
    //   both flavors route to the identical select_prev/select_next call:
    //   zero behavior divergence. Alt exists because macOS Mission Control
    //   hijacks Ctrl+arrows system-wide before they reach the terminal.
    // * Plain ↑ / ↓ move the TEXT CURSOR vertically inside the editor via
    //   the editor's native Up/Down mapping (caret up/down, column
    //   preserved and clamped). They
    //   never submit and never switch threads; the caret auto-follows the
    //   grown editor viewport because ui::draw renders the widget over
    //   the full grown block every frame.
    let input_is_empty = app.input.lines().iter().all(|l| l.is_empty());

    match (key.code, ctrl) {
        (KeyCode::Up, true) => app.select_prev(),
        (KeyCode::Down, true) => app.select_next(),
        // Alt+↑/↓ (ctrl unset): same select_prev/next mechanics as the ctrl
        // flavor above. When BOTH modifiers ride along, the ctrl arm wins:
        // identical to pre-alt behavior.
        (KeyCode::Up, false) if alt => app.select_prev(),
        (KeyCode::Down, false) if alt => app.select_next(),
        // Plain (and Shift-decorated) vertical arrows go straight into the
        // textarea's readline-compatible handler: caret movement only.
        // Alt+↑/↓ never reach this arm (the synonym arms above take them).
        (KeyCode::Up | KeyCode::Down, _) => {
            let converted: chibi_tui::input::Input = key.into();
            app.input.input(converted);
        }
        (KeyCode::PageUp, _) => app.scroll_up(app.chat_visible_rows),
        (KeyCode::PageDown, _) => app.scroll_down(app.chat_visible_rows),
        // y with a held chat selection copies its plain text through the
        // SAME clipboard path as the drag-release copy (and the log
        // viewer's `y`); the outcome is ignored — a failed write degrades
        // silently. The selection STAYS active after copying (the user
        // still sees what they copied; a click / Esc / thread switch
        // clears it as before). Without a selection the arm does not
        // claim the key: plain `y` keeps its pre-existing behavior
        // (typing into the draft) and no binding is shadowed.
        // Latin-only by convention: under RU/UA layouts the plain letter
        // is never rewritten (only Ctrl-chords are normalized), so `y`
        // stays a Latin keypress by design.
        (KeyCode::Char('y'), false) if !alt && app.selection.is_some() => {
            if let Some(text) = app.selection_text() {
                copy(&text);
            }
        }
        (KeyCode::Esc, _) if !input_is_empty => {
            // Non-empty input: clear it (and any mouse selection with it).
            app.clear_selection();
            app.clear_input();
        }
        // Esc with an empty input clears the mouse selection (the
        // keyboard's "deselect"); it still never quits.
        (KeyCode::Esc, _) => app.clear_selection(),
        // Only Ctrl+C quits (idle) or cancels (busy).
        // BARE Enter is swallowed here and submitted by the loop's
        // should_submit() gate; Shift+Enter / Alt+Enter fall through to the
        // textarea as newline inserts. On terminals WITHOUT the kitty
        // keyboard protocol, Shift+Enter arrives as bare Enter bytes and
        // degrades to submit, as documented in the README.
        (KeyCode::Enter, _)
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            app.input.insert_newline();
        }
        (KeyCode::Enter, _) => {}
        // Everything else (including Ctrl+A/E, Alt+B/F, Alt+D word ops)
        // goes into the textarea's readline-compatible handler.
        (_, _) => {
            let converted: chibi_tui::input::Input = key.into();
            app.input.input(converted);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
