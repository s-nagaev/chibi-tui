//! Lossless dotenv engine + key registry for the backend settings file
//! (`~/chibi-bot/settings` — the file the Python backend reads with
//! `python-dotenv`'s `dotenv_values()` at startup).
//!
//! This is the pure core consumed by both the pre-loop config screen and the
//! in-app config modal. It has no App/TUI/IO-side dependencies.
//!
//! # Why hand-rolled
//!
//! The file's comments *are* the user's documentation (~520 lines of prose vs
//! ~100 data lines), so a parse→re-serialize crate would destroy information.
//! The engine keeps a line-preserving model: untouched lines are written back
//! byte-exact, only edited records are rebuilt (see [`SettingsDoc`]).
//!
//! # Parse semantics
//!
//! Semantics mirror `python-dotenv` (the exact reader the backend uses):
//! `export ` prefix, whitespace trimming around key/`=`/value, double-quoted
//! values with escapes (which may span physical lines), single quotes literal,
//! inline comments stripped only when `#` follows whitespace, bare `#` glued
//! to a value survives, `${VAR}` passed through verbatim (we never
//! interpolate), `KEY=` parses to an *empty-but-set* value, duplicate keys
//! resolve last-wins. Original EOL kind, BOM and trailing-newline presence are
//! preserved; CRLF is tolerated.
//!
//! # Edit semantics
//!
//! * Set: patch the specific record, re-quoting the value when it contains
//!   ` #`, `"`, `\`, `${`, `=`, a newline, or leading/trailing whitespace.
//! * A commented-out key is **uncommented in place** on edit (the template
//!   header contract: a changed setting must be uncommented).
//! * Clear: comment the line out (`# KEY=old-value`), never write `KEY=`
//!   (which would parse as the empty string, i.e. *set*).
//! * Append (key absent): under the matching section banner when
//!   identifiable, else at EOF, preceded by a `# <description>` comment line.
//! * Unknown file keys are tolerated, preserved verbatim and never rewritten.
//!
//! # Secrets invariant
//!
//! Values of secret keys are plaintext on disk and live in this model.
//! **No `Debug`/`Display` implementation in this module ever exposes a
//! value**: `KeyLine`, `Line` and `SettingsDoc` implement redacted manual
//! `Debug` impls, and nothing here writes to the diagnostics ring buffer or
//! any log sink. Callers must not log rendered documents either.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings file is not valid UTF-8")]
    NotUtf8,
    #[error("settings file changed on disk since it was loaded")]
    ChangedOnDisk,
    #[error("cannot determine the user home directory")]
    NoHomeDir,
}

/// File fingerprint captured on load and re-checked before save, so external
/// edits (user touching the file while the TUI is open) are never silently
/// overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: SystemTime,
    pub len: u64,
}

// ---------------------------------------------------------------------------
// Line model
// ---------------------------------------------------------------------------

/// Line terminator of a key record's last physical line (`None` = last line of
/// a file without a trailing newline).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::Crlf => "\r\n",
        }
    }
}

/// One logical record of the settings file. Untouched records keep their raw
/// bytes (`raw`) and are written back verbatim; only edited records are
/// rebuilt from their fields.
///
/// The `Debug` impl is manual and redacts `value` (secrets invariant).
#[derive(Clone)]
pub struct KeyLine {
    /// Complete raw text of the record, including EOLs of all its physical
    /// lines (a double-quoted value may span several lines).
    raw: String,
    /// Leading whitespace before the key or the `#` marker.
    pub indent: String,
    /// The line is commented out (`# KEY=…` / `#KEY=…`).
    pub commented: bool,
    /// Commented with a space after `#` (`# KEY=`) vs glued (`#KEY=`).
    pub hash_space: bool,
    /// The line uses the `export ` prefix.
    pub export: bool,
    pub key: String,
    /// Raw value text as it appears between `=` and the inline comment
    /// (unmodified; used for byte-minimal comment-out).
    raw_value: String,
    /// Inline comment text (starts with `#`), if any.
    pub inline_comment: Option<String>,
    /// Decoded value (quotes stripped, escapes resolved, `export ` handled).
    pub value: String,
    /// EOL of the record's last physical line; `None` = no trailing newline.
    eol: Option<Eol>,
}

impl fmt::Debug for KeyLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyLine")
            .field("key", &self.key)
            .field("commented", &self.commented)
            .field(
                "value",
                &if self.value.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .finish_non_exhaustive()
    }
}

/// A line of the settings file. `Banner` marks the `# ====…` frame lines used
/// as section anchors; `Other` holds non-key, non-comment junk lines that
/// python-dotenv would ignore — they are preserved verbatim.
#[derive(Clone)]
pub enum Line {
    Blank { raw: String },
    Banner { raw: String },
    Comment { raw: String },
    Key(KeyLine),
    Other { raw: String },
}

impl fmt::Debug for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Line::Blank { .. } => f.write_str("Blank"),
            Line::Banner { .. } => f.write_str("Banner"),
            Line::Comment { .. } => f.write_str("Comment"),
            Line::Key(k) => f.debug_tuple("Key").field(k).finish(),
            Line::Other { .. } => f.write_str("Other"),
        }
    }
}

/// State of a registry key in the file, as the loader sees it.
pub enum ValueState {
    /// An active `KEY=…` line exists (last one wins). `value` may be empty
    /// (empty ≠ absent for the backend: `KEY=` parses to `''`, which *is* set).
    Active { value: String },
    /// No active line; a commented-out `# KEY=…` line exists (its decoded
    /// value, when parseable, is carried for display).
    Commented { value: Option<String> },
    /// The key does not appear in the file at all.
    Absent,
}

