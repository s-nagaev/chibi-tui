<h1 align="center"><img width=150 src="https://github.com/s-nagaev/chibi/raw/main/docs/logo.png" alt="Chibi Logo"></h1>

# Chibi TUI

<p align="center">
  <a href="https://github.com/s-nagaev/chibi-tui/actions/workflows/ci.yml"><img src="https://github.com/s-nagaev/chibi-tui/actions/workflows/ci.yml/badge.svg" alt="Build"></a>
  <a href="https://www.codefactor.io/repository/github/s-nagaev/chibi"><img src="https://www.codefactor.io/repository/github/s-nagaev/chibi/badge" alt="CodeFactor"></a>
  <a href="https://hub.docker.com/r/pysergio/chibi/tags"><img src="https://img.shields.io/badge/arch-arm64%20%7C%20amd64-informational" alt="Architectures"></a>
  <a href="https://github.com/s-nagaev/chibi/blob/main/LICENSE"><img src="https://img.shields.io/github/license/s-nagaev/chibi" alt="License"></a>
  <a href="https://chibi.bot"><img src="https://img.shields.io/badge/docs-chibi.bot-blue" alt="Documentation"></a>
</p>

A terminal UI client for [Chibi](https://github.com/s-nagaev/chibi) — an AI
assistant — built with Rust + ratatui + crossterm + tokio. Chats with the
assistant over IDE protocol v1 (JSONL over stdio), renders markdown answers
with syntax-highlighted code blocks, and persists chat history locally.

| Splash | Chat |
|---|---|
| ![splash](docs/00_splash.png) | ![start](docs/01_start.png) |

## Requirements

- A terminal with 24-bit color support.
- The `chibi` binary in `PATH` (the TUI spawns it as `chibi stdio --tui`).
- For building: a Rust toolchain (edition 2021).

## Installation

### Prebuilt binaries

Grab an archive from the
[GitHub Releases](https://github.com/s-nagaev/chibi-tui/releases) page — every
release ships a binary per platform plus a `SHA256SUMS.txt` with the
checksums of all archives:

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `chibi-tui-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `chibi-tui-<version>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `chibi-tui-<version>-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `chibi-tui-<version>-x86_64-pc-windows-msvc.zip` |

Download the archive matching your platform and `SHA256SUMS.txt`, verify the
checksum (`sha256sum -c` on Linux / `shasum -a 256 -c` on macOS / `Get-FileHash`
in PowerShell), extract the archive (`tar -xzf` / `unzip`) and put the
`chibi-tui` binary somewhere on your `PATH`.

### From crates.io

```bash
cargo install chibi-tui
```

(available on crates.io after the first release — until then, use a prebuilt
binary or build from source).

### Backend

The TUI is a client: it spawns the `chibi` binary from the
[`chibi-bot`](https://pypi.org/project/chibi-bot/) Python package and talks
IDE protocol v1 over stdio:

```bash
pip install chibi-bot
```

The default transport spawns `chibi stdio --tui`; the workspace root travels
inside each request frame (your `--workspace` value, the current directory by
default), never on the backend command line. To point the TUI at a custom
backend executable instead of the `chibi` found on `PATH`, set
`CHIBI_BACKEND_BIN=/path/to/backend`.

### Build from source

```bash
git clone https://github.com/s-nagaev/chibi-tui chibi-tui && cd chibi-tui
cargo build --release
# binary at target/release/chibi-tui
```

## Usage

```bash
chibi-tui
# OR
chibi-tui --workspace /path/to/project
```

Options:

| Flag | Meaning |
| --- | --- |
| `--workspace <dir>` | Workspace root passed to the backend (default: current dir). |
| `--mock` | Run fully on mocks — no backend process, no I/O; development/screenshot mode. |
| `--history-dir <dir>` | Override the chat-history directory. |

The app starts in fullscreen alternate-screen mode; the terminal is restored on exit.

## Keybindings

| Key | Action |
| --- | --- |
| `Ctrl+↑` / `Ctrl+↓` | Switch to the previous / next thread (the chat scroll resets on every switch; on stock macOS use `Alt+↑` / `Alt+↓` instead — see the macOS note below) |
| `Ctrl+T` | Toggle keyboard focus between the Chat pane (the prompt editor, default) and the Sidebar (the thread list) — while the Sidebar holds focus, `↑`/`↓` move the thread selection with live switching, `Enter`/`Esc` return to the editor with the draft untouched, and `PgUp`/`PgDn` keep scrolling the chat |
| `↑` / `↓` | Move the text cursor up/down inside the multi-line input (they move the thread selection instead while the Sidebar holds focus) |
| `Ctrl+N` | Start a new chat |
| `Ctrl+P` | Clone the current thread with its full conversation context as a `<name> (copy)` thread (refused while the thread is busy; on older backends a popup explains instead) |
| `Ctrl+R` | Rename the current thread inline (`Enter` saves, `Esc` cancels) |
| `Ctrl+D` | Delete the current thread after confirmation (`Enter`/`y` confirm, `Esc`/`n` cancel; refused while the thread is busy) |
| `Ctrl+F` | Find text in the current thread: type to filter, `↑`/`↓` navigate matches, `Enter` jump to a match, `Esc` close |
| `Ctrl+Shift+F` | Find text across all threads; `Enter` switches to the match's thread and jumps to the match (requires the kitty keyboard protocol) |
| `Ctrl+G` | Open the diagnostics log viewer (`PgUp`/`PgDn` or `↑`/`↓` scroll, `Esc` close) |
| `Ctrl+O` | Toggle the status strip — a dim `cwd: <workspace> · <model>` readout on the chat header's top border, extended with a `· ctx …` context-usage segment once the backend reports one (hidden by default) |
| `Ctrl+S` | Toggle the dim reasoning (thoughts) block above the latest answer (on by default; a session-only view state — nothing is ever removed from saved history) |
| `Ctrl+M` | Open the model picker popup (`↑`/`↓` navigate, `Enter` switch, `Esc` close; requires the kitty keyboard protocol) |
| `Enter` | Send the message (or queue it while this chat is busy) |
| `Shift+Enter` / `Alt+Enter` | Insert a newline into the input (multi-line prompts) |
| `Ctrl+C` | Cancel the in-flight request of the current chat; quit when idle |
| `PgUp` / `PgDn` | Scroll the chat view up/down one page (on macOS laptops without Page keys, `fn`+`↑` / `fn`+`↓` are equivalent) |
| Mouse wheel ↑ / ↓ | Scroll the chat view three lines per notch while the cursor is over it (scrolling back down to the end re-pins follow-bottom); with the Sidebar focused, the wheel over it moves the thread selection (no switching on hover without focus). In the log viewer / help modal / model picker, the wheel works wherever the cursor is |
| Mouse drag-select | Press and drag inside the chat view to highlight text; on release the selected plain text is copied to the clipboard (OSC 52 + `CHIBI_TUI_COPY_CMD` fallback; a failed write degrades silently). Newlines appear at real line breaks, never at wrap points. `Esc`, a plain click (press + release without dragging) or switching threads clears the selection; the selection is session-only and never saved to history |
| `Esc` | Clear the input or dismiss a popup (also clears a mouse text selection); while the Sidebar holds focus, it just returns focus to Chat without touching the draft |
| `Ctrl+V` | Paste from the clipboard (macOS: Cmd+V) |
| `Ctrl+A` / `Ctrl+E` | Move the cursor to the start / end of the line |
| `Ctrl+U` | Delete from the cursor to the start of the line |
| `Ctrl+L` | Stop the running request of the current chat behind a confirmation popup (`Enter`/`y` confirm, `Esc`/`n` cancel; a no-op when idle; on backends without `/stop` support a toast explains instead) |
| `Shift+Ctrl+L` | Reset the current thread behind a confirmation popup (same confirm/cancel keys): the thread's history is dropped and the local dialog cleared, both while running and when idle (on legacy terminals this degrades to `Ctrl+L` — type `/reset` at the prompt instead) |
| `F1` | Toggle the keybindings help modal — a centered, scrollable popup listing every active chord |

> **Keyboard layouts:** all `Ctrl`-chords work under the Russian (ЙЦУКЕН)
> and Ukrainian keyboard layouts — the app normalizes the layout character
> crossterm reports (`Ctrl+Ф` → `Ctrl+A`, `Ctrl+С` → `Ctrl+C`,
> `Ctrl+І` → `Ctrl+S`, …), including the shifted uppercase shapes
> (`Shift+Ctrl+Д` is the `Shift+Ctrl+L` reset). Plain-letter hotkeys
> (`R` reconnect, `y`/`n` confirmations, `k`/`j`/`g`/`w` in the log viewer)
> still require a Latin layout; typing text into the draft, the rename
> editor and the search popups works with any layout.

> **Terminal support:** `Shift+Enter` / `Alt+Enter`, `Ctrl+Shift+F`,
> `Shift+Ctrl+L` and `Ctrl+M` require a terminal that implements the
> [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
> (kitty, WezTerm, foot, recent Ghostty, iTerm2, …); chibi-tui requests the
> protocol at startup (on non-Windows builds — see the Windows note below),
> and on terminals without support the request is silently ignored:
> `Shift+Enter` / `Alt+Enter` degrade to plain `Enter` (sending the
> message), `Ctrl+Shift+F` degrades to `Ctrl+F`, `Shift+Ctrl+L` to
> `Ctrl+L`, and `Ctrl+M` arrives as bare `Enter`.
> `Ctrl+↑` / `Ctrl+↓` and their `Alt+↑` / `Alt+↓` synonyms do **not** need
> the protocol: terminals emit modifier-aware legacy sequences for them.
> They fail only where something outside the app intercepts the chord —
> macOS Mission Control binds `Ctrl+↑` / `Ctrl+↓` system-wide (use the
> `Alt` variants there; see the macOS note below), and multiplexers or SSH
> remotes may swallow the modifier, in which case the chord degrades to a
> plain arrow (caret move).
>
> **Windows:** the kitty keyboard protocol is never requested on Windows
> builds. Crossterm reads Windows console input through the Win32 console
> API, which cannot represent kitty sequences, so requesting the protocol
> would only make kitty-capable terminals (recent Windows Terminal among
> them) encode keys the console path cannot decode — Ctrl+↑ / Ctrl+↓ thread
> switching broke exactly this way before the gate. On Windows builds the
> protocol-dependent chords above (`Shift+Enter`, `Ctrl+Shift+F`,
> `Shift+Ctrl+L`, `Ctrl+M`) therefore degrade even in kitty-capable
> terminals, while `Ctrl+↑` / `Ctrl+↓` work through the console API's own
> modifier reporting.

> **macOS:** the system binds `Ctrl+↑` / `Ctrl+↓` to Mission Control's
> *Move between spaces*, so prefer the `Alt+↑` / `Alt+↓` variant there (in
> Terminal.app, enable **Use Option as Meta Key** under *Settings → Profiles
> → Keys* so Option reaches the app), or uncheck *Move left / right a space*
> under **System Settings → Keyboard → Keyboard Shortcuts → Mission Control**
> to keep using the Ctrl chords.

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

Diagnostics file sink: set `CHIBI_TUI_LOG` to a path to mirror the in-memory
diagnostic stream (backend stderr + `[tui]` lifecycle events, see the
Diagnostics section) to a file. Unset by default.

No other configuration files, no network access beyond what the spawned
backend performs.

## License

MIT — see [LICENSE](LICENSE).
