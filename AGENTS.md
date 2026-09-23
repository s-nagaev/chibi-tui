# AGENTS.md

## What this is

`chibi-tui` is a Rust terminal UI client for [Chibi](https://github.com/s-nagaev/chibi) —
an AI assistant. It is a pure client: it spawns the Python backend as
`chibi stdio --tui` and talks IDE protocol v1 (JSONL over stdio); the
workspace root travels inside each request frame
(`ClientMessage::Request.workspace_root`), never on the backend command
line. Stack: ratatui + crossterm + tokio, MIT.

## Commands

```bash
cargo test                                     # unit + fixture + integration + doc tests
cargo fmt --check                              # formatting gate
cargo clippy --all-targets -- -D warnings      # lint gate (warnings are errors)
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items   # docs gate
cargo build --release                          # binary at target/release/chibi-tui
```

- `cargo test` must be fully green before any commit. CI (`ci.yml`) runs
  fmt, clippy with `-D warnings`, the docs gate, and tests — all WITHOUT
  `--locked`; the release workflow (`release.yml`) runs all cargo commands
  WITH `--locked`. `Cargo.lock` is committed and kept in sync with
  `Cargo.toml` changes deliberately via a normal build/update commit.
- Live run: `cargo run -- --workspace <dir>` (optional, defaults to the
  process cwd) — `--workspace` is the TUI's own flag, its value only sent
  to the backend inside request frames; the spawned command is plain
  `chibi stdio --tui`. Backend-binary seam: the pipeline spawns
  `CHIBI_FAKE_BACKEND` if set, else `chibi` from `$PATH` — do not confuse
  it with `CHIBI_BACKEND_BIN`, which only gates the setup screen's
  override hint. Demo run: `cargo run -- --mock` (no backend process;
  history still persists).
- Integration tests spawn `tests/fake_backend.py` by RELATIVE path — run
  `cargo test` from the repo root; the fake backend is stdlib-only Python 3
  and needs `python3` on `PATH`.

## Architecture

The library (`lib.rs`) holds the testable core; `main.rs` is the
runtime/controller layer — tokio event loop, keyboard/mode dispatch,
submit/queue handling, persistence calls, last-thread tracking, many tests.
UI code never talks to processes or sockets — it consumes `BackendEvent`
streams from `crate::backend`, so the backend source (mock ↔ live) is
swappable without UI changes.

```text
src/
├── main.rs             runtime/controller: tokio event loop, keyboard & mode dispatch, submit/queue, persistence + last-thread tracking, CLI
├── lib.rs              library root; module tree + public re-exports
├── protocol.rs         serde types of IDE protocol v1 (internally tagged; ProtocolVersion parses only the literal 1)
├── backend_client.rs   child-process lifecycle: spawn, handshake, JSONL framing, stderr → diag log
├── request_pipeline.rs request correlation, targeted cancel, status/agent_event broadcast, reconnect; one owner actor task
├── live.rs             LiveBackend: real backend source; frames → BackendEvent; wire ids: thread = i64 UUID hash, request = u64 stand-in
├── backend.rs          Backend trait + MockBackend + BackendEvent — the seam the UI consumes
├── history.rs          persistence: one JSON per thread + last-thread.json pointer
├── app.rs              App state: chats, selection, input, connection, per-thread mirrors, sticky display state, per-chat subagent counters
├── markdown.rs         pulldown-cmark → styled ratatui lines (+ syntect highlighting)
├── mock.rs             static mock conversations and replies for --mock
├── model.rs            Message / Role / ChatStatus / ChatLifecycle types
├── model_picker.rs     parser for the backend's textual /model listing (no dedicated protocol — plain chat pipeline)
├── popup.rs            modal error popup (R reconnect · Esc dismiss · q/Ctrl+C quit)
├── splash.rs           ASCII kitten splash screen (~1.5 s, skippable)
├── setup_screen.rs     startup screen with install hints when the backend binary is missing
├── clipboard.rs        arboard-backed clipboard paste into the input (Ctrl+V / Cmd+V)
├── diag.rs             diagnostics ring buffer (512 lines) + optional CHIBI_TUI_LOG file sink; parses ` | LEVEL | `
├── theme.rs            Tokyo Night palette + syntect global syntax state
└── ui.rs               layout & rendering: sidebar, chat view, input, status strip, log viewer, popups
```

`README.md` is partially stale — do not learn behavior from it: its
Architecture list is missing `model_picker.rs` / `setup_screen.rs` /
`clipboard.rs` / `diag.rs`, and its "Model label" / status-strip sections
predate persistence (labels are persisted per thread as `last_model`, not
session-scoped). Fix the README when touching those areas.

## Behavioral contracts — do not break

- **Sticky display state.** The status-strip `ctx …` segment is a
  last-known value: re-seeded from the entered chat's `last_usage` on every
  thread activation and updated only when a `result` frame with `usage`
  resolves (a frame without `usage`/`model` must not wipe anything). The
  model label is NOT seeded — it is derived per render frame from the
  active chat (`picker_model_labels` override first, then the newest
  message carrying a label). A result resolution stamps the label onto the
  new message; a Ctrl+M picker confirmation stages the override (and
  stamps `last_model`) immediately. A new request (or a dequeue) keeps ctx
  and model but clears THAT chat's thoughts; on restart both restore from
  the snapshot's optional `last_usage` / `last_model` keys (no backfill —
  older snapshots stay omitted.
- **Thread activation is the sole selection/activation seam.** Every path
  that selects a thread must go through the same seam (`select_chat`, or
  `activate_thread` for restore), or sticky ctx state, unread markers, and
  scroll position will diverge between switch paths.
- **`agent_event` frames are mid-turn and NON-terminal.** They must never
  resolve a request's lifecycle; only `result` / `error` are terminal.
  Subagent progress folds into PER-CHAT counters (`Chat::subagent_counts`,
  keyed by the frame's numeric request id → `(active, total)`),
  independent of the request lifecycle: background subagents outlive their
  turn's result frame, so an idle chat still renders
  `· subagents working: n` (counter alone, no spinner/label). The line
  renders only for the ACTIVE chat, sums across that chat's requests,
  hides at 0, and switches with the thread (session state, never
  persisted).
- **Capabilities.** The TUI advertises `{"thoughts": true, "subagents": true}`
  at the handshake; the protocol version stays `1`. Unknown fields inside
  known frames are ignored; unknown frame kinds are tolerated and traced
  to the diag log.
- **Thoughts are per-chat, session-only view state.** The dim reasoning
  block lives on the chat that produced it (`Chat::last_thoughts`): a
  VISIBLE result of the owning chat carrying thoughts is the only writer,
  hidden model-picker exchanges and fieldless command frames never touch
  it, a new visible request start clears only that chat's block, and the
  renderer reads the ACTIVE chat's field — a background reply can never
  leak its reasoning into the viewed chat. Toggled with Ctrl+S (a `^S
  on/off` token on the status hints row mirrors the state), never
  persisted, and a 64 KB truncation marker arrives pre-truncated from the
  backend.
- **Storage.** Platform data dir via `dirs::data_dir()`: on Linux
  `$XDG_DATA_HOME/chibi-tui` when set, else `~/.local/share/chibi-tui`;
  `~/Library/Application Support/chibi-tui` (macOS); `%APPDATA%\chibi-tui`
  (Windows); fallback `.chibi-tui` relative to the cwd when `data_dir()`
  is `None` (`history.rs::resolve_root`). Layout: `threads/<thread_uuid>.json`
  plus `last-thread.json` (atomic temp-file + rename, rewritten on every
  activation). Resolution order: `--history-dir` → `CHIBI_TUI_HOME` →
  platform data dir. Corrupt snapshots are skipped, save failures are
  non-fatal (stderr only), files are keyed by thread id — never by title —
  and the history format is strictly additive.
- **Backend log contract.** Backend stderr lines are machine-parseable as
  `YYYY-MM-DD HH:MM:SS | LEVEL | message`; `diag.rs` parses ` | LEVEL | `
  at ingestion to color the log viewer (unrecognized levels render plain).
  Diagnostics never break the app: an unopenable file sink is silently
  disabled.
- **Empty answers.** An empty assistant reply (or the bare `ACK_MARKER`,
  `"<chibi>ACK</chibi>"`) produces no chat bubble; an answer that merely
  contains the marker next to real text is shown raw — marker cleanup is
  the backend's job, not the TUI's.
- **Failure mode.** Backend connect failures or mid-session drops surface
  as the modal error popup (`R` retry, `Esc` dismiss, `q`/Ctrl+C quit) —
  never a panic, never a silent swallow.
- **Terminal capability degradation.** The kitty keyboard protocol is
  pushed at startup ONLY on non-Windows builds
  (`PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)`; see
  `push_kitty_flags()` in `main.rs` — the Win32 console input path cannot
  represent kitty sequences, and pushing there makes kitty-capable
  terminals emit keys the console path mis-decodes, which is how
  Ctrl+↑/↓ thread switching broke). The `TerminalRestore` guard pops the
  flags if and only if they were pushed — the push/pop decisions always
  mirror each other. On unix, legacy encodings (`ESC[1;5A`) and kitty
  CSI-u encodings decode into the same `KeyEvent`, so both flavors work
  everywhere the byte stream is parsed. Chords that degrade on legacy
  terminals (Shift+Enter, Ctrl+M, Ctrl+Shift+F, Shift+Ctrl+L) are
  documented in the README keybinding table — keep it honest when touching
  keybindings.

## Wire frames (protocol v1)

Client → server: `initialize` (client info + capabilities), `request`,
`cancel`, `shutdown`. Server → client: `ready`, `status`, `agent_event`,
`result` (optional `model` / `provider` / `usage` / `thoughts`), `error`
(`request_id: null` = global; the pipeline drops those). Wire `request_id`s
are client-chosen UUID strings (`Submitted`); `BackendEvent` variants carry
numeric stand-ins (`u64`, derived from the UUID hash in `live.rs`); the
wire `thread_id` is a deterministic i64 hash of the chat's stable UUID.
`protocol.rs` types are the frozen contract: extend, don't reshape.

## Tests

- Unit tests live inline (`#[cfg(test)] mod tests` at the bottom of each
  module); integration tests live in `tests/`.
- `tests/protocol_fixtures.rs` runs against `tests/fixtures/ide_protocol/` —
  canonical copies of the backend repo's fixtures. Never edit them here: fix
  upstream and re-copy.
- `tests/request_pipeline.rs` and `tests/wave2_e2e.rs` drive the
  deterministic `tests/fake_backend.py` through the real `LiveBackend` glue
  path; the e2e suite also renders through `ratatui::backend::TestBackend`
  and covers persistence and diagnostics contracts.
- Test names state the behavior they verify, descriptive snake_case:
  `result_retains_latest_turn_usage_and_thoughts`.
- Wrap every `await` in async tests in `tokio::time::timeout` (~10 s) so a
  protocol deadlock fails fast in CI instead of hanging the suite.

## Rust practices observed in this repo

- **Errors:** `thiserror` enums with precise variants and `#[source]` io
  errors (see `BackendError`). Degrade instead of die wherever a lost
  snapshot, sink, or message must not take down the TUI.
- **Concurrency:** tokio throughout. The request pipeline is a single-owner
  actor task that owns the pending map — no locks; statuses and agent
  events fan out over broadcast channels and can never consume a request's
  final outcome. A dead pipe fails all pending requests as
  `BackendError::Broken`; `RequestPipeline::reconnect` respawns +
  re-handshakes the backend.
- **Docs:** every module carries `//!` rationale docs, many items carry
  `///` docs — but the docs gate (`RUSTDOCFLAGS=-D warnings cargo doc`)
  enforces truthful, compilable docs only, not completeness (no
  `missing_docs` lint; internal items not exhaustively documented).
- **Comments:** plain `//` comments are pervasive — non-obvious invariants
  and context in `app.rs` / `ui.rs` / `main.rs`, including
  `// ---- feature ----` section markers; match the surrounding density.
  No provenance anywhere: never mention AI authorship, generation
  workflow, or tooling in code, comments, docs, or commit messages.
- **Style:** rustfmt defaults; clippy clean with `-D warnings`. Release
  profile: `lto = true`, `codegen-units = 1`, `opt-level = 3`, `strip = true`
  (mind when debugging release binaries); `Cargo.lock` is committed and
  updated deliberately (see Commands).

## Commits & releases

- One-line commits, `Type: summary`, imperative mood — `Feat:`, `Fix:`,
  `Docs:`, `Test:`, `Chore:` (see `git log --oneline`).
- `CHANGELOG.md` follows Keep a Changelog under `[Unreleased]`; versioning
  is SemVer. Releases are tag-driven: the git tag must match the
  `Cargo.toml` version exactly (`.github/workflows/release.yml` hard-fails
  on drift); the release workflow runs all cargo commands with `--locked`.