impl fmt::Debug for ValueState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted on purpose: the value may be a secret (secrets invariant).
        match self {
            ValueState::Active { value } => f
                .debug_struct("Active")
                .field(
                    "value",
                    &if value.is_empty() {
                        "<empty>"
                    } else {
                        "<redacted>"
                    },
                )
                .finish(),
            ValueState::Commented { value: Some(_) } => f
                .debug_struct("Commented")
                .field("value", &"<redacted>")
                .finish(),
            ValueState::Commented { value: None } => f.write_str("Commented { value: None }"),
            ValueState::Absent => f.write_str("Absent"),
        }
    }
}

impl PartialEq for ValueState {
    fn eq(&self, other: &ValueState) -> bool {
        match (self, other) {
            (ValueState::Active { value: a }, ValueState::Active { value: b }) => a == b,
            (ValueState::Commented { value: a }, ValueState::Commented { value: b }) => a == b,
            (ValueState::Absent, ValueState::Absent) => true,
            _ => false,
        }
    }
}
impl Eq for ValueState {}

/// Line-preserving document: parse → edit → render, with byte-exact
/// round-trips for untouched lines.
#[derive(Clone)]
pub struct SettingsDoc {
    lines: Vec<Line>,
    bom: bool,
}

impl fmt::Debug for SettingsDoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted on purpose: `raw` bytes of key lines may hold secrets.
        f.debug_struct("SettingsDoc")
            .field("lines", &self.lines.len())
            .finish()
    }
}

impl SettingsDoc {
    /// Parse a settings file body. Infallible: every physical line lands in
    /// some variant, so a render of an unedited document is byte-identical to
    /// the input (BOM, CRLF and trailing-newline included).
    pub fn parse(text: &str) -> SettingsDoc {
        let (bom, rest) = match text.strip_prefix('\u{feff}') {
            Some(r) => (true, r),
            None => (false, text),
        };
        let phys = split_physical(rest);
        let mut lines = Vec::with_capacity(phys.len());
        let mut i = 0;
        while i < phys.len() {
            let pl = &phys[i];
            let trimmed = pl.text.trim();
            if trimmed.is_empty() {
                lines.push(Line::Blank {
                    raw: pl.raw.clone(),
                });
                i += 1;
                continue;
            }
            if trimmed.starts_with('#') {
                let content = trimmed.trim_start_matches('#').trim();
                if !content.is_empty() && content.chars().all(|c| c == '=') {
                    lines.push(Line::Banner {
                        raw: pl.raw.clone(),
                    });
                } else if let Some(start) = try_parse_key_line(&pl.text, pl.eol, true) {
                    lines.push(Line::Key(start.line));
                } else {
                    lines.push(Line::Comment {
                        raw: pl.raw.clone(),
                    });
                }
                i += 1;
                continue;
            }
            match try_parse_key_line(&pl.text, pl.eol, false) {
                Some(start) if start.multiline_open => {
                    let (record, consumed) = fold_multiline(&phys, i, start);
                    lines.push(Line::Key(record));
                    i += consumed;
                }
                Some(kl) => {
                    lines.push(Line::Key(kl.line));
                    i += 1;
                }
                None => {
                    lines.push(Line::Other {
                        raw: pl.raw.clone(),
                    });
                    i += 1;
                }
            }
        }
        SettingsDoc { lines, bom }
    }

