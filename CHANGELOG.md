# Changelog

All notable changes to this project are documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Upgraded `ratatui` to 0.30 and `crossterm` to 0.29, closing the transitive `lru` vulnerability advisory (`lru` is now 0.18.5; Dependabot had flagged 0.12.5 as unfixable while `ratatui` 0.29 pinned it).
- Replaced the `tui-textarea` dependency with a minimal in-house readline input editor (`input.rs`): upstream has no ratatui-0.30-compatible release, and its pinned `ratatui 0.29` would have kept the vulnerable `lru` in the tree. The editor surface (readline keybindings, multi-line buffer, caret-following scroll, kill ring) matches the old behavior; `Ctrl+U` keeps readline kill-to-head semantics, as before.

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
