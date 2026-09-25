# Changelog

All notable changes to this project are documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `y` copies the current chat text selection to the clipboard (the same OSC 52 + `CHIBI_TUI_COPY_CMD` path as the drag-release copy); the selection stays highlighted after copying. Without an active selection `y` keeps typing into the input as before.

### Changed

- The quit-confirmation popup is restyled as a compact rounded banner: 4 rows total (rounded corners, bold ` Quit ` title in the top border, question row + amber hints row), width hugging its content instead of stretching to half the frame. Keys, state, and isolation behavior are unchanged.
- Quitting now asks for confirmation: every manual exit path — idle `Ctrl+C`, `q`/`Ctrl+C` from the error popup, and the exit keys on the startup splash and the backend setup screen — opens a centered "Quit chibi-tui?" popup (`y`/`Enter` quit, `n`/`Esc`/`q` stay). `Ctrl+C` with an in-flight request still cancels the request instantly and never opens the popup; dismissing the confirmation restores the exact prior state (draft, focus, open popups, streaming request).
- Upgraded `ratatui` to 0.30 and `crossterm` to 0.29, closing the transitive `lru` vulnerability advisory (`lru` is now 0.18.5; Dependabot had flagged 0.12.5 as unfixable while `ratatui` 0.29 pinned it).
- Replaced the `tui-textarea` dependency with a minimal in-house readline input editor (`input.rs`): upstream has no ratatui-0.30-compatible release, and its pinned `ratatui 0.29` would have kept the vulnerable `lru` in the tree. The editor surface (readline keybindings, multi-line buffer, caret-following scroll, kill ring) matches the old behavior; `Ctrl+U` keeps readline kill-to-head semantics, as before.

### Fixed

- Selection copy (chat drag-release and `y`, plus the log viewer's `y`) now actually reaches the system clipboard on terminals that ignore OSC 52 (e.g. iTerm2 with its default settings, Terminal.app): copies are additionally written through the `arboard` system clipboard, alongside the existing OSC 52 escape and the `CHIBI_TUI_COPY_CMD` fallback — the copy succeeds when any transport delivers.

## [0.2.0] - 2026-09-22

### Added

- Mouse wheel scrolling for the chat view, the focused sidebar, and the log viewer / help modal / model picker surfaces.
- Mouse text selection in the chat view: drag to highlight, release to copy the plain text to the clipboard.
- Ctrl-chords now work under Russian and Ukrainian keyboard layouts (plain-letter hotkeys remain Latin-only).

### Fixed

- Ctrl+↑/Ctrl+↓ thread switching works on Windows Terminal again, as the kitty keyboard protocol is now requested only on platforms where it works.

## [0.1.0] - 2026-08-25

### Added

- Initial release: a terminal UI client for the Chibi AI assistant speaking IDE protocol v1 (JSONL via stdio), with a live backend featuring auto-reconnect plus a mock mode for development. Markdown rendering with syntax highlighting, persistent local thread history, thread management and model picking, all wrapped in a Tokyo Night theme.