    /// Serialize back. Untouched lines contribute their raw bytes verbatim.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.bom {
            out.push('\u{feff}');
        }
        for line in &self.lines {
            match line {
                Line::Blank { raw }
                | Line::Banner { raw }
                | Line::Comment { raw }
                | Line::Other { raw } => out.push_str(raw),
                Line::Key(k) => out.push_str(&k.raw),
            }
        }
        out
    }

    /// Loader-visible state of `key` (last active occurrence wins; commented
    /// lines are only reported when no active line exists).
    pub fn get(&self, key: &str) -> ValueState {
        let mut last_active: Option<String> = None;
        let mut last_commented: Option<String> = None;
        for line in &self.lines {
            if let Line::Key(k) = line {
                if k.key == key {
                    if k.commented {
                        last_commented.get_or_insert_with(|| k.value.clone());
                    } else {
                        last_active = Some(k.value.clone());
                    }
                }
            }
        }
        if let Some(value) = last_active {
            return ValueState::Active { value };
        }
        match last_commented {
            Some(value) => ValueState::Commented { value: Some(value) },
            None => ValueState::Absent,
        }
    }

    /// All keys that appear in the file (including commented ones), in file
    /// order, first occurrence only.
    pub fn file_keys(&self) -> Vec<&str> {
        let mut seen = Vec::new();
        for line in &self.lines {
            if let Line::Key(k) = line {
                if !seen.contains(&k.key.as_str()) {
                    seen.push(k.key.as_str());
                }
            }
        }
        seen
    }

    /// Set the value of `key`. Patch the active record if present, else
    /// uncomment the commented record in place, else append it (with
    /// `section`/`description` metadata when available).
    pub fn set_key(
        &mut self,
        key: &str,
        value: &str,
        section: Option<&str>,
        description: Option<&str>,
    ) {
        // 1. Patch the last active occurrence.
        let active = self.last_key_index(key, false);
        if let Some(idx) = active {
            self.patch_line(idx, false, value);
            return;
        }
        // 2. Uncomment-in-place the last commented occurrence.
        let commented = self.last_key_index(key, true);
        if let Some(idx) = commented {
            self.patch_line(idx, true, value);
            return;
        }
        // 3. Append under the matching section banner, else at EOF.
        self.append_key(key, value, section, description);
    }

    /// Registry-shaped convenience wrapper for [`SettingsDoc::set_key`].
    pub fn set(&mut self, def: &KeyDef, value: &str) {
        self.set_key(def.key, value, Some(def.section), Some(def.description));
    }

    /// Clear `key`: comment the active line out in place, preserving the old
    /// value text for the user's reference. Never writes `KEY=` (which would
    /// parse as the empty string, i.e. *set*). No-op when the key has no
    /// active line.
    pub fn clear(&mut self, key: &str) {
        if let Some(idx) = self.last_key_index(key, false) {
            if let Line::Key(k) = &mut self.lines[idx] {
                let body = k.rebuild_body();
                let eol = k.eol.map(|e| e.as_str()).unwrap_or("");
                k.raw = format!("{}# {}{}", k.indent, body, eol);
                k.commented = true;
                k.hash_space = true;
            }
        }
    }

    fn last_key_index(&self, key: &str, commented: bool) -> Option<usize> {
        self.lines.iter().rposition(|line| match line {
            Line::Key(k) => k.key == key && k.commented == commented,
            _ => false,
        })
    }

    fn patch_line(&mut self, idx: usize, uncomment: bool, value: &str) {
        if let Line::Key(k) = &mut self.lines[idx] {
            let serialized = serialize_value(value);
            if uncomment {
                k.commented = false;
            }
            k.raw_value = serialized.clone();
            k.value = value.to_string();
            let body = k.rebuild_body();
            k.raw = format!(
                "{}{}{}",
                k.indent,
                body,
                k.eol.map(|e| e.as_str()).unwrap_or("")
            );
        }
    }

    fn append_key(
        &mut self,
        key: &str,
        value: &str,
        section: Option<&str>,
        description: Option<&str>,
    ) {
        // Normalize: appended records need the previous last line to end with
        // a newline.
        if let Some(last) = self.lines.last_mut() {
            let raw = match last {
                Line::Blank { raw }
                | Line::Banner { raw }
                | Line::Comment { raw }
                | Line::Other { raw } => raw,
                Line::Key(k) => &mut k.raw,
            };
            if !raw.ends_with('\n') {
                raw.push('\n');
            }
        }
        let serialized = serialize_value(value);
        let record = KeyLine {
            raw: format!("{}={}\n", key, serialized),
            indent: String::new(),
            commented: false,
            hash_space: false,
            export: false,
            key: key.to_string(),
            raw_value: serialized,
            inline_comment: None,
            value: value.to_string(),
            eol: Some(Eol::Lf),
        };
        let mut block: Vec<Line> = Vec::new();
        let insertion = section.and_then(|title| self.section_insert_index(title));
        let need_blank = match insertion {
            Some(at) => at > 0 && !matches!(self.lines[at - 1], Line::Blank { .. }),
            None => self
                .lines
                .last()
                .map(|l| !matches!(l, Line::Blank { .. }))
                .unwrap_or(false),
        };
        if need_blank {
            block.push(Line::Blank {
                raw: "\n".to_string(),
            });
        }
        if let Some(desc) = description {
            block.push(Line::Comment {
                raw: format!("# {}\n", desc),
            });
        }
        block.push(Line::Key(record));
        match insertion {
            Some(at) => {
                self.lines.splice(at..at, block);
            }
            None => {
                self.lines.extend(block);
            }
        }
    }

    /// Index of the line *before which* a new key for `section` should be
    /// inserted: just after the last content line of the matching section
    /// (i.e. before the blank run that precedes the next section banner).
    fn section_insert_index(&self, title: &str) -> Option<usize> {
        let start = self.lines.iter().position(|line| {
            matches!(line, Line::Comment { raw } | Line::Banner { raw } | Line::Other { raw } if raw.contains(title))
        })?;
        // The title line sits between two frame banners; the next section
        // boundary is the banner *after* the closing frame.
        let closing_frame = self.lines[start + 1..]
            .iter()
            .position(|line| matches!(line, Line::Banner { .. }))
            .map(|off| start + 1 + off)
            .unwrap_or(self.lines.len());
        let next_banner = self.lines[closing_frame + 1..]
            .iter()
            .position(|line| matches!(line, Line::Banner { .. }))
            .map(|off| closing_frame + 1 + off)
            .unwrap_or(self.lines.len());
        let mut at = next_banner;
        while at > start + 1 && matches!(self.lines[at - 1], Line::Blank { .. }) {
            at -= 1;
        }
        Some(at)
    }
}

impl KeyLine {
    /// Body text (everything after the leading whitespace, without the `#`
    /// marker for commented records): `export KEY=value # comment`.
    fn rebuild_body(&self) -> String {
        let hash = if self.commented {
            if self.hash_space {
                "# "
            } else {
                "#"
            }
        } else {
            ""
        };
        format!(
            "{}{}{}={}{}",
            hash,
            if self.export { "export " } else { "" },
            self.key,
            self.raw_value,
            self.inline_comment
                .as_deref()
                .map(|c| format!(" {}", c))
                .unwrap_or_default(),
        )
    }
}

// ---------------------------------------------------------------------------
// Physical line splitting & dotenv parsing
// ---------------------------------------------------------------------------

struct PhysLine {
    /// Raw bytes of the line including its EOL (absent for a final line
    /// without trailing newline).
    raw: String,
    /// Line content without the EOL.
    text: String,
    /// EOL kind; `None` for a final line without trailing newline.
    eol: Option<Eol>,
}

fn split_physical(text: &str) -> Vec<PhysLine> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find('\n') {
        let (line, tail) = rest.split_at(pos);
        let eol = if line.ends_with('\r') {
            Eol::Crlf
        } else {
            Eol::Lf
        };
        out.push(PhysLine {
            raw: format!("{}\n", line),
            text: line.to_string(),
            eol: Some(eol),
        });
        rest = &tail[1..];
    }
    if !rest.is_empty() {
        out.push(PhysLine {
            raw: rest.to_string(),
            text: rest.to_string(),
            eol: None,
        });
    }
    out
}

/// Successul single-line key parse. `multiline_open` marks a double-quoted
/// value whose closing quote is on a later physical line.
struct KeyStart {
    line: KeyLine,
    multiline_open: bool,
}

