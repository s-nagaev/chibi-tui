# chibi-tui

A terminal UI client for [Chibi](https://github.com/s-nagaev/chibi) — an AI
assistant — built with Rust + ratatui + crossterm + tokio. Chats with the
assistant over IDE protocol v1 (JSONL over stdio), renders markdown answers
with syntax-highlighted code blocks, and persists chat history locally.

| Splash | Chat |
|---|---|
| ![splash](docs/00_splash.png) | ![start](docs/01_start.png) |

More screenshots: [spinner](docs/02_spinner.png) ·
[answer](docs/03_answer.png) · [other chat](docs/04_other_chat.png).

## Requirements

- A terminal with 24-bit color support.
- The `chibi` binary in `PATH` (the TUI spawns it as `chibi ide --stdio`).
- For building: a Rust toolchain (edition 2021).

## Installation

Once published to crates.io:

```bash
cargo install chibi-tui
```

Or build from source:

```bash
git clone <repo-url> chibi-tui && cd chibi-tui
cargo build --release
# binary at target/release/chibi-tui
```

## Usage

```bash
chibi-tui --workspace /path/to/project   # live mode: talks to `chibi ide --stdio`
chibi-tui --mock                         # demo mode: static mocks, no backend needed
```

Options:

| Flag | Meaning |
|---|---|
| `--workspace <dir>` | Workspace root passed to the backend (default: current dir). |
| `--mock` | Run fully on mocks — no backend process, no I/O; development/screenshot mode. |
| `--history-dir <dir>` | Override the chat-history directory. |

The app starts in fullscreen alternate-screen mode; the terminal is restored on exit.

## Keybindings

| Key | Action |
|---|---|
| `↑` / `↓` or `k` / `j` | Switch chat (when input is empty) |
| `N` | New chat (when input is empty) |
| `Enter` | Send message |
| `PgUp` / `PgDn` | Scrollback in chat view |
| `Ctrl+C` | Cancel in-flight request; quit when idle |
| `Esc` / `q` | Quit (when idle) |
| `Ctrl+V` (`Cmd+V` on macOS) | Paste from clipboard into input |
| `Ctrl+A` / `Ctrl+E` | Move cursor to line start / end |
| `Ctrl+U` / `Ctrl+L` | Clear text before cursor / clear line |
| `R` | Reconnect (shown in error popup when backend disconnects) |

If the backend fails to connect or drops mid-session, a modal error popup
appears instead of crashing; `R` retries, `Esc`/`q` quits.

## Configuration & Data

Chat history is stored locally under the platform data directory:

```
~/.local/share/chibi-tui/threads/     # Linux (XDG data dir)
~/Library/Application Support/chibi-tui/threads/  # macOS
%APPDATA%\chibi-tui\threads\          # Windows
```

Each chat is saved as `<chat-id>.json` and restored on startup. Corrupt files
are skipped rather than blocking startup. Override with `CHIBI_TUI_HOME` or
`--history-dir`.

No other configuration files, no network access beyond what the spawned
backend performs.

## Architecture

```
src/
├── main.rs             tokio event loop: keyboard / backend events / spinner tick
├── lib.rs              library root; all testable logic lives here
├── protocol.rs         serde types of IDE protocol v1
├── backend_client.rs   child-process lifecycle: spawn, handshake, JSONL framing
├── request_pipeline.rs request correlation, cancellation, status updates
├── live.rs             LiveBackend: real backend source
├── backend.rs          Backend trait + MockBackend (latency + canned replies)
├── history.rs          local persistence (<data>/chibi-tui/threads/)
├── app.rs              state: chats, selection, input buffer, connection
├── markdown.rs         pulldown-cmark → styled ratatui lines (+ syntect)
├── mock.rs             static mock conversations and reply set
├── model.rs            Message / Role / ChatStatus types
├── popup.rs            modal popups (errors)
├── splash.rs           ASCII kitten splash screen (~1.5s, skippable)
├── theme.rs            Tokyo Night palette
└── ui.rs               layout: sidebar + chat view + input + status line
```

Both backend sources surface progress as `BackendEvent` streams, so the UI is
agnostic to which one is wired in — replacing mocks with another transport
requires only a new `Backend` implementation.

## Development

```bash
cargo fmt --check                              # formatting
cargo clippy --all-targets -- -D warnings      # lints
cargo test                                     # unit + fixture + integration tests
cargo doc --no-deps                            # API docs
```

CI runs fmt, clippy (-D warnings), tests and a release build on stable and beta.

## License

MIT — see [LICENSE](LICENSE).
