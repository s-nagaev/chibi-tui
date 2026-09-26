//! Chibi TUI — terminal client for Chibi.
//!
//! Backend selection:
//! * default — [`chibi_tui::LiveBackend`]: spawns the real JSONL peer
//!   (`chibi stdio --tui`-compatible), handshakes, streams real answers;
//!   chats persist under the platform data dir (`<data>/chibi-tui/threads/`)
//!   and are restored on startup.
//! * `--mock` — [`chibi_tui::backend::MockBackend`] with pre-filled demo
//!   chats, no processes, no I/O: development / screenshot mode.
//!
//! UX layer:
//! * `Ctrl+C` cancels the in-flight request when one exists; when idle it
//!   opens the quit confirmation ("Quit chibi-tui?", `y`/Enter quits,
//!   `n`/Esc stays). `Esc` clears non-empty input and dismisses the error
//!   popup.
//! * backend failures surface as a modal popup (`R` reconnect, `Esc`
//!   dismisses, `q`/`Ctrl+C` quit) instead of crashing or being silently dropped;
//! * connection state is shown in the status bar
//!   (`● connected / connecting… / disconnected (press R)`);
//! * readline-style input keys (`Ctrl+A/E/U/L`, word ops via the in-house
//!   readline editor `input.rs`), plus clipboard paste (`Ctrl+V`, macOS
//!   Cmd+V).
//!
//! All logic lives in the library crate (`lib.rs`); this binary only wires
//! the terminal.

use std::io;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream, KeyCode, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use futures_util::StreamExt;
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use chibi_tui::app::{Connection, Focus, Mode, ReconnectRequest};
use chibi_tui::backend::{Backend, BackendEvent};
use chibi_tui::{history, mock, setup_screen, splash, theme, ui};

mod keymap;
use keymap::handle_key;

/// Command-line interface.
#[derive(Debug, Parser)]
#[command(name = "chibi-tui", about = "Chibi terminal client")]
struct Cli {
    /// Workspace root passed to the backend (`request.workspace_root`).
    #[arg(long, default_value_os_t = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))]
    workspace: std::path::PathBuf,

    /// Run fully on mocks (no backend process) for development/demo.
    #[arg(long)]
    mock: bool,

    /// Override the chat-history directory (default: platform data dir).
    #[arg(long)]
    history_dir: Option<std::path::PathBuf>,
}

/// Which backend source is wired into the event loop.
///
/// `LivePlaceholder` marks a live session whose backend is currently down
/// (failed initial connect or unrecovered disconnect): submits are rejected,
/// and `R` in the error popup retries the real connection.
enum Source {
    Live(chibi_tui::LiveBackend),
    Mock(Box<chibi_tui::backend::MockBackend>),
    LivePlaceholder,
}

