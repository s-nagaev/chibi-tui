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
| `↑` / `↓` | Switch chat (when input is empty) |
| `Ctrl+N` | New chat |
| `Ctrl+R` | Rename the current thread inline (`Enter` save · `Esc` cancel) |
| `Enter` | Send message (or queue it while this chat is busy) |
| `⇧↵` / `⌥↵` | Insert a newline into the input (multi-line prompts) |
| `Ctrl+C` | Cancel the in-flight request of the current chat; quit when idle |
| `PgUp` / `PgDn` | Scroll chat view up/down one page (by visible rows) |
| macOS: `fn`+`↑` / `fn`+`↓` | Equivalent to PgUp/PgDn on laptops without a dedicated Page key |
| `Esc` | Clear input / dismiss popup |
| `Ctrl+V` | Paste clipboard (macOS: Cmd+V) |

> **Terminal support note:** `⇧↵` / `⌥↵` require a terminal that implements
> the [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
> (kitty, WezTerm, foot, recent Ghostty, …). chibi-tui requests it at startup
> via crossterm's `PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)`
> and pops the flags on exit. On terminals without support the request is
> ignored and **Shift+Enter degrades to plain Enter — i.e. it sends the
> message** instead of inserting a newline. There is no reliable way to
> distinguish the keys there; this is a terminal limitation, not a bug.
| `Ctrl+A` / `Ctrl+E` | Move cursor to start / end of line |
| `Ctrl+U` | Delete from cursor to start of line |
| `Ctrl+L` | Clear input |

**Growing input block:** the editor area expands from one up to twenty rows as
the multiline draft grows (`Shift+Enter`), while the chat pane shrinks to make
room. Past twenty lines the view auto-follows the caret, keeping the line you
are typing on screen; `PgUp`/`PgDn` keep scrolling the chat, and clearing the
input collapses the block back to a single row. The rename editor grows the
same way for multiline title drafts.

### Renaming threads

`Ctrl+R` turns the bottom input line into a single-line editor prefilled with
the current thread title (`✎ Rename thread…`). `Enter` saves the trimmed name
(empty names are rejected and keep the old title), `Esc` cancels and restores
the untouched message draft. Renaming works while a request is running; the
sidebar, header and persisted history update immediately (files are keyed by
thread id, so a rename never orphans a history snapshot).

While renaming, `Shift+Enter` / `Alt+Enter` insert a literal newline into the
draft (multi-line titles render as one space-separated line in the sidebar).
Every Enter press inside the rename editor belongs to the editor — it never
submits the message prompt.

If the backend fails to connect or drops mid-session, a modal error popup
appears instead of crashing; `R` retries, `Esc` dismisses, `q` or `Ctrl+C` quits.

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
