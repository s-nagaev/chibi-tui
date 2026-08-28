# Changelog

All notable changes to this project are documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Status strip (`Ctrl+O`): a hideable dim one-row `cwd: <workspace> · <model>`
  readout on the chat header's top border — right-aligned, zero vertical cost,
  truncated with an ellipsis on narrow terminals, hidden by default. The model
  segment reuses the per-message model metadata (updates on result resolution,
  per-chat across switches, `—` placeholder when unknown); the workspace cwd is
  the `--workspace` basename. Hints bar gained the permanent `^O info` token at
  a net-zero width change (`^D del`/`^T panel` compacted to bare `^D`/`^T`), so
  the 120-column contract with the longest status label still holds. Designed
  to extend with a context-size segment once the protocol reports real usage.
- Model label in the assistant header: when the backend's `result` frame
  carries the optional `model`/`provider` fields, the answer's header renders
  as `● Chibi (model)` with a dim parenthetical (model preferred, provider as
  fallback); fieldless frames and restored history keep the plain `● Chibi`.
  Labels attach per message; a long model name wraps safely with the
  row-accurate scroll math. The label is session-scoped — the history file
  format is unchanged.
- Growing input block: the editor area now expands from 1 up to 20 rows with
  the multiline draft (`Shift+Enter`), squeezing the chat pane; past 20 lines
  the view auto-follows the caret. The `⏎ send` chip moves to the first row of
  a grown block, the sidebar divider stays unbroken at every height, and the
  rename editor grows equally with multiline title drafts.
- Multi-line input (`Shift+Enter` / `Alt+Enter` insert a newline; bare `Enter`
  still sends). chibi-tui requests the kitty keyboard protocol at startup and
  pops it on exit; on terminals without support Shift+Enter degrades to
  send-on-Enter (documented in the README). While renaming, modified
  Enters insert newlines into the title draft instead of saving.
- Inline thread rename (`Ctrl+R`): bottom input line becomes a single-line
  editor prefilled with the current title; `Enter` saves (trimmed, empty
  rejected), `Esc` cancels keeping the message draft; works on busy threads;
  renamed titles persist across restarts.

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