impl Source {
    /// True when submissions can be routed somewhere real.
    fn accepts_submissions(&self) -> bool {
        !matches!(self, Source::LivePlaceholder)
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    // --- CLI ---
    let cli = Cli::parse();

    // --- terminal setup ---
    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        EnableMouseCapture
    );
    // Bracketed paste: ask the terminal to wrap clipboard pastes in
    // `\e[200~ ... \e[201~` so a multi-line paste arrives as ONE crossterm
    // `Event::Paste` instead of a raw keystroke flood. Without the brackets a
    // three-line paste is decoded as ordinary typing — every embedded newline
    // becomes a bare Enter key event and fires a submit (the bug this guards
    // against). Best-effort, same policy as the kitty flags: an unsupported
    // terminal ignores the escape sequence and `handle_paste` degrades to the
    // Ctrl+V clipboard path. Platform-gated like the kitty push: on native
    // Windows crossterm reads input through the Win32 console API, which
    // never synthesizes `Event::Paste`, so pushing the sequence there is a
    // no-op at best.
    let enable_bracketed_paste = !cfg!(windows);
    if enable_bracketed_paste {
        let _ = crossterm::execute!(stdout, EnableBracketedPaste);
    }
    // On builds where crossterm decodes a raw ANSI byte stream (unix,
    // including WSL inside Windows Terminal), ask for the kitty keyboard
    // protocol so capable terminals deliver distinct Shift+Enter /
    // Alt+Enter modifiers instead of bare-Enter bytes. Best-effort: an
    // unsupported terminal ignores the escape sequence and the app degrades
    // to submit-on-Enter (documented in the README). The result is
    // deliberately discarded: setup must never abort the app over an
    // optional enhancement.
    //
    // The push is platform-gated, NOT unconditional: on native Windows
    // builds crossterm reads input through the Win32 console API, which
    // cannot represent kitty sequences at all (crossterm reports enhancement
    // support as always-off there). Pushing the flags makes terminals with
    // kitty support — notably recent Windows Terminal — encode modified keys
    // as CSI-u sequences that the console path cannot decode, silently
    // killing chords like Ctrl+Up/Down thread switching. Legacy terminals
    // already report Ctrl+arrows via unambiguous modifier-aware legacy
    // sequences, so skipping the push loses nothing on Windows. The teardown
    // guard below restores whatever actually got enabled: it pops the flags
    // if and only if they were pushed here.
    let push_kitty_flags = push_kitty_flags();
    if push_kitty_flags {
        let _ = crossterm::execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    // Every exit path below (splash abort, `?` failures, normal end, panic
    // unwind) restores the terminal exactly once through this guard.
    let _terminal_restore = TerminalRestore::new(push_kitty_flags, enable_bracketed_paste);
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let theme = theme::Theme::tokyo_night();

    // --- splash screen (~1.5s, skippable) ---
    let mut reader = EventStream::new();
    if !splash::run(&mut terminal, &mut reader, &theme).await? {
        return Ok(()); // aborted from the splash
    }
    drop(reader);

    // --- backend connect (before drawing any chat UI) ---
    //
    // Mock mode keeps its canned demo chats. Live mode restores persisted
    // history (or starts a single fresh chat on first run). A failed initial
    // connect no longer aborts the app: it starts disconnected with an error
    // popup and can be recovered with `R`. A missing backend binary on a
    // clean machine instead gets the dedicated setup screen, with install
    // commands and a retry key.
    let restore_dir = cli.history_dir.clone();
    let mut app;
    let mut source = if cli.mock {
        app = chibi_tui::app::App::new(mock::initial_chats());
        app.connection = Connection::Connected; // mocks are always "up"
        Source::Mock(Box::new(chibi_tui::backend::MockBackend::new()))
    } else {
        app = bootstrap_app(restore_dir.as_deref());
        // Missing-backend setup screen loop: it runs before the main event
        // loop exists, so a retry (`r` on the screen) is just another
        // connect attempt. A quit from the screen leaves the app cleanly.
        loop {
            match chibi_tui::LiveBackend::connect(&cli.workspace).await {
                Ok(live) => {
                    app.connection = Connection::Connected;
                    // feature gates read the handshake's
                    // advertised commands (detection, never assumption).
                    app.set_backend_commands(live.backend_commands().to_vec());
                    break Source::Live(live);
                }
                Err(e) => {
                    // Spawn failure with no explicit binary override means
                    // the default `chibi` is simply absent: show the
                    // dedicated setup screen instead of the generic popup.
                    // Everything else (a broken custom binary, handshake
                    // problems) keeps the old path below.
                    let override_set = std::env::var_os("CHIBI_BACKEND_BIN");
                    if setup_screen::applies(&e, override_set.as_deref()) {
                        let mut reader = EventStream::new();
                        let flow = setup_screen::run(&mut terminal, &mut reader, &theme).await?;
                        drop(reader);
                        if flow == setup_screen::Flow::Quit {
                            return Ok(()); // user quit from the setup screen
                        }
                        continue; // r: re-attempt the spawn
                    }
                    app.connection = Connection::Disconnected;
                    app.show_error(format!("{e}\n(hint: use --mock for the offline demo mode)"));
                    break Source::LivePlaceholder;
                }
            }
        }
    };

    // wire the strip's cwd source for BOTH backends (the
    // CLI flag defaults to the process cwd, so mock mode has it too). Read
    // reactively by the renderer each frame; the workspace is CLI-only
    // today, so the value never changes at runtime.
    app.workspace_root = Some(cli.workspace.to_string_lossy().into_owned());

    // Single channel: backend tasks push progress events, UI consumes.
    let (event_tx, mut event_rx) = mpsc::channel::<BackendEvent>(64);

    // Out-of-band continuation answers (background tool results) are
    // session-scoped: pump them into the shared channel for the initial
    // connection (a reconnect spawns its own pump in `connect_live`).
    if let Source::Live(live) = &source {
        live.pump_background_messages(event_tx.clone());
        live.pump_cwd_updates(event_tx.clone());
    }

    let res = run_loop(
        &mut terminal,
        app,
        &mut source,
        &cli.workspace,
        event_tx,
        &mut event_rx,
        &theme,
        cli.history_dir.as_deref(),
    )
    .await;

    // --- graceful shutdown of a live backend process ---
    if let Source::Live(live) = &source {
        let _ = live.shutdown().await;
    }

    res
}

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Whether the kitty keyboard protocol is pushed at startup.
///
/// The protocol is only useful — and only safe — where crossterm decodes a
/// raw ANSI byte stream (unix, including WSL inside Windows Terminal): there
/// the legacy encodings (`ESC[1;5A` for Ctrl+arrows) and the kitty CSI-u
/// encodings both decode into the very same key events, so pushing the flags
/// is free and upgrades capable terminals to unambiguous Shift/Ctrl chords.
/// On native Windows builds crossterm instead reads the Win32 console API,
/// which cannot represent kitty sequences (crossterm itself reports
/// enhancement support as always-off there); pushing the flags makes
/// terminals with kitty support — notably recent Windows Terminal — emit
/// CSI-u sequences the console path cannot decode, which is how Ctrl+↑/↓
/// thread switching broke. So the flags are never pushed on Windows and the
/// app runs on the console API's own modifier reporting.
fn push_kitty_flags() -> bool {
    !cfg!(windows)
}

/// RAII teardown: restores the terminal when dropped. Instantiated right
/// after terminal setup so EVERY exit path — splash abort (`return Ok(())`),
/// `?` failures, normal end, and panic unwind — leaves raw mode disabled,
/// the alternate screen left, mouse capture off,
/// kitty keyboard-enhancement flags popped exactly when they were pushed,
/// and bracketed paste mode disabled exactly when it was enabled.
struct TerminalRestore {
    /// Mirrors the startup push decision: the pop is issued if and only if
    /// the flags were pushed, so the terminal's kitty flag stack can never
    /// leak into the user's shell (nor under-pop someone else's entry).
    pop_kitty_flags: bool,
    /// Mirrors the startup enable decision: bracketed paste is a terminal
    /// MODE (not a stack), so it is turned off if and only if it was turned
    /// on here — never blind, to avoid un-bracketing someone else's paste
    /// mode on exit.
    disable_bracketed_paste: bool,
}

impl TerminalRestore {
    fn new(pop_kitty_flags: bool, disable_bracketed_paste: bool) -> Self {
        Self {
            pop_kitty_flags,
            disable_bracketed_paste,
        }
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        if self.pop_kitty_flags {
            let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        if self.disable_bracketed_paste {
            let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        }
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Show);
    }
}

/// Attempt to (re)connect the live backend. On success the source becomes a
/// real [`Source::Live`] handle and the popup clears; on failure the app
/// stays disconnected and the error lands in the popup.
///
/// `R` from the error popup routes here. The old pipeline (if any) is shut
/// down best-effort before a fresh one is spawned.
async fn connect_live(
    source: &mut Source,
    app: &mut chibi_tui::app::App,
    workspace: &std::path::Path,
    event_tx: mpsc::Sender<BackendEvent>,
) {
    if let Source::Live(old) = source {
        let _ = old.shutdown().await;
    }
    *source = Source::LivePlaceholder;
    app.connection = Connection::Connecting;
    match chibi_tui::LiveBackend::connect(workspace).await {
        Ok(live) => {
            app.connection = Connection::Connected;
            // refresh the advertised commands after a
            // reconnect too, the new session re-handshakes.
            app.set_backend_commands(live.backend_commands().to_vec());
            app.dismiss_error();
            // Session-scoped continuation answers ride the new connection's
            // own pump; the old pump retired with its closed channel.
            live.pump_background_messages(event_tx.clone());
            live.pump_cwd_updates(event_tx);
            *source = Source::Live(live);
        }
        Err(e) => {
            app.connection = Connection::Disconnected;
            app.show_error(e.to_string());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    terminal: &mut Tui,
    mut app: chibi_tui::app::App,
    source: &mut Source,
    workspace: &std::path::Path,
    event_tx: mpsc::Sender<BackendEvent>,
    event_rx: &mut mpsc::Receiver<BackendEvent>,
    theme: &theme::Theme,
    history_dir: Option<&std::path::Path>,
) -> io::Result<()> {
    let mut reader = EventStream::new();
    let mut spinner_tick = tokio::time::interval(Duration::from_millis(100));
    spinner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Write-on-activation seam: the last-thread pointer is persisted whenever
    // the active thread differs from the last observed one (the first
    // observation is the startup selection), so even a hard crash records
    // the thread the user was reading.
    let mut tracked_thread: Option<String> = None;

    loop {
        // Resize events force a full repaint of the next frame anyway; the
        // layout (sidebar width, code panels) adapts purely from the new
        // frame size. There is no cached geometry anywhere in the render path.
        terminal.draw(|f| ui::draw(f, &mut app, theme))?;

        tokio::select! {
            // ---- keyboard / resize ----
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(CtEvent::Key(key))) => {
                        if key.kind == crossterm::event::KeyEventKind::Press {
                            // snapshot BEFORE the key
                            // lands so the loop can tell a rename commit
                            // (Renaming --Enter--> Normal) apart from a plain
                            // message submission. Rename detection uses
                            // `matches!(.. Renaming)`, NOT `!is_normal()`,
                            // because the delete-confirm popup also leaves
                            // Normal mode, and its Enter
                            // is a deletion, never a rename.
                            let name_before = app.chat_title();
                            let was_renaming = matches!(app.mode, Mode::Renaming { .. });
                            let was_confirming_delete =
                                matches!(app.mode, Mode::ConfirmDelete);
                            // snapshot BEFORE the key
                            // lands too. The search popup's Enter jumps and
                            // closes, so by the time the loop runs the mode
                            // is already Normal again; only the snapshot can
                            // tell that Enter apart from a plain submit.
                            let was_searching = matches!(app.mode, Mode::Searching { .. });
                            // same snapshot for the
                            // GLOBAL search popup: its Enter activates the
                            // match's thread AND closes the popup, so the
                            // loop must know that Enter belonged to it.
                            let was_searching_all =
                                matches!(app.mode, Mode::SearchingAll { .. });
                            // snapshot of the SIDEBAR-focus
                            // flag. Bare Enter while the sidebar holds focus
                            // means "apply & return to the editor" (handle_key
                            // flips focus back), so it must never also submit
                            // the message draft the editor still holds.
                            let was_sidebar_focused = app.focus == Focus::Sidebar;
                            // the picker's Enter
                            // confirms the selected model: it belongs to
                            // the popup, never to message submission.
                            let was_model_picking =
                                matches!(app.mode, Mode::ModelPicking { .. });
                            // the help modal has no
                            // Enter action at all (read-only table), so its
                            // Enter must never leak into message submission
                            // either.
                            let was_help_viewing =
                                matches!(app.mode, Mode::HelpViewing { .. });
                            // the Enter that
                            // confirms the stop/reset popup belongs to the
                            // popup, never to the message draft.
                            let was_confirming_stop_reset =
                                matches!(app.mode, Mode::ConfirmStopReset { .. });
                            // the quit-confirm popup's Enter belongs to the
                            // popup too. The popup is a FLAG, not a Mode, and
                            // it can sit ABOVE a non-empty draft: its
                            // confirming Enter (and the second Ctrl+C, which
                            // is not a submit key anyway) must never ALSO
                            // submit the draft it is parked on top of. The
                            // snapshot is taken before the key lands because
                            // `confirm_quit` clears the flag inside
                            // handle_key — the loop would otherwise see a
                            // plain Enter over a full draft and send it right
                            // before shutting down.
                            let was_quit_confirm = app.quit_confirm;

                            handle_key(&mut app, key);

                            // a hidden exchange
                            // staged by the key handlers (`^M` open or
                            // Enter selection) is sent through the SAME
                            // `send_submitted` path as any prompt; it just
                            // carries its own ids and adds no bubbles.
                            if let Some(hidden) = app.take_picker_submission() {
                                send_submitted(source, &hidden, event_tx.clone());
                            }

                            // a staged clone request rides
                            // the same send path as any prompt; its terminal
                            // event resolves the pending clone inside App.
                            if let Some(clone_req) = app.take_clone_submission() {
                                send_submitted(source, &clone_req, event_tx.clone());
                            }

                            // a staged /stop or
                            // /reset control request rides the same
                            // out-of-band send path — deliberately NOT the
                            // busy-chat FIFO, which would only run it after
                            // the very turn it exists to interrupt.
                            if let Some(control_req) = app.take_control_submission() {
                                send_submitted(source, &control_req, event_tx.clone());
                            }

                            // A rename commit happened iff Enter closed an
                            // open session and the active chat's name changed.
                            // Rejected drafts (empty / whitespace-only) leave
                            // the name untouched and skip persistence.
                            let renamed = was_renaming
                                && key.code == KeyCode::Enter
                                && app.mode.is_normal()
                                && app.chat_title() != name_before;
                            // ANY Enter pressed inside rename mode belongs to
                            // the rename editor, never to message submission
                            // (a rejected save must not leak into the prompt).
                            let enter_consumed_by_rename =
                                was_renaming && key.code == KeyCode::Enter;
                            // the Enter that confirmed the
                            // delete popup belongs to the popup too, so it must
                            // never submit the message draft to the neighbour.
                            let enter_consumed_by_delete =
                                was_confirming_delete && key.code == KeyCode::Enter;
                            // same gate for the
                            // stop/reset popup's confirming Enter.
                            let enter_consumed_by_stop_reset =
                                was_confirming_stop_reset && key.code == KeyCode::Enter;
                            // the Enter that jumped to the
                            // selected match belongs to the popup too, so it
                            // must never submit the message draft.
                            let enter_consumed_by_search =
                                was_searching && key.code == KeyCode::Enter;
                            // same gate for the
                            // global search popup's Enter: it activates the
                            // target thread and must never submit the draft
                            // to that (or any) chat.
                            let enter_consumed_by_search_all =
                                was_searching_all && key.code == KeyCode::Enter;
                            // the sidebar's bare Enter is
                            // consumed too: it returns focus to the editor
                            // pane and never submits.
                            let enter_consumed_by_sidebar =
                                was_sidebar_focused && key.code == KeyCode::Enter;
                            // the picker's Enter is
                            // consumed by the popup (model switch staged).
                            let enter_consumed_by_picker =
                                was_model_picking && key.code == KeyCode::Enter;
                            // the help modal's Enter
                            // is swallowed with the rest of its keys.
                            let enter_consumed_by_help =
                                was_help_viewing && key.code == KeyCode::Enter;
                            // the quit-confirm popup's
                            // confirming Enter is consumed the same way:
                            // with the popup open, Enter means "quit", never
                            // "send the draft underneath".
                            let enter_consumed_by_quit_confirm =
                                was_quit_confirm && key.code == KeyCode::Enter;

                            // A confirmed thread deletion removes the persisted
                            // history file (idempotent: a missing file is
                            // success). Consumed here, right after the key, so
                            // the removal happens regardless of what follows.
                            if let Some(removed_id) = app.pending_delete.take() {
                                if let Err(e) =
                                    history::delete_chat_file_in(history_dir, &removed_id)
                                {
                                    eprintln!(
                                        "chibi-tui: could not delete chat history: {e}"
                                    );
                                }
                            }

                            // Popup-requested reconnect (R).
                            if let Some(ReconnectRequest {}) = app.reconnect_requested.take() {
                                if !cli_mock_source(source) {
                                    connect_live(source, &mut app, workspace, event_tx.clone()).await;
                                } else {
                                    // Mocks never go down; treat R as dismiss.
                                    app.dismiss_error();
                                    app.connection = Connection::Connected;
                                }
                            }

                            // Cancel requested by Ctrl+C on the ACTIVE chat's
                            // in-flight request (queued prompts survive).
                            if let Some((request_id, thread_id)) = app.pending_cancel.take() {
                                match source {
                                    Source::Live(live) => {
                                        // Owned clone for the spawned task: the
                                        // handle shares the pipeline actor.
                                        let live = live.clone();
                                        let tx = event_tx.clone();
                                        tokio::spawn(async move {
                                            let _ =
                                                cancel_request(&request_id, &thread_id, &live, tx)
                                                    .await;
                                        });
                                    }
                                    Source::Mock(_) | Source::LivePlaceholder => {
                                        // No backend to receive a cancel frame:
                                        // resolve locally so the spinner cannot
                                        // get stuck.
                                        app.resolve_cancel_locally();
                                    }
                                }
                            }

                            // Per-thread async: submitting into an IDLE chat
                            // sends immediately; into a BUSY chat it enqueues
                            // (take_input already appended the queued bubbles
                            // and returned None). Other chats are never blocked.
                            //
                            // a rename commit takes priority
                            // over submission: its Enter was consumed by the
                            // editor, and the renamed chat is persisted here so
                            // the new title survives restarts.
                            if renamed {
                                // The chat exists by construction here (a
                                // rename commit cannot delete it), but guard
                                // anyway, because `active` may point nowhere after a
                                // delete-into-empty-state.
                                if let Some(chat) = app.chats.get(app.active) {
                                    persist_chat(chat, history_dir);
                                }
                            } else if enter_consumed_by_rename {
                                // Rejected rename save (empty draft): nothing
                                // to do: old name kept, nothing persisted.
                            } else if app.error_popup.is_none()
                                && source.accepts_submissions()
                                && should_submit(&key)
                                && !enter_consumed_by_delete
                                && !enter_consumed_by_stop_reset
                                && !enter_consumed_by_search
                                && !enter_consumed_by_search_all
                                && !enter_consumed_by_sidebar
                                && !enter_consumed_by_picker
                                && !enter_consumed_by_help
                                && !enter_consumed_by_quit_confirm
                            {
                                let submitted = app.take_input();
                                if let Some(submitted) = submitted {
                                    send_submitted(source, &submitted, event_tx.clone());
                                    app.begin_request(&submitted);
                                    persist_chat(&app.chats[app.active], history_dir);
                                } else if app.active_queue_len() > 0 {
                                    // Enqueued into the busy chat's FIFO:
                                    // persist so queued bubbles survive an
                                    // unexpected shutdown too.
                                    persist_chat(&app.chats[app.active], history_dir);
                                }
                            }

                        }
                    }
                    Some(Ok(CtEvent::Resize(_, _))) => {
                        // Repaint immediately on the new size so the layout
                        // (sidebar width, code panels) adapts without waiting
                        // for the next event.
                        terminal.draw(|f| ui::draw(f, &mut app, theme))?;
                    }
                    Some(Ok(CtEvent::Mouse(mouse))) => {
                        // Wheel routing hit-tests against the SAME layout the
                        // renderer just painted: the panel rectangles are
                        // recomputed from the live frame size — there is no
                        // cached geometry anywhere in the render path.
                        let size = terminal.size()?;
                        let area = Rect::new(0, 0, size.width, size.height);
                        mouse::handle_mouse(&mut app, mouse, area);
                    }
                    Some(Ok(CtEvent::Paste(text))) => {
                        // A bracketed paste is NOT a key: it never reaches
                        // `handle_key`, can never match bare Enter, and can
                        // never submit. The whole payload — newlines
                        // included — lands in the draft in one insert.
                        handle_paste(&mut app, &text);
                    }
                    Some(Ok(_)) => {} // focus and other crossterm events
                    Some(Err(e)) => return Err(io::Error::other(e)),
                    None => {} // stream ended; keep looping until quit
                }
            }

            // ---- backend events ----
            Some(evt) = event_rx.recv() => {
                // Per-thread async: after a chat's terminal event, start its
                // next queued prompt (FIFO), including for background chats.
                let background_wire_id = match &evt {
                    BackendEvent::BackgroundMessage { wire_thread_id, .. } => Some(*wire_thread_id),
                    _ => None,
                };
                let drain_thread_id = match &evt {
                    BackendEvent::QueueDrain { thread_id } => Some(thread_id.clone()),
                    _ => None,
                };
                if let Some(wire_id) = background_wire_id {
                    app.apply_backend_event(evt);
                    // Persist the chat the continuation belongs to (routed by
                    // the wire thread id; the active-chat default below misses
                    // background threads).
                    if let Some(chat) = app
                        .chats
                        .iter()
                        .find(|c| chibi_tui::live::wire_thread_id(&c.id) == wire_id)
                    {
                        persist_chat(chat, history_dir);
                    }
                } else if let Some(thread_id) = drain_thread_id {
                    if let Some(next) = app.dequeue_next_for(&thread_id) {
                        send_submitted(source, &next, event_tx.clone());
                    }
                    // after the visible FIFO had its
                    // chance, hand out the next parked HIDDEN request for
                    // the same thread: only when the chat stayed Idle (a
                    // just-started visible prompt keeps it parked for the
                    // next drain).
                    if let Some(hidden) = app.take_deferred_hidden_request(&thread_id) {
                        send_submitted(source, &hidden, event_tx.clone());
                    }
                    if let Some(chat) = app.chats.iter().find(|c| c.id == thread_id) {
                        persist_chat(chat, history_dir);
                    }
                } else {
                    app.apply_backend_event(evt);
                    // Persist the chat the event belongs to (routed by
                    // thread id when present; otherwise the active one).
                    if let Some(chat) = app.chats.get(app.active) {
                        persist_chat(chat, history_dir);
                    }
                }
            }

            // ---- spinner animation (~10 fps while busy) ----
            _ = spinner_tick.tick() => {
                // Animate whenever ANY chat is busy so a background request's
                // sidebar marker keeps pulsing-style freshness; the visible
                // spinner line itself only renders for the ACTIVE chat.
                if app.any_busy() {
                    app.tick_spinner();
                }
                // the transient status toast (busy
                // refusal) auto-expires on the same 100 ms cadence.
                app.tick_status_message();
            }
        }

        note_active_thread(&mut tracked_thread, &app, history_dir);

        if app.should_quit {
            return Ok(());
        }
    }
}

