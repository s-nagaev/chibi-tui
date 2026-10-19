# Changelog

All notable changes to this project are documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-10-19

### Added

- Mouse support in the backend settings editor: the wheel moves the group or row selection by the pane under the cursor, and a left click selects a group or row; mouse events are swallowed while the value editor or a confirmation prompt is open. Available in both editor shells (the F2 in-app modal and the post-onboarding screen).
- Base-surface mouse clicks: a left click on a thread in the sidebar switches to that thread (same live switching as the arrow keys) and focuses the sidebar; clicking below the last thread only moves focus, and clicks on the sidebar borders change nothing. A left click in the chat pane or the input area moves focus to the chat, alongside the existing drag-to-select text behavior.
- Failover chain settings in the config editor: `FAILOVER_CHAIN_MASTER` (main chat requests) and `FAILOVER_CHAIN_SUBAGENT` (sub-agent requests) are now editable in the Model Routing group, accepting `auto`, `disabled`, or an ordered comma-separated list of `provider/model` pairs.

## [0.5.0] - 2026-10-09

### Added

- Missing-backend onboarding: when the backend binary is absent, the setup screen probes for a usable Python (>=3.11,<4) and pipx by execution (stock macOS `/usr/bin/python3` is 3.9.6) and verifies the installed binary by running `stdio --help`.
- Install ladder: pipx → TUI-owned venv under the platform data dir → `pip --user` (only when the interpreter is not externally-managed) → `--break-system-packages` as a loudly warned last resort. Installer output streams into a live tail pane; `Esc` cancels the install and `Ctrl+C` quits anyway. The backend config screen always opens before respawning.
- The verified backend binary path persists between sessions: after a successful install the next launch skips the setup screen, and the resolved bin dir is injected into the backend child's `PATH` for automatic reconnects. The `CHIBI_BACKEND_BIN` override still wins, and a persisted path whose binary has vanished does not suppress the setup screen.
- Backend settings editor driven by a lossless dotenv engine: parse/render is byte-exact (quoting, comments, CRLF, BOM and missing trailing newline are preserved), per-row validation mirrors the backend's import rules, and saves are atomic (temp + fsync + rename, `.bak` backup, chmod 600) with an external-change guard.
- Two editor shells: an F2 modal over a live session (after a save it offers a backend restart now or when idle) and a full-screen editor that opens right after onboarding. The editor also opens on startup when no provider key is configured (skipped in mock mode and under `CHIBI_ENV=dev`). Secret values render masked and are only revealed explicitly during editing.

### Fixed

- The setup screen's pipx hint installed the wrong package (`pipx install chibi`); it now reads `pipx install chibi-bot`.

## [0.4.0] - 2026-09-28

### Changed

- Event-driven redraw with dirty-flag frame painting: the UI now repaints only on real state changes instead of a hot render loop, dropping idle CPU from 35.6% to 0.10% and cutting CPU during streaming by roughly 6.5x.
- Added a content-keyed markdown render cache: finalized chat messages are parsed and syntax-highlighted once per (content, theme) pair instead of on every frame, sharply cutting CPU while streaming or scrolling through a rich transcript. Session-only app state — the history snapshot format is untouched, and the streaming pending row keeps its plain-text render.

### Added

- Incremental streaming markdown render: completed blocks (code panels, tables) render as full markdown while the response streams, the in-progress tail stays plain text, and the final result reconciles byte-identically with a from-scratch render.
- Live response streaming: the TUI opts in to `capabilities.streaming` and renders `delta` frames as plain text on the request's pending bubble while the turn runs; the terminal `result` frame stays authoritative and overwrites the partial, late deltas after it are dropped, a mid-stream error keeps the partial text, and delta frames never trigger persistence or resolve the request lifecycle. Canonical IDE-protocol fixture set re-synced from the backend (adds `valid_streaming_session_*` and `valid_subagent_events_*`).

### Fixed

- Syntax highlighting now covers TypeScript/TSX, Swift, Kotlin, Zig, Elixir, TOML, Dockerfile, Objective-C and more via a bundled extended grammar set; previously only the ~75 default syntect grammars were available, so several mainstream languages had never been highlighted since v0.1.0.

## [0.3.1] - 2026-09-26

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
