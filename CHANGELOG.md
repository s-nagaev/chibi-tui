# Changelog

All notable changes to this project are documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Keybindings help modal (`F1`): a centered popup listing every active chord
  — global keys, input editing, sidebar navigation, rename/delete
  confirmations, model picker, both search popups, the log viewer and the
  error popup — grouped and scrollable (`↑`/`↓` one line, `PgUp`/`PgDn` one
  page, clamped at the table edges over the render-fed viewport). The same
  chord or `Esc` closes; `Ctrl+C` quits as in every popup. The modal's
  content is a single const table next to its renderer, and a dispatch-side
  test suite pins that table against the real key handlers (an enumerated
  chord map plus a typing-leak probe over every Normal-mode row), so a
  chord added to the dispatch without a matching help row fails the suite.
  The status hints row advertises the modal (`F1 help`); to pay for the
  token the self-evident `↑↓ caret` hint retired (plain arrows in the
  editor move the caret — README still documents it, and the modal is now
  the on-screen reference for everything else).
- Remember the last active thread across restarts: the thread id is recorded
  in `last-thread.json` next to the threads directory on every activation
  (write-on-activation, atomic temp-file + rename), so even a hard crash
  remembers what the user was reading. On startup the app re-opens that
  thread — selection and the sticky ctx/model display state restored exactly
  as if the user had picked it — and falls back silently to the default
  startup selection when the pointer is missing, unreadable, or points to a
  thread whose history no longer exists. Honors `CHIBI_TUI_HOME` and
  `--history-dir` like all history storage; no wire or history-file format
  changes.
- Model picker (`Ctrl+M`): `PgUp`/`PgDn` page the list by one viewport of
  visible rows instead of stepping item by item. The page size is the
  popup's rendered list height (the same render-fed seam the chat pane and
  the log viewer page by), the jump is clamped at both edges (no
  wraparound, matching the arrow keys), and the landed-on row stays
  highlighted and on screen. The picker footer hint now reads
  `↑↓ navigate · PgUp/PgDn page · Enter switch · Esc close · Ctrl+C quit`.
- Last-known usage and model now persist per thread and survive a restart:
  the ctx segment in the status strip and the panel model readout are seeded
  from the thread snapshot on startup instead of starting blank until the
  first live turn. The fields are stored as optional `last_usage` /
  `last_model` keys in the thread's history file and are only written once a
  turn or a model switch actually reports them; existing history files parse
  unchanged, threads without recorded data display exactly as before, and
  per-answer model annotations no longer backfill from the thread's current
  model (see the model-switch fix below).
- Status strip (`Ctrl+O`): a hideable dim one-row `cwd: <workspace> · <model>`
  readout on the chat header's top border — right-aligned, zero vertical cost,
  truncated with an ellipsis on narrow terminals, hidden by default. The model
  segment reuses the per-message model metadata (updates on result resolution,
  per-chat across switches, `—` placeholder when unknown); the workspace cwd is
  the `--workspace` basename. Hints bar gained the permanent `^O info` token at
  a net-zero width change (`^D del`/`^T panel` compacted to bare `^D`/`^T`), so
  the 120-column contract with the longest status label still holds.
- Model label in the assistant header: when the backend's `result` frame
  carries the optional `model`/`provider` fields, the answer's header renders
  as `● Chibi (model)` with a dim parenthetical (model preferred, provider as
  fallback). Each header names ONLY the model that produced its own answer —
  captured at answer time and persisted with the snapshot — so a mid-chat
  model switch never re-labels past answers and a restart keeps every
  restored answer's own model. A header without its own label (older
  history, fieldless frames) stays the plain `● Chibi`; a thread with no
  known model keeps it too. Labels attach per message; a long model
  name wraps safely with the row-accurate scroll math.
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
- Reasoning (thoughts) display: when the backend's `result` frame carries the
  optional `thoughts` field, a dim block with the raw reasoning trace renders
  ABOVE the latest answer. The block is static text (no streaming); reasoning
  over 64 KB arrives already truncated by the backend with a visible
  `[... LLM reasoning truncated: 64 KB limit reached ...]` marker. Toggled
  with `Ctrl+S`, on by default; the block is session view state only: it is
  never written to the history file, and answers without reasoning (or with
  the toggle off) render exactly as before the feature existed.
- Context usage segment in the status strip: when the `result` frame carries
  the optional `usage` object, the `Ctrl+O` readout gains a trailing
  `· ctx 14% (18.4k/131.0k)` segment: a floored percent of input tokens
  against the backend-reported context window, with both counts humanized.
  An unknown window shows the absolute count alone (`· ctx 18.4k`), never an
  invented maximum; the segment is omitted entirely until usage arrives and
  only updates when a new result frame resolves.
