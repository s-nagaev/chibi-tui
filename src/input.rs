//! Minimal multi-line readline-style input editor.
//!
//! In-house replacement for the `tui-textarea` crate: the editor surface this
//! app actually uses is small (readline keybindings, multi-line buffer,
//! caret-following scroll, a placeholder slot for state checks), and the
//! upstream crate pins an older `ratatui`/`crossterm` generation whose types
//! cannot interoperate with the current ones. Keeping the dependency would
//! pin the vulnerable `lru` advisory this workspace set out to close.
//!
//! Behavioral contract mirrors what `tui-textarea` provided here:
//! * readline movement/editing (`Ctrl+A/E/K/U/W/Y/B/F/P/N`, `Alt+B/F/D`,
//!   arrows, `Home`/`End`, `Backspace`/`Delete`);
//! * a kill ring of exactly ONE slot (`Ctrl+U`/`Ctrl+K`/`Ctrl+W`/`Alt+D` all
//!   store, `Ctrl+Y` pastes back) — same as the old engine;
//! * multi-line buffer keyed by `(row, col)` with `col` a CHAR index;
//! * rendering paints the caret with `REVERSED` (the terminal cursor is never
//!   moved) and scrolls only as needed to keep the caret visible;
//! * unknown keys are no-ops. `App` intercepts `Enter`, `Ctrl+U`, `Ctrl+L`,
//!   `Ctrl+M`, `Ctrl+F`, `Ctrl+P`… BEFORE the editor sees them — the
//!   readline fallbacks below exist for fidelity, not for the app's flows.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use std::cell::Cell;
use unicode_width::UnicodeWidthChar;

/// A key event distilled from crossterm, shaped for the editor.
///
/// Mirrors the old `tui_textarea::Key` enum closely enough that call sites
/// only change their path, not their shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Key {
    #[default]
    Null,
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
    /// Any key the editor has no mapping for (no-op).
    Other,
}

/// An editor input: a key plus the readline-relevant modifier flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Input {
    /// Convenience constructor matching the old struct-literal call sites.
    pub fn char(ch: char) -> Self {
        Self {
            key: Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }
}

impl From<crossterm::event::KeyEvent> for Input {
    fn from(key: crossterm::event::KeyEvent) -> Self {
        use crossterm::event::{KeyCode, KeyModifiers};
        let key_kind = match key.code {
            KeyCode::Char(ch) => Key::Char(ch),
            KeyCode::Enter => Key::Enter,
            KeyCode::Tab => Key::Tab,
            KeyCode::BackTab => Key::BackTab,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Delete => Key::Delete,
            KeyCode::Insert => Key::Insert,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::F(n) => Key::F(n),
            KeyCode::Null => Key::Null,
            _ => Key::Other,
        };
        Self {
            key: key_kind,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            shift: key.modifiers.contains(KeyModifiers::SHIFT),
        }
    }
}

/// Multi-line readline-style input buffer + renderer.
#[derive(Clone, Debug)]
pub struct InputArea {
    lines: Vec<String>,
    /// Caret position: line index, then CHAR index into that line.
    row: usize,
    col: usize,
    placeholder: Option<String>,
    /// Single-slot kill ring: text stored by the last kill, pasted by Ctrl+Y.
    yank: Option<String>,
    /// Caret-following scroll offsets, remembered across frames. `Cell`
    /// because the `Widget` impl takes the editor by shared reference.
    v_scroll: Cell<usize>,
    h_scroll: Cell<usize>,
}

impl Default for InputArea {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            placeholder: None,
            yank: None,
            v_scroll: Cell::new(0),
            h_scroll: Cell::new(0),
        }
    }
}