/// Try to parse one physical line as a key record (`# KEY=…` style included).
/// Returns `None` when the line is not a key line (comment prose, junk, …).
fn try_parse_key_line(text: &str, eol: Option<Eol>, allow_commented: bool) -> Option<KeyStart> {
    let trimmed = text.trim_end_matches(['\r', '\n']);
    let indent_len = trimmed.len() - trimmed.trim_start().len();
    let indent = &trimmed[..indent_len];
    let rest = &trimmed[indent_len..];

    let (commented, hash_space, content) = if let Some(after_hash) = rest.strip_prefix('#') {
        if !allow_commented {
            return None;
        }
        let hash_space = after_hash.starts_with(' ');
        let content = if hash_space {
            &after_hash[1..]
        } else {
            after_hash
        };
        (true, hash_space, content)
    } else {
        (false, false, rest)
    };

    // Optional `export ` prefix.
    let (export, body) = match content.strip_prefix("export") {
        Some(after) if after.starts_with(' ') || after.starts_with('\t') => {
            (true, after.trim_start())
        }
        _ => (false, content),
    };

    // Key token: [A-Za-z_][A-Za-z0-9_]*, optional surrounding whitespace, `=`.
    let body_trimmed = body.trim_start();
    let key_end = body_trimmed
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    if key_end == 0 || !body_trimmed.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return None;
    }
    let key = &body_trimmed[..key_end];
    let after_key = body_trimmed[key_end..].trim_start();
    let raw_after_eq = after_key.strip_prefix('=')?;

    // Inline comment: `#` following whitespace, outside quotes.
    let (raw_value, inline_comment) = split_inline_comment(raw_after_eq);
    let raw_value = raw_value.trim();

    let multiline_open = raw_value.starts_with('"') && find_unescaped_quote(raw_value, 1).is_none();
    let value = if multiline_open {
        String::new() // filled by `fold_multiline`
    } else {
        decode_value(raw_value)
    };

    let line = KeyLine {
        raw: format!("{}{}", trimmed, eol.map(|e| e.as_str()).unwrap_or("")),
        indent: indent.to_string(),
        commented,
        hash_space,
        export,
        key: key.to_string(),
        raw_value: raw_value.to_string(),
        inline_comment: inline_comment.map(|s| s.trim().to_string()),
        value,
        eol,
    };
    Some(KeyStart {
        line,
        multiline_open,
    })
}

/// Fold a multiline double-quoted record starting at `phys[start]` (the
/// physical line whose value opened a `"` that never closed). Consumes
/// physical lines until the closing quote; physical line breaks inside the
/// quotes become `\n` in the decoded value — matching python-dotenv.
fn fold_multiline(phys: &[PhysLine], start: usize, opened: KeyStart) -> (KeyLine, usize) {
    let mut raw = opened.line.raw.clone();
    let mut inner = opened.line.raw_value[1..].to_string();
    let mut inline_comment = None;
    let mut eol = opened.line.eol;
    let mut j = start + 1;
    while j < phys.len() {
        let pl = &phys[j];
        raw.push_str(&pl.raw);
        match find_unescaped_quote(&pl.text, 0) {
            Some(q) => {
                inner.push('\n');
                inner.push_str(&pl.text[..q]);
                let remainder = pl.text[q + 1..].trim();
                if remainder.starts_with('#') {
                    inline_comment = Some(remainder.to_string());
                }
                eol = pl.eol;
                j += 1;
                break;
            }
            None => {
                inner.push('\n');
                inner.push_str(&pl.text);
                eol = pl.eol;
                j += 1;
            }
        }
    }
    // An unterminated quote is tolerated: python-dotenv swallows the rest of
    // the file as the value; the record simply ends at EOF.
    let mut line = opened.line;
    line.raw = raw;
    line.value = unescape(&inner);
    line.inline_comment = inline_comment;
    line.eol = eol;
    (line, j - start)
}

/// Split ` # comment` off a raw value: `#` only starts a comment when it
/// follows whitespace and is not inside quotes. A bare `#` glued to the value
/// survives (`hash#nospace`).
fn split_inline_comment(value_part: &str) -> (&str, Option<&str>) {
    let chars: Vec<char> = value_part.chars().collect();
    let mut in_double = false;
    let mut in_single = false;
    let mut prev_ws = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !in_double && !in_single && c == '#' && prev_ws {
            let byte_idx = chars[..i].iter().map(|c| c.len_utf8()).sum();
            return (&value_part[..byte_idx], Some(&value_part[byte_idx..]));
        }
        match c {
            '\\' if in_double => {
                i += 2;
                continue;
            }
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            _ => {}
        }
        prev_ws = c.is_whitespace();
        i += 1;
    }
    (value_part, None)
}

/// Byte index of the first unescaped `"` in `s` at or after byte `from`
/// (escape-aware inside double quotes).
fn find_unescaped_quote(s: &str, from: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Decode a single-line raw value (already trimmed, inline comment removed):
/// double quotes → escape processing, single quotes → literal, plain → as-is.
fn decode_value(raw_value: &str) -> String {
    if let Some(inner) = raw_value.strip_prefix('"') {
        if let Some(q) = find_unescaped_quote(raw_value, 1) {
            return unescape(&raw_value[1..q]);
        }
        // Unterminated single-line quote (defensive; multiline is folded
        // elsewhere): treat as literal.
        return inner.to_string();
    }
    if let Some(inner) = raw_value.strip_prefix('\'') {
        if raw_value.len() >= 2 && raw_value.ends_with('\'') {
            return inner[..inner.len() - 1].to_string();
        }
        return inner.to_string();
    }
    raw_value.to_string()
}

/// Resolve double-quote escape sequences the way python-dotenv does for the
/// common cases: `\n`, `\r`, `\t`, `\"`, `\'`, `\\`; any other `\x` stays
/// verbatim (backslash + `x`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Serialize a value for writing: quote (double) only when required — value
/// contains ` #`, `"`, `\`, `${`, `=`, a newline/tab, or leading/trailing
/// whitespace — escaping `\`, `"`, `\n`, `\r`, `\t` inside the quotes.
fn serialize_value(value: &str) -> String {
    let needs_quotes = value != value.trim()
        || value.contains('"')
        || value.contains('\\')
        || value.contains("${")
        || value.contains('=')
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('\t')
        || value.contains(" #")
        || value.starts_with('#');
    if !needs_quotes {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Key registry
// ---------------------------------------------------------------------------

/// Value kind of a registry key, driving client-side validation and the UI
/// editor shape. The backend validates nothing at startup and type violations
/// are fatal at import (handshake "status 1"), so validation happens here.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Secret,
    PlainStr,
    Bool,
    Enum(&'static [&'static str]),
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64 },
    Path,
    Csv,
}

