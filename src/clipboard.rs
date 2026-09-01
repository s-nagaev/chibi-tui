//! Clipboard plumbing for the TUI.
//!
//! Reading (paste into the input field) goes through `arboard`. Writing
//! (the `y` key in the ^G log viewer) is dependency-free on purpose: the
//! primary transport is an OSC 52 escape sequence, which most modern
//! terminals understand, with an optional external command as fallback.
//!
//! OSC 52 format used here: `ESC ] 52 ; c ; <base64 of utf-8 payload> BEL`
//! (`\x1b]52;c;<b64>\x07`). The `c` selection is the clipBOARD equivalent.
//! Note that a terminal can silently ignore the sequence (for example iTerm2
//! before the "Applications in terminal may access clipboard" pref is
//! enabled), and the app has no way to detect that, so the escape is
//! fire-and-forget by design.

use std::io::Write;

/// Outcome of a copy attempt, mapped to the viewer header feedback
/// (`copied` vs `copy: unavailable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    /// At least one transport accepted the payload.
    Copied,
    /// Every available transport failed, or there was nothing to write to.
    Unavailable,
}

/// Build the OSC 52 escape sequence for `text` (standard base64, padded).
pub fn osc52_sequence(text: &str) -> String {
    format!("\u{1b}]52;c;{}\u{7}", base64(text.as_bytes()))
}

/// Minimal standard base64 encoder (RFC 4648, with `=` padding). Kept local
/// so the copy path stays free of a new dependency.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Pipe `text` into the stdin of `cmd` (run through `sh -c`, so values like
/// `pbcopy` or `tee /tmp/x` both work). Drops stdin afterwards so the child
/// sees EOF, then reaps it.
fn pipe_to_cmd(cmd: &str, text: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
        // Dropping stdin closes the pipe: `cat`-style readers finish.
        drop(stdin);
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "copy command failed: {status}"
        )))
    }
}

/// Copy `text` to the system clipboard, testable form.
///
/// `out` receives the OSC 52 bytes (production passes stdout, tests pass a
/// buffer). `cmd` is the `CHIBI_TUI_COPY_CMD` fallback, if set: the payload
/// is piped to its stdin as well. Both transports are attempted when
/// available; the copy counts as done when at least one succeeded, since a
/// terminal that silently ignores OSC 52 cannot be detected in-band.
pub fn copy_text_with(text: &str, out: &mut dyn Write, cmd: Option<&str>) -> CopyOutcome {
    let mut ok = out
        .write_all(osc52_sequence(text).as_bytes())
        .and_then(|_| out.flush())
        .is_ok();
    if let Some(cmd) = cmd.map(str::trim).filter(|c| !c.is_empty()) {
        if pipe_to_cmd(cmd, text).is_ok() {
            ok = true;
        }
    }
    if ok {
        CopyOutcome::Copied
    } else {
        CopyOutcome::Unavailable
    }
}

/// Production entry point: OSC 52 to stdout, plus the `CHIBI_TUI_COPY_CMD`
/// fallback when the env var is set to a non-empty value.
pub fn copy_text(text: &str) -> CopyOutcome {
    let cmd = std::env::var_os("CHIBI_TUI_COPY_CMD")
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|v| !v.trim().is_empty());
    let mut out = std::io::stdout().lock();
    copy_text_with(text, &mut out, cmd.as_deref())
}

/// Read text from the system clipboard. Returns `None` when the clipboard is
/// unavailable, empty, or holds non-text content.
pub fn get_text() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get_text().ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: reading the clipboard must never panic; it may legitimately
    /// return either side depending on the host environment.
    #[test]
    fn get_text_never_panics() {
        let _ = get_text();
    }

    /// The OSC 52 wire format: ESC ] 52 ; c ; <base64> BEL, with standard
    /// padded base64 over the utf-8 bytes (so multibyte payloads survive).
    #[test]
    fn osc52_sequence_format() {
        assert_eq!(osc52_sequence("hi"), "\u{1b}]52;c;aGk=\u{7}");
        assert_eq!(osc52_sequence("h\u{e9}llo"), "\u{1b}]52;c;aMOpbGxv\u{7}");
    }

    /// Primary transport: the OSC 52 bytes land in the given writer and the
    /// copy counts as done.
    #[test]
    fn copy_via_osc52_writes_sequence() {
        let mut sink = Vec::new();
        let outcome = copy_text_with("log line one", &mut sink, None);
        assert_eq!(outcome, CopyOutcome::Copied);
        assert_eq!(sink, osc52_sequence("log line one").into_bytes());
    }

    /// Fallback transport: with CHIBI_TUI_COPY_CMD semantics, the payload is
    /// piped to the command's stdin verbatim (tee writes it to a file we can
    /// assert on).
    #[test]
    fn copy_via_fallback_cmd_pipes_stdin() {
        let path = std::env::temp_dir().join(format!(
            "chibi_tui_copy_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cmd = format!("tee {}", path.display());
        let mut sink = Vec::new();
        let outcome = copy_text_with("piped payload", &mut sink, Some(&cmd));
        assert_eq!(outcome, CopyOutcome::Copied);
        let written = std::fs::read_to_string(&path).expect("tee wrote the file");
        assert_eq!(written, "piped payload");
        std::fs::remove_file(&path).ok();
    }

    /// Unavailable path: a dead writer AND a nonexistent command leave no
    /// working transport, so the header would show `copy: unavailable`.
    #[test]
    fn copy_unavailable_when_every_transport_fails() {
        struct Dead;
        impl Write for Dead {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("no tty in tests"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let outcome = copy_text_with("x", &mut Dead, Some("chibi_tui_no_such_binary_xyz"));
        assert_eq!(outcome, CopyOutcome::Unavailable);
        // Dead writer alone is just as unavailable.
        let outcome = copy_text_with("x", &mut Dead, None);
        assert_eq!(outcome, CopyOutcome::Unavailable);
    }
}