impl InputArea {
    /// Insert text at the caret. `\n` splits lines (multi-line paste);
    /// the caret lands after the inserted text.
    pub fn insert_str(&mut self, text: &str) {
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            self.insert_str_at_cursor(first);
        }
        for part in parts {
            self.insert_newline();
            self.insert_str_at_cursor(part);
        }
    }

    fn insert_str_at_cursor(&mut self, text: &str) {
        let byte = self.char_to_byte(self.row, self.col);
        self.lines[self.row].insert_str(byte, text);
        self.col += text.chars().count();
    }

    /// Split the current line at the caret (Shift+Enter / Alt+Enter path).
    pub fn insert_newline(&mut self) {
        let byte = self.char_to_byte(self.row, self.col);
        let rest = self.lines[self.row].split_off(byte);
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, rest);
    }

    /// Kill from the head of the line to the caret (readline `Ctrl+U`
    /// semantics; `App::handle_key` calls this directly). At head of line the
    /// preceding newline goes instead — the line joins the previous one,
    /// matching tui-textarea 0.7's `delete_line_by_head` and the editor's own
    /// `Ctrl+J` arm. The killed text is stored for `Ctrl+Y`.
    pub fn delete_line_by_head(&mut self) {
        self.kill_move_line_head();
    }

    /// The whole buffer, one entry per line.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Caret position as `(row, col)` with `col` in chars.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn set_placeholder_text(&mut self, text: impl Into<String>) {
        self.placeholder = Some(text.into());
    }

    pub fn placeholder_text(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }

    /// Dispatch one input event — a faithful port of the tui-textarea 0.7
    /// default keymap (the arms this app can actually reach).
    ///
    /// Ctrl-chords match LOWERCASE characters only, exactly like the old
    /// engine: `Ctrl+Shift+A` (kitty sends `'A'`) was a no-op there and stays
    /// one here — pinned by the cyrillic-layout parity test in `main.rs`.
    /// Known deliberate divergences, both overridden app-side anyway:
    /// `Ctrl+U` kills to line head (readline semantics) instead of undo, and
    /// `Ctrl+R` is inert instead of redo.
    pub fn input(&mut self, input: Input) {
        let key = input.key;
        let (ctrl, alt) = (input.ctrl, input.alt);
        match key {
            // ---- newline / typing --------------------------------
            Key::Char('m') if ctrl && !alt => self.insert_newline(),
            Key::Char('\n' | '\r') if !ctrl && !alt => self.insert_newline(),
            Key::Enter => self.insert_newline(),
            Key::Char(c) if !ctrl && !alt => self.insert_char(c),
            Key::Tab if !ctrl && !alt => self.insert_tab(),
            // ---- deletion ----------------------------------------
            Key::Char('h') if ctrl && !alt => self.backspace(),
            Key::Backspace if !ctrl && !alt => self.backspace(),
            Key::Char('d') if ctrl && !alt => self.delete_next_char(),
            Key::Delete if !ctrl && !alt => self.delete_next_char(),
            Key::Char('k') if ctrl && !alt => self.kill_move_line_end(),
            Key::Char('j') if ctrl && !alt => self.kill_move_line_head(),
            Key::Char('u') if ctrl && !alt => self.kill_move_line_head(),
            Key::Char('w') if ctrl && !alt => self.delete_word_back(),
            Key::Char('h') if !ctrl && alt => self.delete_word_back(),
            Key::Backspace if alt => self.delete_word_back(),
            Key::Delete if alt => self.delete_word_forward(),
            Key::Char('d') if alt => self.delete_word_forward(),
            // ---- character movement -------------------------------
            Key::Char('n') if ctrl && !alt => self.move_down(),
            Key::Down if !ctrl && !alt => self.move_down(),
            Key::Char('p') if ctrl && !alt => self.move_up(),
            Key::Up if !ctrl && !alt => self.move_up(),
            Key::Char('f') if ctrl && !alt => self.move_forward(),
            Key::Right if !ctrl && !alt => self.move_forward(),
            Key::Char('b') if ctrl && !alt => self.move_back(),
            Key::Left if !ctrl && !alt => self.move_back(),
            // ---- line / buffer movement ---------------------------
            Key::Char('a') if ctrl && !alt => self.move_line_head(),
            Key::Home => self.move_line_head(),
            Key::Left if ctrl && alt => self.move_line_head(),
            Key::Char('b') if ctrl && alt => self.move_line_head(),
            Key::Char('e') if ctrl && !alt => self.move_line_end(),
            Key::End => self.move_line_end(),
            Key::Right if ctrl && alt => self.move_line_end(),
            Key::Char('f') if ctrl && alt => self.move_line_end(),
            Key::Char('<') if alt => self.move_buffer_top(),
            Key::Up if ctrl && alt => self.move_buffer_top(),
            Key::Char('p') if ctrl && alt => self.move_buffer_top(),
            Key::Char('>') if alt => self.move_buffer_bottom(),
            Key::Down if ctrl && alt => self.move_buffer_bottom(),
            Key::Char('n') if ctrl && alt => self.move_buffer_bottom(),
            // ---- word movement ------------------------------------
            Key::Char('f') if alt => self.word_forward(),
            Key::Right if ctrl => self.word_forward(),
            Key::Char('b') if alt => self.word_back(),
            Key::Left if ctrl => self.word_back(),
            // ---- paragraph movement -------------------------------
            Key::Char(']') if alt => self.paragraph_forward(),
            Key::Char('n') if alt => self.paragraph_forward(),
            Key::Down if ctrl => self.paragraph_forward(),
            Key::Char('[') if alt => self.paragraph_back(),
            Key::Char('p') if alt => self.paragraph_back(),
            Key::Up if ctrl => self.paragraph_back(),
            // ---- kill ring ----------------------------------------
            Key::Char('y') if ctrl && !alt => self.yank(),
            // Ctrl+R (redo) / Ctrl+X/C/V / PageUp/Down / mouse / F-keys:
            // either intercepted by `App::handle_key` or inert, as before.
            _ => {}
        }
    }

    // ---- cursor primitives ------------------------------------

    fn line_len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    fn char_to_byte(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(self.lines[row].len())
    }

    /// Display width of the line prefix before `col`.
    fn prefix_width(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .chars()
            .take(col)
            .map(|c| c.width().unwrap_or(0))
            .sum()
    }

    fn insert_char(&mut self, ch: char) {
        let byte = self.char_to_byte(self.row, self.col);
        self.lines[self.row].insert(byte, ch);
        self.col += 1;
    }

    /// Soft tab: spaces up to the next multiple of 4 display columns
    /// (the old engine's defaults: `tab_len = 4`, soft tab).
    fn insert_tab(&mut self) {
        let width = self.prefix_width(self.row, self.col);
        let pad = 4 - (width % 4);
        self.insert_str_at_cursor(&" ".repeat(pad));
    }

    fn move_forward(&mut self) {
        if self.col < self.line_len(self.row) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn move_back(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len(self.row);
        }
    }

    fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.line_len(self.row));
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.line_len(self.row));
        }
    }

    fn move_line_head(&mut self) {
        self.col = 0;
    }

    fn move_line_end(&mut self) {
        self.col = self.line_len(self.row);
    }

    fn move_buffer_top(&mut self) {
        self.row = 0;
        self.col = self.col.min(self.line_len(0));
    }

    fn move_buffer_bottom(&mut self) {
        self.row = self.lines.len() - 1;
        self.col = self.col.min(self.line_len(self.row));
    }

    // ---- deletion primitives ----------------------------------

    /// Join the current line into the previous one (removes the newline
    /// BEFORE the caret). No-op on the first line.
    fn delete_newline_back(&mut self) {
        if self.row == 0 {
            return;
        }
        let line = self.lines.remove(self.row);
        self.row -= 1;
        self.col = self.line_len(self.row);
        self.lines[self.row].push_str(&line);
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let byte = self.char_to_byte(self.row, self.col);
            let prev = self.lines[self.row][..byte]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.lines[self.row].replace_range(prev..byte, "");
            self.col -= 1;
        } else {
            self.delete_newline_back();
        }
    }

    /// Delete the char AFTER the caret; at end of line removes the newline
    /// (pulls the next line up), at end of buffer a no-op.
    fn delete_next_char(&mut self) {
        if self.col < self.line_len(self.row) {
            let start = self.char_to_byte(self.row, self.col);
            let end = self.char_to_byte(self.row, self.col + 1);
            self.lines[self.row].replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    /// Kill from the caret to end of line (`Ctrl+K`); at end of line the
    /// newline goes instead (join next line).
    fn kill_move_line_end(&mut self) {
        let width = self.line_len(self.row);
        if self.col < width {
            let start = self.char_to_byte(self.row, self.col);
            let killed = self.lines[self.row].split_off(start);
            self.yank = Some(killed);
        } else {
            self.delete_next_char();
        }
    }

    /// Kill from head of line to the caret (`Ctrl+J`, and `Ctrl+U` — the
    /// app's readline override); at head of line the preceding newline goes
    /// instead (join into previous line).
    fn kill_move_line_head(&mut self) {
        if self.col > 0 {
            let end = self.char_to_byte(self.row, self.col);
            let killed = self.lines[self.row].drain(..end).collect::<String>();
            self.col = 0;
            self.yank = Some(killed);
        } else {
            self.delete_newline_back();
        }
    }

    // ---- word ops (three char kinds: whitespace / punctuation / other) --

    fn char_kind(c: char) -> u8 {
        if c.is_whitespace() {
            0
        } else if c.is_ascii_punctuation() {
            1
        } else {
            2
        }
    }

    /// Column of the head of the next word on this line, if any.
    fn find_word_start_forward(line: &str, start: usize) -> Option<usize> {
        let mut it = line.chars().enumerate().skip(start);
        let mut prev = Self::char_kind(it.next()?.1);
        for (col, c) in it {
            let cur = Self::char_kind(c);
            if cur != 0 && prev != cur {
                return Some(col);
            }
            prev = cur;
        }
        None
    }

    /// Column just past the end of the word at/after the caret, if any.
    fn find_word_exclusive_end_forward(line: &str, start: usize) -> Option<usize> {
        let mut it = line.chars().enumerate().skip(start);
        let mut prev = Self::char_kind(it.next()?.1);
        for (col, c) in it {
            let cur = Self::char_kind(c);
            if prev != 0 && prev != cur {
                return Some(col);
            }
            prev = cur;
        }
        None
    }

    /// Column of the head of the word before the caret, if any.
    fn find_word_start_backward(line: &str, start: usize) -> Option<usize> {
        let byte = line
            .char_indices()
            .nth(start)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        let mut it = line[..byte].chars().rev().enumerate();
        let mut cur = Self::char_kind(it.next()?.1);
        for (i, c) in it {
            let next = Self::char_kind(c);
            if cur != 0 && next != cur {
                return Some(start - i);
            }
            cur = next;
        }
        (cur != 0).then_some(0)
    }

    /// Move to the head of the next word (`Alt+F`); at end of line to the
    /// head of the next line; on the last line to its end.
    fn word_forward(&mut self) {
        if let Some(col) = Self::find_word_start_forward(&self.lines[self.row], self.col) {
            self.col = col;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        } else {
            self.col = self.line_len(self.row);
        }
    }

    /// Move to the head of the previous word (`Alt+B`); at head of line to
    /// the end of the previous line; on the first line to its head.
    fn word_back(&mut self) {
        if let Some(col) = Self::find_word_start_backward(&self.lines[self.row], self.col) {
            self.col = col;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len(self.row);
        } else {
            self.col = 0;
        }
    }

    /// Kill back to the head of the previous word (`Ctrl+W`, `Alt+H`,
    /// `Alt+Backspace`); at head of line the preceding newline goes.
    fn delete_word_back(&mut self) {
        if let Some(col) = Self::find_word_start_backward(&self.lines[self.row], self.col) {
            let start = self.char_to_byte(self.row, col);
            let end = self.char_to_byte(self.row, self.col);
            let killed = self.lines[self.row][start..end].to_string();
            self.lines[self.row].replace_range(start..end, "");
            self.col = col;
            self.yank = Some(killed);
        } else if self.col > 0 {
            let end = self.char_to_byte(self.row, self.col);
            let killed = self.lines[self.row].drain(..end).collect::<String>();
            self.col = 0;
            self.yank = Some(killed);
        } else {
            self.delete_newline_back();
        }
    }

    /// Kill forward to the end of the current/next word (`Alt+D`,
    /// `Alt+Delete`); at end of line the newline goes.
    fn delete_word_forward(&mut self) {
        if let Some(col) = Self::find_word_exclusive_end_forward(&self.lines[self.row], self.col) {
            let start = self.char_to_byte(self.row, self.col);
            let end = self.char_to_byte(self.row, col);
            let killed = self.lines[self.row][start..end].to_string();
            self.lines[self.row].replace_range(start..end, "");
            self.yank = Some(killed);
        } else {
            let end = self.line_len(self.row);
            if self.col < end {
                let start = self.char_to_byte(self.row, self.col);
                let killed = self.lines[self.row].split_off(start);
                self.yank = Some(killed);
            } else if self.row + 1 < self.lines.len() {
                self.delete_next_char();
            }
        }
    }

    /// Jump to the first non-empty line after an empty one (`Alt+]`,
    /// `Alt+N`); past the buffer end, to the last line.
    fn paragraph_forward(&mut self) {
        let mut prev_is_empty = self.lines[self.row].is_empty();
        for row in self.row + 1..self.lines.len() {
            let is_empty = self.lines[row].is_empty();
            if !is_empty && prev_is_empty {
                self.row = row;
                self.col = self.col.min(self.line_len(row));
                return;
            }
            prev_is_empty = is_empty;
        }
        self.move_buffer_bottom();
    }

    /// Jump to the first non-empty line before an empty one (`Alt+[`,
    /// `Alt+P`); above the buffer top, to the first line.
    fn paragraph_back(&mut self) {
        if self.row == 0 {
            return;
        }
        let mut prev_is_empty = self.lines[self.row - 1].is_empty();
        for row in (0..self.row - 1).rev() {
            let is_empty = self.lines[row].is_empty();
            if is_empty && !prev_is_empty {
                self.row = row + 1;
                self.col = self.col.min(self.line_len(row + 1));
                return;
            }
            prev_is_empty = is_empty;
        }
        self.move_buffer_top();
    }

    fn yank(&mut self) {
        if let Some(text) = self.yank.clone() {
            self.insert_str(&text);
        }
    }

    // ---- rendering ---------------------------------------------

    /// Display-column offset of the caret within its line.
    fn caret_col_width(&self) -> usize {
        self.prefix_width(self.row, self.col)
    }
}

impl Widget for &InputArea {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        // caret-following vertical scroll
        let height = area.height as usize;
        let v = self.v_scroll.get();
        let v = if self.row < v {
            self.row
        } else if self.row >= v + height {
            self.row + 1 - height
        } else {
            v
        };
        self.v_scroll.set(v);
        // caret-following horizontal scroll (display columns)
        let width = area.width as usize;
        let caret_col = self.caret_col_width();
        let h = self.h_scroll.get();
        let h = if caret_col < h {
            caret_col
        } else if caret_col >= h + width {
            caret_col + 1 - width
        } else {
            h
        };
        self.h_scroll.set(h);

        for (i, y) in (area.y..area.y + area.height).enumerate() {
            let row = v + i;
            if row >= self.lines.len() {
                break;
            }
            let line = &self.lines[row];
            let mut col = 0usize; // display column of the next char
            let mut x = 0usize; // cell offset within the area
            for ch in line.chars() {
                let w = ch.width().unwrap_or(0).max(1);
                if col + w <= h {
                    col += w;
                    continue;
                }
                if col < h {
                    // wide char straddling the scroll edge: skip whole char
                    col += w;
                    continue;
                }
                if x + w > width {
                    break;
                }
                buf[(area.x + x as u16, y)].set_char(ch);
                col += w;
                x += w;
            }
            // untouched cells keep whatever bg the panel already painted
        }

        // the caret: REVERSED cell under the cursor (the terminal cursor is
        // never moved); renders as a reversed blank at end-of-line
        let caret_row = self.row - v;
        let cx = area.x as usize + caret_col - h;
        if caret_row < height && cx < area.x as usize + width {
            buf[(cx as u16, area.y + caret_row as u16)]
                .set_style(Style::new().add_modifier(Modifier::REVERSED));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(w: &mut InputArea, code: KeyCode, modifiers: KeyModifiers) {
        w.input(Input::from(KeyEvent::new(code, modifiers)));
    }

    #[test]
    fn typing_and_readline_movement_work() {
        let mut w = InputArea::default();
        for ch in "hello".chars() {
            press(&mut w, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(w.cursor(), (0, 5));
        press(&mut w, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(w.cursor(), (0, 0));
        press(&mut w, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(w.cursor(), (0, 5));
    }

    #[test]
    fn backspace_edits_then_joins_and_ctrl_u_yank_cycle() {
        let mut w = InputArea::default();
        w.insert_str("ab\ncd");
        assert_eq!(w.lines(), ["ab", "cd"]);
        assert_eq!(w.cursor(), (1, 2));
        // two chars deleted from the current line's tail
        press(&mut w, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut w, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(w.lines(), ["ab", ""]);
        assert_eq!(w.cursor(), (1, 0));
        // a third backspace at col 0 pulls the caret up and joins the lines
        press(&mut w, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(w.lines(), ["ab"]);
        assert_eq!(w.cursor(), (0, 2));
        // Ctrl+U kills the line head; ^Y pastes the kill back
        press(&mut w, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(w.lines(), [""]);
        assert_eq!(w.cursor(), (0, 0));
        press(&mut w, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(w.lines(), ["ab"]);
    }

    #[test]
    fn multiline_paste_splits_lines() {
        let mut w = InputArea::default();
        w.insert_str("one\ntwo\nthree");
        assert_eq!(w.lines(), ["one", "two", "three"]);
        assert_eq!(w.cursor(), (2, 5));
        w.insert_str("!");
        assert_eq!(w.lines()[2], "three!");
    }

    #[test]
    fn insert_newline_splits_at_caret() {
        let mut w = InputArea::default();
        w.insert_str("abcd");
        press(&mut w, KeyCode::Left, KeyModifiers::NONE);
        press(&mut w, KeyCode::Left, KeyModifiers::NONE);
        w.insert_newline();
        assert_eq!(w.lines(), ["ab", "cd"]);
        assert_eq!(w.cursor(), (1, 0));
    }

    #[test]
    fn arrows_clamp_at_buffer_edges() {
        let mut w = InputArea::default();
        w.insert_str("ab\ncdef");
        press(&mut w, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (0, 2)); // col clamped to the upper row's width
        press(&mut w, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (0, 2)); // stays on the first row
        press(&mut w, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (1, 2)); // col preserved across rows
        press(&mut w, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (1, 2)); // stays on the last row
    }

    #[test]
    fn word_ops_move_and_kill() {
        let mut w = InputArea::default();
        w.insert_str("one two three");
        press(&mut w, KeyCode::Char('a'), KeyModifiers::CONTROL); // caret → (0,0)
                                                                  // Alt+F walks word-HEAD forward: "one" → "two" → "three"
        press(&mut w, KeyCode::Char('f'), KeyModifiers::ALT);
        assert_eq!(w.cursor(), (0, 4));
        press(&mut w, KeyCode::Char('f'), KeyModifiers::ALT);
        assert_eq!(w.cursor(), (0, 8));
        // Alt+B walks back to the head of "two"
        press(&mut w, KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(w.cursor(), (0, 4));
        assert_eq!(w.lines(), ["one two three"]);
        // Alt+D kills forward over "two"
        press(&mut w, KeyCode::Char('d'), KeyModifiers::ALT);
        assert_eq!(w.lines(), ["one  three"]);
        // Ctrl+W kills back over the word left of the caret
        let mut w2 = InputArea::default();
        w2.insert_str("one two three");
        press(&mut w2, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(w2.lines(), ["one two "]);
    }

    #[test]
    fn unicode_line_measured_in_chars() {
        let mut w = InputArea::default();
        w.insert_str("жаба");
        assert_eq!(w.cursor(), (0, 4));
        press(&mut w, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(w.cursor(), (0, 0));
        press(&mut w, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (0, 4));
    }

    #[test]
    fn delete_at_end_of_line_joins_next_line() {
        let mut w = InputArea::default();
        w.insert_str("ab\ncd");
        // Up parks the caret at the end of the FIRST line ("ab")
        press(&mut w, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (0, 2));
        // Delete at end-of-line pulls the next line up
        press(&mut w, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(w.lines(), ["abcd"]);
        assert_eq!(w.cursor(), (0, 2));
    }

    #[test]
    fn ctrl_u_at_head_of_non_first_line_joins_previous_line() {
        let mut w = InputArea::default();
        w.insert_str(
            "ab
cd",
        );
        press(&mut w, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (0, 2));
        press(&mut w, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut w, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (1, 0));
        // Ctrl+U at col 0 of a continuation line: the newline goes instead,
        // the line joins the previous one (tui-textarea 0.7 parity)
        press(&mut w, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(w.lines(), ["abcd"]);
        assert_eq!(w.cursor(), (0, 2));
    }

    #[test]
    fn ctrl_j_parity_with_ctrl_u_at_head_of_line() {
        // Ctrl+J and the public Ctrl+U path must behave identically at col 0
        let mut w = InputArea::default();
        w.insert_str(
            "ab
cd",
        );
        press(&mut w, KeyCode::Up, KeyModifiers::NONE);
        press(&mut w, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut w, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(w.cursor(), (1, 0));
        press(&mut w, KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(w.lines(), ["abcd"]);
        assert_eq!(w.cursor(), (0, 2));
    }
}