/// Registry group (mirrors the template's section layout).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    /// Template §2 — provider API keys.
    ProviderKeys,
    /// Template §3 — Suno / ElevenLabs.
    MediaProviders,
    /// Template §10 (+ DEFAULT_* from §8) — model & role routing.
    ModelRouting,
    /// Core behaviour toggles & limits (§5/§6/§8/§16/§17).
    CoreBehaviour,
    /// Template §7 — model/tool restriction lists.
    Restrictions,
    /// Template §5 — agent filesystem paths.
    AgentPaths,
}

impl Group {
    pub const ALL: [Group; 6] = [
        Group::ProviderKeys,
        Group::MediaProviders,
        Group::ModelRouting,
        Group::CoreBehaviour,
        Group::Restrictions,
        Group::AgentPaths,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::ProviderKeys => "Providers · API keys",
            Group::MediaProviders => "Media providers",
            Group::ModelRouting => "Model & role routing",
            Group::CoreBehaviour => "Core behaviour",
            Group::Restrictions => "Restrictions",
            Group::AgentPaths => "Agent paths",
        }
    }

    pub fn keys(self) -> impl Iterator<Item = &'static KeyDef> {
        REGISTRY.iter().filter(move |d| d.group == self)
    }
}

/// One registry row: `(env_key, group, kind, default, description, secret,
/// commented_in_template, section)`.
#[derive(Clone, Copy)]
pub struct KeyDef {
    pub key: &'static str,
    pub group: Group,
    pub kind: Kind,
    /// Default as shipped by the template (empty string = unset).
    pub default: &'static str,
    pub description: &'static str,
    /// Rendered as a secret in the UI (masked, reveal-on-edit only).
    pub secret: bool,
    /// The key ships commented out in the template → must be uncommented on
    /// first edit.
    pub commented: bool,
    /// Template section title used to place appended keys.
    pub section: &'static str,
}

const S2: &str = "2. LLM PROVIDERS";
const S3: &str = "3. MEDIA PROVIDERS";
const S5: &str = "5. AGENT CAPABILITIES";
const S6: &str = "6. MCP CONFIGURATION";
const S7: &str = "7. WHITELISTS AND RESTRICTIONS";
const S8: &str = "8. BEHAVIOR SETTINGS";
const S10: &str = "10. PROVIDER SELECTION";
const S16: &str = "16. LOGGING AND DEBUGGING";
const S17: &str = "17. GENERAL SETTINGS";

