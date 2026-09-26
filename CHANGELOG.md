# Changelog

All notable changes to this project are documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Refactored test organization: the flat inline test blobs in `main.rs`, `app.rs`, and `ui.rs` are split into per-topic sibling test modules (`keymap/tests.rs`, `mouse/tests.rs`, `app/tests/`, `ui/tests/`, `tests.rs` + `tests/`), and the keyboard dispatch and mouse handling were extracted from `main.rs` into dedicated `keymap` and `mouse` modules. No behavior changes.

## [0.3.0] - 2026-09-26

### Added

- `y` copies the chat text selection to the clipboard; the selection stays active.
- Quit confirmation before every manual exit: `y`/`Enter` quit, `n`/`Esc`/`q` stay; `Ctrl+C` with an in-flight request still cancels it.

### Changed

- The sidebar and the chat pane are outlined with full rounded borders; titles, status strip, focus, and mouse hit-testing unchanged.
- The quit-confirmation popup is restyled as a compact rounded banner.
- Upgraded `ratatui` to 0.30 and `crossterm` to 0.29 (closes the transitive `lru` advisory); replaced `tui-textarea` with an in-house readline editor.

### Fixed

- Copies reach the system clipboard on terminals that ignore OSC 52 (`arboard` fallback).
- Newlines render in multi-line chat bubbles.
- Multi-line paste no longer submits the prompt once per line (pinned with wire tests).

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
