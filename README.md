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

### Model label

When the backend reports which model produced an answer (the protocol `result`
frame carries optional `model`/`provider` fields), the assistant's header line
shows it next to the agent name in a dim parenthetical:

```
● Chibi (glm-5.2)
```

If both fields are present the short `model` display name wins; a
`provider`-only answer labels itself with the provider. The label is attached
per message at answer time, so two consecutive replies from different models
are each labeled with their own.

The parenthetical is **absent** — a plain `● Chibi` — when the answer carries
no model information: an older backend, a `result` frame without the optional
fields, or historical messages. Known limitation: the label is session-scoped
and intentionally not persisted in the history file (its format is unchanged),
so after a restart every restored message shows the plain header again; a
later wave may revisit this.

### Behavior notes

Empty agent acknowledgements are hidden; the spinner is the signal that work
continues. When an agent answers with nothing (or only the protocol-level
`<chibi>ACK</chibi>` marker), no assistant bubble appears at all — the pending
spinner simply resolves and the next queued prompt (if any) sends immediately.
An answer that merely *contains* the marker next to real text is shown as-is,
raw; cleaning up partial markers is the backend's job, not the TUI's.

## Keybindings

| Key | Action |
|---|---|
| `Ctrl+↑` / `Ctrl+↓` | Switch to previous / next thread (resets the chat scroll; clamped at list edges) |
| `Alt+↑` / `Alt+↓` | Same as `Ctrl+↑` / `Ctrl+↓` — a full synonym (see the macOS note below) |
| `Ctrl+T` | **Toggle pane focus** Chat ↔ Sidebar (see the pane-focus section below). While the Sidebar holds focus, `↑`/`↓` move the thread selection with live switching and bare `Enter`/`Esc` return to the editor. Works in ANY terminal: it is a plain Ctrl+letter chord, so it never degrades the way the arrow chords do without the kitty protocol. The previous wrap-cycling thread switcher this key used to bind was removed |
| `↑` / `↓` | Move the text cursor up/down inside the input (navigate the multi-line draft); while the Sidebar holds focus they move the thread selection instead (see pane focus below) |
| `Ctrl+N` | New chat |
| `Ctrl+R` | Rename the current thread inline (`Enter` save · `Esc` cancel) |
| `Ctrl+D` | Delete the current thread (`Enter`/`y` confirm · `Esc`/`n` cancel; refused while the thread is busy) |
| `Ctrl+F` | Find in the current thread (type to filter, `↑`/`↓` navigate matches, `Enter` jump to match, `Esc` close) |
| `Ctrl+Shift+F` | Find in ALL threads (global search; same popup family with thread-title labels and total counts; `Enter` switches to the match's thread and jumps; requires the kitty keyboard protocol) |
| `Ctrl+G` | Open the diagnostics log viewer (backend stderr + TUI lifecycle events; `PgUp`/`PgDn` or `↑`/`↓` scroll, `Esc` close — see the Diagnostics section) |
| `Ctrl+O` | Toggle the status strip — a dim one-row `cwd: <workspace> · <model>` readout on the chat header's top border (hidden by default; see the Status strip section) |
| `Enter` | Send message (or queue it while this chat is busy) |
| `⇧↵` / `⌥↵` | Insert a newline into the input (multi-line prompts) |
| `Ctrl+C` | Cancel the in-flight request of the current chat; quit when idle |
| `PgUp` / `PgDn` | Scroll chat view up/down one page (by visible rows) |
| macOS: `fn`+`↑` / `fn`+`↓` | Equivalent to PgUp/PgDn on laptops without a dedicated Page key |
| `Esc` | Clear input / dismiss popup; while the Sidebar holds focus it just returns focus to Chat (draft untouched) |
| `Ctrl+V` | Paste clipboard (macOS: Cmd+V) |
| `Ctrl+A` / `Ctrl+E` | Move cursor to start / end of line |
| `Ctrl+U` | Delete from cursor to start of line |
| `Ctrl+L` | Clear input and wipe the visible screen (chat view returns to bottom) |

> **Terminal support note:** `⇧↵` / `⌥↵` (newline inserts), `Ctrl+↑` /
> `Ctrl+↓` and `Alt+↑` / `Alt+↓` (thread switching) and `Ctrl+Shift+F`
> (global search) require a terminal that implements the
> [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
> (kitty, WezTerm, foot, recent Ghostty, iTerm2, …). chibi-tui requests it at
> startup via crossterm's `PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)`
> and pops the flags on exit. On terminals without support the request is
> silently ignored:
>
> - **Shift+Enter degrades to plain Enter — i.e. it sends the message**
>   instead of inserting a newline.
> - **`Ctrl+↑` / `Ctrl+↓` become indistinguishable from plain `↑` / `↓`**
>   (legacy terminals send the same escape bytes as plain arrows, or nothing
>   for the combination): those presses then move the text cursor instead of
>   switching threads. On stock macOS this flavor is additionally hijacked by
>   Mission Control *before* the terminal ever sees it — use `Alt+↑` /
>   `Alt+↓` there (see the macOS note below).
> - **`Alt+↑` / `Alt+↓` arrive as an Esc press followed by a plain arrow** on
>   legacy terminals: crossterm splits the `ESC ESC [ A`-style sequence into
>   two events, so Esc clears the input and the arrow then moves the caret —
>   no thread switch. This is a terminal limitation, not a bug: use a
>   kitty-protocol-capable terminal (or iTerm2, which sends proper Alt+arrow
>   sequences out of the box) if you need Alt thread switching there.
> - **`Ctrl+Shift+F` becomes indistinguishable from `Ctrl+F`** (the Shift
>   modifier is lost on such terminals): those presses then open the
>   in-thread search instead of the global one. There is no other keyboard
>   path to global search, so on such terminals it is unavailable; use a
>   kitty-protocol-capable terminal if you need it. There is no reliable way
>   to distinguish these keys there; this is a terminal limitation, not a bug.

**macOS note (Mission Control):** macOS binds `Ctrl+↑` / `Ctrl+↓` to the
*Move between spaces* shortcuts system-wide, so on stock macOS those chords
are swallowed by Mission Control before the terminal ever receives them. This
is why chibi-tui also binds `Alt+↑` / `Alt+↓` as a **full synonym** — identical
thread-switching semantics, no behavior divergence. To re-enable the Ctrl
variant instead, turn the system shortcuts off: **System Settings → Keyboard →
Keyboard Shortcuts → Mission Control** → uncheck *Move left a space* / *Move
right a space* (the `Ctrl+↑`/`Ctrl+↓` entries). Per-terminal Alt behavior:
**kitty** is native (its keyboard protocol is requested at startup), **iTerm2**
sends proper Option+arrow sequences out of the box, and **Terminal.app**
needs **Use Option as Meta Key** (*Settings → Profiles → Keys*) so Option
reaches the app at all — but on terminals without kitty-protocol support the
chord may still be split into Esc + arrow (see the terminal support note
above); verify on your terminal.

**Pane focus (`Ctrl+T`):** `Ctrl+T` toggles the keyboard between the two
panes — **Chat** (the prompt editor, default) and **Sidebar** (the thread
list). It deliberately remains a plain Ctrl+letter chord, so it arrives
intact in **every** terminal; modals (rename, delete confirm, both search
popups) sit above focus and closing any of them always lands back on Chat.

While the **Sidebar** holds focus:

- `↑` / `↓` move the thread selection with **live switching** — exactly the
  clamped mechanics of `Ctrl+↑`/`Ctrl+↓` (the active chat and its view
  follow instantly; every switch resets scrolling to follow-bottom).
- bare `Enter` applies and returns focus to Chat (the highlighted chat
  stays active; it never submits the draft), `Esc` returns WITHOUT touching
  what you have typed.
- plain typing never leaks into the editor: printable keys, Backspace,
  Shift/Alt+Enter newlines and readline edits are all swallowed.
- `PgUp`/`PgDn` still scroll the CHAT pane — reading works regardless of
  which pane holds focus.
- global service chords stay live: `^N` (new chat — then focus lands on
  Chat), `^R` rename, `^D` delete (still refused while busy), `^F` /
  `Ctrl+Shift+F` search popups, `^G` log viewer, `^O` status strip, `^L`
  clear screen, `^C` cancel/quit.

The focused sidebar signals itself through the theme only: its divider and
`Chats` title lift to a brighter accent and the idle dot column brightens;
the chat pane's `❯` marker dims while typing is parked.

The old wrap-around *cycling* semantics this key used to carry ("jump to
the NEXT thread, last wraps to first") was removed in favor of the toggle:
use `Ctrl+↑` / `Ctrl+↓` (or their Alt synonyms) for sequential thread
switching.

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

### Deleting threads

`Ctrl+D` opens a centered confirmation popup for the current thread
(`Delete thread`). `Enter` or `y` confirms, `Esc` or `n` cancels, `Ctrl+C`
quits without deleting. While the popup is open, all other keys are ignored —
nothing leaks into the message draft and no global binding fires.

Only **idle** threads can be deleted: if the current thread has a request in
flight or prompts queued, `Ctrl+D` refuses with a brief status message
(`can't delete — busy`) and no popup appears. Deleting an idle thread while
another thread runs in the background is fine — the background request is
untouched. On confirm the thread is removed from the list and its persisted
history file is deleted (a missing file is treated as success). The **next**
thread is selected, or the **previous** one when the last was deleted, or the
clean empty state when no threads remain; the chat view returns to
follow-bottom. Terminal events arriving later for a removed thread are dropped
silently.

### Finding text in a thread

`Ctrl+F` opens a centered search popup scoped to the current thread. Type to
filter: matches are recomputed live, case-insensitively, over the rendered
message text (markdown markup like `**` or backticks is never matched). Each
match is listed as `role · snippet` with a short context window around the
hit, and the total count is shown in the popup title.

`↑` / `↓` move the selection, `Enter` jumps the chat view to the selected
match (the wrapped row containing the hit is placed near the top of the
pane — accurate even for long, visually-wrapped paragraphs) and closes the
popup, `Esc` closes without moving the view, `Ctrl+C` quits. While the popup
is open, all other keys are ignored — the message draft is never touched and
no global binding fires. Searching is strictly read-only and works even while
the thread is busy with a request; `PgUp`/`PgDn` scrolling is suspended while
the popup is open (the jump drives the view instead).

### Finding text everywhere

`Ctrl+Shift+F` opens the same popup family scoped to **all** threads: matches
from every chat are combined — ordered by chat, then by message within each
chat — and each entry is labeled with its **thread title** before the role
label and snippet. The popup title shows the total match count and the number
of threads searched.

`↑` / `↓` move the selection, `Enter` **switches to the match's thread** (the
same selection mechanics as `Ctrl+↑` / `Ctrl+↓` — and `Alt+↑` / `Alt+↓`) and
jumps the chat view to the selected match — the wrapped row containing the
hit is placed near the top of the pane, accurate even for long,
visually-wrapped paragraphs — then closes the popup. `Esc` closes without
switching or jumping, `Ctrl+C` quits.
Like the in-thread search, it is strictly read-only, works while threads are
busy, and all other keys are ignored while the popup is open.

`Ctrl+Shift+F` needs the kitty keyboard protocol (see the terminal support
note above): on terminals without it the Shift modifier is lost and the chord
degrades to plain `Ctrl+F` (in-thread search).

If the backend fails to connect or drops mid-session, a modal error popup
appears instead of crashing; `R` retries, `Esc` dismisses, `q` or `Ctrl+C` quits.

### Diagnostics log

chibi-tui keeps the last **512 lines** of diagnostics in memory and shows them
in a modal viewer opened with **`Ctrl+G`** (also works while the Sidebar pane
holds focus):

- **What lands in the log** — everything the backend prints to its **stderr**
  (lines appended verbatim; timestamps come from the backend itself), plus
  TUI-side lifecycle events stamped with a `[tui]` prefix in the same stream:
  backend spawn, handshake ok/fail, reconnect, pipe closed. One unified
  diagnostic stream.
- **Reading it** — the viewer opens live-tailing at the bottom; new lines
  stream in while you sit there. `PgUp`/`PgDn` (or `↑`/`↓`) scroll; scrolling
  up freezes the view and a `+K new lines` footer counts what arrived while
  you were detached — page back down to re-arm the tail. `Esc` closes. When
  unseen lines arrived while the viewer was closed, a dim `log*` token shows
  on the status line.
- **File sink (opt-in)** — set `CHIBI_TUI_LOG=/path/to/file.log` before
  starting and every line that lands in the ring buffer is mirrored to that
  file (append mode; parent directories are created). Unset (the default)
  means memory only. A file that cannot be opened/created is silently
  ignored — diagnostics never break the app.
- **Honest limitation** — only the backend's **stderr** reaches the buffer.
  The backend's loguru logging currently writes to its **stdout** (the
  protocol channel), so backend log lines do not appear here; the
  backend-side log sink fix is a separate backend task.

### Status strip

A hideable one-row readout on the **chat header's top border**, toggled with
**`Ctrl+O`** (also works while the Sidebar pane holds focus). **Hidden by
default**; the toggle state is plain view state — it survives every modal
open/close and never captures keys.

- **Content** — `cwd: <workspace> · <model>`:
  - `cwd` is the **basename of the workspace root** (the `--workspace` value
    the TUI passes to the backend; CLI-only today, read reactively so a
    future runtime change would show up on the next frame).
  - `model` is the **last known model of the active chat**, reusing the same
    per-message metadata as the `● Chibi (model)` answer headers: it updates
    on every result resolution, an error resolution keeps the last known
    label, and switching chats re-labels from that chat's own history. Both
    segments render a `—` placeholder when unknown. Model labels are
    session-scoped — restored history shows `—` again.
- **Placement** — the strip rides the SAME top-border row as the chat title
  (`#1/4 · JSONL protocol`), right-aligned: it costs **zero vertical space**.
  On narrow terminals it is truncated with an ellipsis to the space left of
  the title, and in a too-narrow chat pane it simply does not render.
- **Styling** — dim (theme-driven), deliberately quiet: context, not content.
- **Extensible** — the planned context-size segment (once the protocol
  reports real usage) will extend the same readout; a local approximation
  was rejected as dishonest.

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