/// The static key registry (owner-approved stripped slice, plan.md v2).
/// Explicitly excluded: `CLIENT` (force-set by the backend CLI — inert in the
/// file), telegram keys and telegram-access whitelists, storage/memory group,
/// heartbeat/influx. Unknown file keys are tolerated and never rewritten.
pub const REGISTRY: &[KeyDef] = &[
    // ---- Group 1: Providers · API keys (31 rows) ----
    secret(
        "OPENAI_API_KEY",
        Group::ProviderKeys,
        S2,
        "OpenAI API key (GPT, Whisper, DALL-E)",
    ),
    secret(
        "ANTHROPIC_API_KEY",
        Group::ProviderKeys,
        S2,
        "Anthropic API key (Claude models)",
    ),
    secret(
        "GEMINI_API_KEY",
        Group::ProviderKeys,
        S2,
        "Google Gemini API key (Gemini, Imagen)",
    ),
    secret(
        "GROK_API_KEY",
        Group::ProviderKeys,
        S2,
        "xAI API key (Grok models)",
    ),
    secret(
        "MISTRALAI_API_KEY",
        Group::ProviderKeys,
        S2,
        "Mistral API key (Mistral, Codestral)",
    ),
    secret(
        "ALIBABA_API_KEY",
        Group::ProviderKeys,
        S2,
        "Alibaba Cloud API key (Qwen, Wan)",
    ),
    secret(
        "DEEPSEEK_API_KEY",
        Group::ProviderKeys,
        S2,
        "DeepSeek API key",
    ),
    secret(
        "MINIMAX_API_KEY",
        Group::ProviderKeys,
        S2,
        "MiniMax API key",
    ),
    secret(
        "MOONSHOTAI_API_KEY",
        Group::ProviderKeys,
        S2,
        "Moonshot AI API key (Kimi)",
    ),
    secret(
        "ZHIPUAI_API_KEY",
        Group::ProviderKeys,
        S2,
        "Zhipu AI API key (GLM)",
    ),
    secret(
        "OPENROUTER_API_KEY",
        Group::ProviderKeys,
        S2,
        "OpenRouter API key",
    ),
    secret(
        "MELIOUS_API_KEY",
        Group::ProviderKeys,
        S2,
        "Melious API key",
    ),
    secret(
        "LYCEUM_API_KEY",
        Group::ProviderKeys,
        S2,
        "Lyceum API key (open models, EU)",
    ),
    secret(
        "GREENPT_API_KEY",
        Group::ProviderKeys,
        S2,
        "GreenPT API key (open models, EU)",
    ),
    secret(
        "NOVITA_AI_API_KEY",
        Group::ProviderKeys,
        S2,
        "Novita AI API key",
    ),
    secret(
        "MIMO_API_KEY",
        Group::ProviderKeys,
        S2,
        "Xiaomi MiMo API key",
    ),
    secret(
        "CHEAPERINFERENCE_API_KEY",
        Group::ProviderKeys,
        S2,
        "Cheaper Inference API key",
    ),
    secret(
        "DEEPINFRA_API_KEY",
        Group::ProviderKeys,
        S2,
        "DeepInfra API key",
    ),
    secret(
        "TOGETHER_API_KEY",
        Group::ProviderKeys,
        S2,
        "Together AI API key",
    ),
    secret(
        "FIREWORKS_API_KEY",
        Group::ProviderKeys,
        S2,
        "Fireworks AI API key",
    ),
    secret(
        "NEBIUS_API_KEY",
        Group::ProviderKeys,
        S2,
        "Nebius Token Factory API key",
    ),
    secret(
        "BASETEN_API_KEY",
        Group::ProviderKeys,
        S2,
        "Baseten API key",
    ),
    secret(
        "SAMBANOVA_API_KEY",
        Group::ProviderKeys,
        S2,
        "SambaNova API key",
    ),
    secret(
        "SILICONFLOW_API_KEY",
        Group::ProviderKeys,
        S2,
        "SiliconFlow API key",
    ),
    secret(
        "PARASAIL_API_KEY",
        Group::ProviderKeys,
        S2,
        "Parasail API key",
    ),
    secret(
        "FEATHERLESS_API_KEY",
        Group::ProviderKeys,
        S2,
        "Featherless API key",
    ),
    secret("ENTRIM_API_KEY", Group::ProviderKeys, S2, "Entrim API key"),
    secret(
        "CLOUDFLARE_API_KEY",
        Group::ProviderKeys,
        S2,
        "Cloudflare API key (chat-only)",
    ),
    secret(
        "CLOUDFLARE_ACCOUNT_ID",
        Group::ProviderKeys,
        S2,
        "Cloudflare account ID (required with the key)",
    ),
    secret(
        "CUSTOMOPENAI_API_KEY",
        Group::ProviderKeys,
        S2,
        "Custom OpenAI-compatible endpoint key",
    ),
    KeyDef {
        key: "CUSTOMOPENAI_URL",
        group: Group::ProviderKeys,
        kind: Kind::PlainStr,
        default: "http://localhost:1234/v1",
        description: "Custom OpenAI-compatible endpoint URL",
        secret: false,
        commented: false,
        section: S2,
    },
    // ---- Group 2: Media providers (2 rows) ----
    secret(
        "SUNO_API_ORG_API_KEY",
        Group::MediaProviders,
        S3,
        "Suno API key (music generation)",
    ),
    secret(
        "ELEVEN_LABS_API_KEY",
        Group::MediaProviders,
        S3,
        "ElevenLabs API key (text-to-speech)",
    ),
    // ---- Group 3: Model & role routing (12 rows) ----
    KeyDef {
        key: "DEFAULT_PROVIDER",
        group: Group::ModelRouting,
        kind: Kind::PlainStr,
        default: "",
        description: "Default LLM provider when not specified",
        secret: false,
        commented: false,
        section: S8,
    },
    KeyDef {
        key: "DEFAULT_MODEL",
        group: Group::ModelRouting,
        kind: Kind::PlainStr,
        default: "",
        description: "Default LLM model when not specified",
        secret: false,
        commented: false,
        section: S8,
    },
    plain_commented("STT_PROVIDER", S10, "Default speech-to-text provider"),
    plain_commented("STT_MODEL", S10, "Default speech-to-text model"),
    plain_commented("TTS_PROVIDER", S10, "Default text-to-speech provider"),
    plain_commented("TTS_MODEL", S10, "Default text-to-speech model"),
    plain_commented("TTS_VOICE", S10, "Default text-to-speech voice"),
    plain_commented("VISION_PROVIDER", S10, "Default vision provider"),
    plain_commented("VISION_MODEL", S10, "Default vision model"),
    plain_commented("OCR_PROVIDER", S10, "Default OCR provider"),
    plain_commented(
        "MODERATION_PROVIDER",
        S10,
        "Default content moderation provider",
    ),
    plain_commented("MODERATION_MODEL", S10, "Default content moderation model"),
    // ---- Group 4: Core behaviour (12 rows) ----
    KeyDef {
        key: "FILESYSTEM_ACCESS",
        group: Group::CoreBehaviour,
        kind: Kind::Bool,
        default: "false",
        description: "Allow the agent to access the filesystem",
        secret: false,
        commented: false,
        section: S5,
    },
    KeyDef {
        key: "ENABLE_MCP_STDIO",
        group: Group::CoreBehaviour,
        kind: Kind::Bool,
        default: "false",
        description: "Enable MCP stdio connections",
        secret: false,
        commented: false,
        section: S6,
    },
    KeyDef {
        key: "ENABLE_MCP_SSE",
        group: Group::CoreBehaviour,
        kind: Kind::Bool,
        default: "true",
        description: "Enable MCP SSE connections",
        secret: false,
        commented: false,
        section: S6,
    },
    KeyDef {
        key: "ALLOW_DELEGATION",
        group: Group::CoreBehaviour,
        kind: Kind::Bool,
        default: "true",
        description: "Allow the agent to delegate tasks to sub-agents",
        secret: false,
        commented: false,
        section: S5,
    },
    KeyDef {
        key: "TEMPERATURE",
        group: Group::CoreBehaviour,
        kind: Kind::Float { min: 0.0, max: 2.0 },
        default: "1.0",
        description: "Sampling temperature (0.0–2.0)",
        secret: false,
        commented: false,
        section: S8,
    },
    KeyDef {
        key: "MAX_TOKENS",
        group: Group::CoreBehaviour,
        kind: Kind::Int {
            min: 1,
            max: 1_000_000,
        },
        default: "32000",
        description: "Maximum number of tokens in LLM responses",
        secret: false,
        commented: false,
        section: S8,
    },
    KeyDef {
        key: "RETRIES",
        group: Group::CoreBehaviour,
        kind: Kind::Int { min: 0, max: 100 },
        default: "3",
        description: "Retry attempts for failed API calls",
        secret: false,
        commented: false,
        section: S8,
    },
    KeyDef {
        key: "TIMEOUT",
        group: Group::CoreBehaviour,
        kind: Kind::Int {
            min: 1,
            max: 86_400,
        },
        default: "180",
        description: "API call timeout in seconds",
        secret: false,
        commented: false,
        section: S8,
    },
    KeyDef {
        key: "SHOW_LLM_THOUGHTS",
        group: Group::CoreBehaviour,
        kind: Kind::Bool,
        default: "false",
        description: "Show LLM reasoning/thought process",
        secret: false,
        commented: false,
        section: S16,
    },
    KeyDef {
        key: "LOG_PROMPT_DATA",
        group: Group::CoreBehaviour,
        kind: Kind::Bool,
        default: "false",
        description: "Log prompt data for debugging",
        secret: false,
        commented: false,
        section: S16,
    },
    KeyDef {
        key: "PUBLIC_MODE",
        group: Group::CoreBehaviour,
        kind: Kind::Bool,
        default: "false",
        description: "Enable public mode",
        secret: false,
        commented: false,
        section: S17,
    },
    KeyDef {
        key: "PROXY",
        group: Group::CoreBehaviour,
        kind: Kind::PlainStr,
        default: "",
        description: "General proxy URL for all API calls",
        secret: false,
        commented: true,
        section: S17,
    },
    // ---- Group 4b: Restrictions (4 rows, owner 2026-10-03) ----
    csv("MODELS_WHITELIST", "Comma-separated list of allowed models"),
    csv(
        "MODELS_BLACKLIST",
        "Comma-separated list of disallowed models",
    ),
    csv("TOOLS_WHITELIST", "Comma-separated list of allowed tools"),
    csv(
        "IMAGE_GENERATIONS_WHITELIST",
        "Comma-separated user IDs allowed to generate images",
    ),
    // ---- Group 5: Agent paths (3 rows) ----
    path("HOME_DIR", "Agent home directory"),
    path("WORKING_DIR", "Agent working directory"),
    path("SKILLS_DIR", "Directory with user custom skills"),
];

