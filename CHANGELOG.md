# Changelog

All notable changes to this project are documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-25

Initial release.

### Added
- Terminal UI client for Chibi over IDE protocol v1 (JSONL via `chibi ide --stdio`):
  spawn + version handshake, request correlation with ids, per-request cancel.
- Live backend with auto-reconnect (`R` from the error popup); backend failures
  surface as modal popups instead of crashes; connection state in the status bar.
- Mock mode (`--mock`) with canned demo chats for development and screenshots.
- Markdown rendering: headings, lists, tables, block quotes, syntax-highlighted
  code panels (syntect).
- Local chat history persisted under the platform data dir
  (`<data>/chibi-tui/threads/`) and restored on startup; corrupt files skipped.
- Calico kitten ASCII splash screen (~1.5s, skippable).
- CLI: `--workspace`, `--mock`, `--history-dir`.
- Keybindings: ↑↓/jk chat switch, `N` new chat, `Enter` send, PgUp/PgDn scroll,
  `Ctrl+C` cancel/quit, `Ctrl+V` paste, readline-style input edits
  (`Ctrl+A/E/U/L`).
- Tokyo Night inspired theme.
- Test suite: 105 tests (74 lib + 14 bin + 11 protocol fixtures + 6 integration).