- Subagent counter: while the active chat has a request in flight and the
  backend reports live subagent progress, the spinner line appends
  `· subagents working: n` (n = currently active subagents). The segment
  follows the active chat only (background work keeps showing in the sidebar
  dot) and without live subagents the line stays byte-for-byte unchanged.
- Handshake capabilities: the TUI declares `{"thoughts": true, "subagents":
  true}` during the version handshake; the protocol version is unchanged
  (1) and older backends tolerate the unknown keys.
- Reader hardening: an incoming frame with an unknown `type` tag leaves an
  `unknown frame type: <tag>` trace in the diagnostics log (`Ctrl+G`)
  instead of vanishing silently; the request lifecycle is unaffected and
  plain garbage lines are still dropped quietly.
- Log viewer level colors: stderr lines shaped
  `YYYY-MM-DD HH:MM:SS | LEVEL | message` are parsed once at ingestion and
  each diagnostics entry keeps its level next to the raw text; the `Ctrl+G`
  viewer colorizes per level (`TRACE` very dim, `DEBUG` dim gray, `INFO`
  and `SUCCESS` green, `WARNING` yellow, `ERROR` red, `CRITICAL` bold red).
  Lines without a recognizable level (old backends, malformed or non-log
  output) render exactly as before, `[tui]` lifecycle events stay dim, and
  the `CHIBI_TUI_LOG` file mirror stays plain text with no ANSI codes.

### Changed
- Backend launch command: live mode now spawns the backend as
  `chibi stdio --tui` instead of `chibi ide --stdio` (the backend removed the
  `ide` subcommand). The JSONL protocol v1 handshake, capability exchange and
  the in-frame workspace root are unchanged; the `CHIBI_FAKE_BACKEND` (test
  seam) and `CHIBI_BACKEND_BIN` (setup-screen hint gate) environment variables
  keep their roles, and reconnect respawns the same new command.

### Fixed
- Thoughts now belong to their thread: the dim reasoning block above the
  latest answer was a single app-global field that ANY chat's terminal frame
  wrote and ANY request start wiped, so a hidden model-picker exchange (or a
  background reply) erased the trace the user was reading, and switching
  threads leaked one chat's reasoning into another's view. The trace now
  lives per chat, is written only by a visible result of the owning chat
  that actually carries reasoning, is cleared only when a new visible
  request starts in THAT chat, and the renderer reads the active chat's own
  value — switching threads shows the entered chat's trace, never a
  neighbor's. Still session-only: nothing is persisted and a restart starts
  every thread without traces. Ctrl+S gained visible feedback: a `^S on/off`
  state token joined the status hints row (to pay for it the self-evident
  `⇧↵` newline hint retired and `^⇧F all` compacted to `^⇧F`; README
  documents both, and the F1 modal lists everything).
- Model-switch backfill: changing the model in the chat (`Ctrl+M`) re-labeled
  every past answer with the newly selected model, and the restore-time
  header fallback did the same for unlabeled history. Per-answer labels are
  now frozen at answer time and rendered from the message's own stored value
  only: the answering model persists additively as an optional `model` key
  per message (backward-compatible with existing history files), restored
  pre-label messages keep the plain `● Chibi` header — never invented,
  never backfilled — and the status strip / panel readout is unchanged.
- Sticky last-known display state: the `ctx` usage segment no longer loses
  its value. It used to be wiped at every request start and overwritten by
  every terminal frame, so a frame without usage (a command result or a
  hidden model-picker exchange between two answers) made the readout vanish
  until the next LLM turn. It now updates only when a frame actually reports
  usage and keeps the previous value otherwise; a fresh session still starts
  with no segment. The status strip's model readout and the per-answer
  `(model)` header annotation already followed the same last-known rules and
  are now pinned by tests: a command answer renders no annotation and leaves
  both readouts untouched, and a model change shows up with the next labelled
  result (per-answer labels themselves now persist — see the model-switch
  fix above).
- The log viewer colorizes the backend's custom log levels (`TOOL`, `THINK`,
  `CALL`, `CHECK`, `MODERATOR`, `SUBAGENT`, `DELEGATE`) instead of leaving
  them plain: each maps onto the theme slot mirroring its backend
  registration color (light-blue → blue, light-magenta and magenta →
  purple, light-red → red, cyan → cyan, blue → blue). Standard-level
  coloring is untouched and unrecognized level names keep the default
  foreground.

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