/// Keys that are NOT in the v1 UI registry but MUST be written `true` by the
/// generated template (owner decision 2026-10-03). Template-only: the backend
/// reads them from the file even though the config screen does not list them.
pub const TEMPLATE_ONLY_FORCED_DEFAULTS: &[(&str, &str)] = &[("ENABLE_INFINITE_CONTEXT", "true")];

const fn secret(
    key: &'static str,
    group: Group,
    section: &'static str,
    description: &'static str,
) -> KeyDef {
    KeyDef {
        key,
        group,
        kind: Kind::Secret,
        default: "",
        description,
        secret: true,
        commented: false,
        section,
    }
}

const fn plain_commented(
    key: &'static str,
    section: &'static str,
    description: &'static str,
) -> KeyDef {
    KeyDef {
        key,
        group: Group::ModelRouting,
        kind: Kind::PlainStr,
        default: "",
        description,
        secret: false,
        commented: true,
        section,
    }
}

const fn csv(key: &'static str, description: &'static str) -> KeyDef {
    KeyDef {
        key,
        group: Group::Restrictions,
        kind: Kind::Csv,
        default: "",
        description,
        secret: false,
        commented: false,
        section: S7,
    }
}

const fn path(key: &'static str, description: &'static str) -> KeyDef {
    KeyDef {
        key,
        group: Group::AgentPaths,
        kind: Kind::Path,
        default: "",
        description,
        secret: false,
        commented: false,
        section: S5,
    }
}

pub fn registry_get(key: &str) -> Option<&'static KeyDef> {
    REGISTRY.iter().find(|d| d.key == key)
}

// ---------------------------------------------------------------------------
// Validation (client-side; the backend validates nothing at startup)
// ---------------------------------------------------------------------------

/// A per-row validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub key: &'static str,
    pub message: String,
}

/// Validate one value against its registry kind.
pub fn validate_value(kind: &Kind, value: &str) -> Result<(), String> {
    match kind {
        Kind::Secret | Kind::PlainStr => Ok(()),
        Kind::Bool => {
            let v = value.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "true" | "false" | "1" | "0" | "yes" | "no") {
                Ok(())
            } else {
                Err(format!(
                    "expected a boolean (true/false/1/0/yes/no), got {:?}",
                    value
                ))
            }
        }
        Kind::Enum(options) => {
            if options.contains(&value.trim()) {
                Ok(())
            } else {
                Err(format!("expected one of {:?}, got {:?}", options, value))
            }
        }
        Kind::Int { min, max } => match value.trim().parse::<i64>() {
            Ok(n) if n >= *min && n <= *max => Ok(()),
            Ok(n) => Err(format!("{} is out of range {}..={}", n, min, max)),
            Err(_) => Err(format!("expected an integer, got {:?}", value)),
        },
        Kind::Float { min, max } => match value.trim().parse::<f64>() {
            Ok(f) if f.is_finite() && f >= *min && f <= *max => Ok(()),
            Ok(f) => Err(format!("{} is out of range {}..={}", f, min, max)),
            Err(_) => Err(format!("expected a number, got {:?}", value)),
        },
        Kind::Csv => {
            let v = value.trim();
            if v.is_empty() {
                return Ok(());
            }
            if v.split(',').any(|item| item.trim().is_empty()) {
                Err("comma-separated list contains an empty item".to_string())
            } else {
                Ok(())
            }
        }
        Kind::Path => {
            let v = value.trim();
            if v.is_empty() {
                return Err("path must not be empty".to_string());
            }
            if v.contains('\0') {
                return Err("path contains a NUL byte".to_string());
            }
            if v.starts_with('~') && dirs::home_dir().is_none() {
                return Err("cannot expand ~: home directory unknown".to_string());
            }
            Ok(())
        }
    }
}