/// Mock sources never reconnect — `R` just clears the popup.
fn cli_mock_source(source: &Source) -> bool {
    matches!(source, Source::Mock(_))
}

/// Hand one accepted submission bundle to the active backend source.
///
/// Used for both immediate sends and per-thread FIFO drains — the bundle
/// carries its own `(request_id, thread_id)` so the backend events route
/// back to the right chat regardless of which chat is on screen.
fn send_submitted(
    source: &mut Source,
    submitted: &chibi_tui::app::Submitted,
    tx: mpsc::Sender<BackendEvent>,
) {
    match source {
        Source::Live(live) => live.submit_encoded(submitted.clone(), tx),
        Source::Mock(mock_backend) => mock_backend.submit(submitted.prompt.clone(), tx),
        Source::LivePlaceholder => {
            // Guarded by `accepts_submissions()` upstream; a drain racing a
            // disconnect must not panic; surface it as a chat error instead.
            let _ = tx.try_send(BackendEvent::Error {
                request_id: 0,
                message: "backend is offline; prompt not sent".to_owned(),
                thread_id: Some(submitted.thread_id.clone()),
            });
        }
    }
}

/// Send the targeted cancel frame for one request. The backend answers with
/// an `error { code: cancelled }` frame that arrives as a normal
/// [`BackendEvent::Error`] and resolves the pending placeholder.
async fn cancel_request(
    request_id: &str,
    thread_id: &str,
    live: &chibi_tui::LiveBackend,
    tx: mpsc::Sender<BackendEvent>,
) {
    if let Err(e) = live.cancel(request_id).await {
        // The pipe is broken or the request vanished: surface it as a popup
        // instead of leaving a stuck spinner.
        let _ = tx
            .send(BackendEvent::Error {
                request_id: 0,
                message: format!("cancel failed ({thread_id}): {e}"),
                thread_id: None,
            })
            .await;
    }
}

/// Persist one chat snapshot; failures are non-fatal (stderr note only).
fn persist_chat(chat: &chibi_tui::app::Chat, dir_override: Option<&std::path::Path>) {
    if let Err(e) = history::save_chat_in(dir_override, chat) {
        eprintln!("chibi-tui: could not save chat history: {e}");
    }
}

/// Build the live-mode startup app: persisted history (or one fresh chat on
/// first run), then re-open the last active thread when both its pointer and
/// the thread's snapshot survived. A missing, unreadable or dangling pointer
/// leaves the default startup selection untouched — no error noise either way.
fn bootstrap_app(dir_override: Option<&std::path::Path>) -> chibi_tui::app::App {
    let mut chats = history::load_chats_from(dir_override);
    if chats.is_empty() {
        chats.push(chibi_tui::app::Chat::new("New chat 1"));
    }
    let mut app = chibi_tui::app::App::new(chats);
    if let Some(id) = history::load_last_thread_in(dir_override) {
        app.activate_thread(&id);
    }
    app
}

/// Persist the active thread pointer whenever activation changed since the
/// previous observation; the first observation records the startup selection
/// itself. Failures are non-fatal (stderr note only), like every other
/// history write.
fn note_active_thread(
    tracked: &mut Option<String>,
    app: &chibi_tui::app::App,
    dir_override: Option<&std::path::Path>,
) {
    let current = app.active_thread_id().map(str::to_owned);
    if current == *tracked {
        return;
    }
    if let Some(id) = current.as_deref() {
        if let Err(e) = history::save_last_thread_in(dir_override, id) {
            eprintln!("chibi-tui: could not save last thread: {e}");
        }
    }
    *tracked = current;
}