/// Validate all registry keys currently set in the document. Returns one
/// structured error per failing row; empty `Vec` = valid.
pub fn validate_doc(doc: &SettingsDoc) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for def in REGISTRY {
        if let ValueState::Active { value } = doc.get(def.key) {
            if let Err(message) = validate_value(&def.kind, &value) {
                errors.push(ValidationError {
                    key: def.key,
                    message,
                });
            }
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// Template generation (missing-file path)
// ---------------------------------------------------------------------------

/// Vendored canonical copy of the backend template (`DEFAULT_CONFIG` body of
/// `chibi/config_generator.py`, with the f-string placeholders intact).
/// Provenance (source branch + commit) is recorded in the fixture header,
/// which generation strips. Never edit the body here: fix upstream, re-copy.
pub const VENDORED_TEMPLATE: &str = include_str!("../tests/fixtures/backend_settings_template");

fn strip_provenance_header(template: &str) -> &str {
    const BODY_START: &str = "\n# Chibi Agent Configuration File\n";
    if template.starts_with(&BODY_START[1..]) {
        return template;
    }
    match template.find(BODY_START) {
        Some(idx) => &template[idx + 1..],
        None => template,
    }
}

/// Literal `~/chibi-bot` base directory — the backend hardcodes it (no XDG).
pub fn settings_base_dir() -> Result<PathBuf, SettingsError> {
    dirs::home_dir()
        .map(|home| home.join("chibi-bot"))
        .ok_or(SettingsError::NoHomeDir)
}

/// Path of the backend settings file: `~/chibi-bot/settings` (no extension).
pub fn settings_path() -> Result<PathBuf, SettingsError> {
    Ok(settings_base_dir()?.join("settings"))
}

/// Create the sibling directory structure the backend generator creates
/// (`chibi-bot/{data,skills,home}`), mirroring `config_generator.py:551-557`.
pub fn ensure_chibi_bot_dirs(base: &Path) -> std::io::Result<()> {
    for dir in [
        base.to_path_buf(),
        base.join("data"),
        base.join("skills"),
        base.join("home"),
    ] {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Render the generated settings file body: interpolate the four path
/// placeholders (LOCAL_DATA_PATH, HOME_DIR, WORKING_DIR, SKILLS_DIR — note the
/// template uses the HOME_DIR placeholder for WORKING_DIR too, mirrored
/// verbatim) and force the three defaults to `true` in the generated output
/// only (FILESYSTEM_ACCESS, ENABLE_MCP_STDIO, ENABLE_INFINITE_CONTEXT).
pub fn generate_settings_text(base: &Path) -> String {
    let template = strip_provenance_header(VENDORED_TEMPLATE);
    let interpolated = template
        .replace(
            "{DATA_DIR.absolute()}",
            &base.join("data").display().to_string(),
        )
        .replace(
            "{HOME_DIR.absolute()}",
            &base.join("home").display().to_string(),
        )
        .replace(
            "{SKILLS_DIR.absolute()}",
            &base.join("skills").display().to_string(),
        );
    let mut doc = SettingsDoc::parse(&interpolated);
    let mut forced: Vec<(&str, &str)> =
        vec![("FILESYSTEM_ACCESS", "true"), ("ENABLE_MCP_STDIO", "true")];
    forced.extend(TEMPLATE_ONLY_FORCED_DEFAULTS.iter().copied());
    for (key, value) in forced {
        doc.set_key(key, value, None, None);
    }
    doc.render()
}

/// Write the generated settings file (atomically, mode 0600 on Unix) when the
/// file does not exist yet, creating the sibling directories. Returns `true`
/// when the file was written, `false` when it already existed.
pub fn write_generated_settings(path: &Path) -> std::io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let base = path.parent().unwrap_or_else(|| Path::new("/"));
    ensure_chibi_bot_dirs(base)?;
    let text = generate_settings_text(base);
    save_settings(&SettingsDoc::parse(&text), path, None)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Atomic write pipeline
// ---------------------------------------------------------------------------

/// File fingerprint captured on load.
pub fn stat_settings(path: &Path) -> std::io::Result<Option<FileMeta>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(FileMeta {
            mtime: meta.modified()?,
            len: meta.len(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Load and parse the settings file, capturing the external-change guard.
pub fn load_settings(path: &Path) -> Result<(SettingsDoc, FileMeta), SettingsError> {
    let bytes = fs::read(path)?;
    let meta = stat_settings(path)?.ok_or_else(|| {
        SettingsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file vanished",
        ))
    })?;
    let text = String::from_utf8(bytes).map_err(|_| SettingsError::NotUtf8)?;
    Ok((SettingsDoc::parse(&text), meta))
}

/// Atomic save: write `settings.tmp` in the same directory → fsync → copy the
/// current file to `settings.bak` → rename → chmod 600 (Unix; documented no-op
/// on Windows, which uses ACLs instead of mode bits) → re-stat. When `guard`
/// is `Some`, the save refuses with [`SettingsError::ChangedOnDisk`] if the
/// file's mtime/len changed since load; pass `None` to overwrite anyway (the
/// UI maps the error to reload/overwrite/cancel).
pub fn save_settings(
    doc: &SettingsDoc,
    path: &Path,
    guard: Option<FileMeta>,
) -> Result<FileMeta, SettingsError> {
    if let Some(expected) = guard {
        if let Some(current) = stat_settings(path)? {
            if current != expected {
                return Err(SettingsError::ChangedOnDisk);
            }
        }
    }
    let dir = path.parent().ok_or_else(|| {
        SettingsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "settings path has no parent directory",
        ))
    })?;
    let tmp = dir.join("settings.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(doc.render().as_bytes())?;
        f.sync_all()?;
    }
    if path.exists() {
        let backup = dir.join("settings.bak");
        fs::copy(path, &backup)?;
    }
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    {
        // chmod 600 is a no-op on Windows: mode bits do not exist there;
        // NTFS ACLs govern access and are intentionally not managed here.
    }
    stat_settings(path)?.ok_or_else(|| {
        SettingsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "settings file vanished right after save",
        ))
    })
}

#[cfg(test)]
mod tests;