/// Paste text into the input draft at the cursor — the shared sink for BOTH
/// paste entry points: the Ctrl+V / Cmd+V clipboard read and the terminal's
/// bracketed-paste event (`Event::Paste`).
///
/// Never submits. Newlines are hard newlines in the draft (the editor's
/// `insert_str` splits them into lines), matching tui-textarea 0.7's paste
/// behavior; `should_submit` stays the ONLY submit path and it runs on bare
/// Enter key events alone — a paste event carries no key at all.
///
/// CRLF (Windows) and bare CR (classic Mac) line endings are normalized to
/// `\n` before insertion so a foreign clipboard never leaves stray `\r`
/// characters in the draft.
///
/// Paste is editor-scoped: while a modal popup, rename, search viewer or the
/// sidebar holds the keyboard, the paste is swallowed — a flood of pasted
/// text must never mutate the message draft that a popup's Enter would
/// otherwise submit. The quit-confirm popup is no exception: it is a flag
/// (not a Mode), so it is checked explicitly — a paste behind it would sit in
/// the draft the confirming Enter must never send.
fn handle_paste(app: &mut chibi_tui::app::App, text: &str) {
    if app.quit_confirm || !app.mode.is_normal() || app.focus != Focus::Chat {
        return;
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.is_empty() {
        app.input.insert_str(&normalized);
    }
}

/// Paste text from the system clipboard into the input field at the cursor.
/// Failures are silent — a locked clipboard must not disturb typing.
fn paste_clipboard(app: &mut chibi_tui::app::App) {
    if let Some(text) = chibi_tui::clipboard::get_text() {
        handle_paste(app, &text);
    }
}

/// Returns true when the given key event should trigger submission of the
/// current input buffer.
///
/// Submit only on BARE Enter — never on every keystroke, and never when a
/// modifier rides along: Shift+Enter and Alt+Enter insert a newline into the
/// textarea instead. This keeps typing fluid and
/// prevents single-letter commands (q/j/k/N) from accidentally submitting
/// when the buffer is still empty.
///
/// Terminal caveat: without the kitty keyboard protocol many terminals send
/// bare-Enter bytes for Shift+Enter, so those presses degrade to submit.
/// `main` pushes crossterm's keyboard-enhancement flags at startup (on
/// non-Windows builds; see `push_kitty_flags`) so capable terminals deliver
/// distinct SHIFT/ALT modifiers.
fn should_submit(key: &crossterm::event::KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers.is_empty()
}

mod mouse;
#[cfg(test)]
mod tests;
