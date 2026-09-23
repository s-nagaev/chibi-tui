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
//! * `Ctrl+C` cancels the in-flight request when one exists; quits only when
//!   idle. `Esc` clears non-empty input and dismisses the error popup.
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
    Event as CtEvent, EventStream, KeyCode, KeyModifiers, KeyboardEnhancementFlags, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use futures_util::StreamExt;
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use chibi_tui::app::{Connection, Focus, Mode, ReconnectRequest};
use chibi_tui::backend::{Backend, BackendEvent};
use chibi_tui::{history, mock, setup_screen, splash, theme, ui};

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
                        handle_mouse(&mut app, mouse, area);
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
/// otherwise submit.
fn handle_paste(app: &mut chibi_tui::app::App, text: &str) {
    if !app.mode.is_normal() || app.focus != Focus::Chat {
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
/// Wheel step per notch: chat rows in the chat panel, one step per entry /
/// line in the modal list surfaces.
const WHEEL_STEP: u16 = 3;

/// Which surface receives the wheel: a modal that owns the screen gets it
/// wherever the cursor is (same modal isolation as the keyboard), the base
/// panels get it routed by cursor position.
enum WheelSurface {
    LogViewer,
    Help,
    ModelPicker,
    Panels,
}

/// Route a crossterm mouse event. Wheel notches scroll (chat, sidebar,
/// modals); left press/drag/release inside the chat pane drive the text
/// selection (see below). Everything else is ignored.
///
/// Routing rules:
/// - an open log viewer / help modal / model picker consumes the wheel
///   regardless of cursor position, mirroring how those modals swallow
///   every key — and their other mouse events stay ignored (a modal owns
///   the whole screen, so no pointer selection can start under it);
/// - cursor over the CHAT panel scrolls the chat (`App::scroll_up` unpins
///   from follow-bottom, `scroll_down` re-pins at 0; `ui::scroll_skip`
///   clamps at render);
/// - cursor over the SIDEBAR moves the thread selection ONLY while the
///   sidebar holds keyboard focus — hovering without focus intentionally
///   does nothing (hover-switching threads was judged too noisy UX);
/// - a left PRESS inside the chat pane (Normal mode, no error popup)
///   starts a drag selection through the render-fed `App::chat_geometry`
///   hit-test seam; DRAG moves the head; RELEASE copies the selected
///   plain text through the SAME clipboard path as the log viewer's `y`
///   (`App::release_selection` turns a press+release without drag into a
///   plain click that clears). Presses outside the chat pane clear the
///   selection too, and a release is finalized anywhere in the base
///   surface — the cursor commonly leaves the pane before the button
///   comes up.
fn handle_mouse(app: &mut chibi_tui::app::App, mouse: MouseEvent, area: Rect) {
    handle_mouse_with_copy(app, mouse, area, &|text| {
        // Selection copy rides the SAME clipboard path as the log
        // viewer's `y` (OSC 52 + the env fallback); the outcome is
        // deliberately ignored — a failed write must not disturb the UI.
        let _ = chibi_tui::clipboard::copy_text(text);
    });
}

/// Testable form of [`handle_mouse`]: the clipboard write arrives as a
/// closure (production passes [`chibi_tui::clipboard::copy_text`], tests
/// capture into a buffer), so the dispatch flow can assert the copy
/// without touching a real clipboard.
fn handle_mouse_with_copy(
    app: &mut chibi_tui::app::App,
    mouse: MouseEvent,
    area: Rect,
    copy: &dyn Fn(&str),
) {
    let surface = match &app.mode {
        Mode::LogViewer { .. } => WheelSurface::LogViewer,
        Mode::HelpViewing { .. } => WheelSurface::Help,
        Mode::ModelPicking { .. } => WheelSurface::ModelPicker,
        _ => WheelSurface::Panels,
    };
    match surface {
        WheelSurface::LogViewer => match mouse.kind {
            MouseEventKind::ScrollUp => app.log_cursor_up(usize::from(WHEEL_STEP)),
            MouseEventKind::ScrollDown => app.log_cursor_down(usize::from(WHEEL_STEP)),
            _ => {}
        },
        WheelSurface::Help => match mouse.kind {
            MouseEventKind::ScrollUp => {
                for _ in 0..WHEEL_STEP {
                    app.help_scroll_up();
                }
            }
            MouseEventKind::ScrollDown => {
                for _ in 0..WHEEL_STEP {
                    app.help_scroll_down();
                }
            }
            _ => {}
        },
        WheelSurface::ModelPicker => match mouse.kind {
            MouseEventKind::ScrollUp => {
                for _ in 0..WHEEL_STEP {
                    app.model_picker_select_prev();
                }
            }
            MouseEventKind::ScrollDown => {
                for _ in 0..WHEEL_STEP {
                    app.model_picker_select_next();
                }
            }
            _ => {}
        },
        WheelSurface::Panels => {
            let rects = ui::layout_rects(area, app.input_lines_height());
            // Selection presses/drags/releases are Normal-mode-only and
            // popup-free: popups (confirm dialogs, rename, search) and the
            // error popup own the whole screen — the pointer must not draw
            // a selection under them, and events under them are swallowed
            // (same isolation as the keyboard). A selection started before
            // a popup opened stays held underneath; a stale live drag is
            // harmless — the next Normal-mode click or Esc clears it.
            // The wheel keeps its pre-existing routing below.
            let selectable = app.mode.is_normal() && app.error_popup.is_none();
            match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    match ui::panel_region(&rects, mouse.column, mouse.row) {
                        ui::PanelRegion::Chat => match mouse.kind {
                            MouseEventKind::ScrollUp => app.scroll_up(WHEEL_STEP),
                            _ => app.scroll_down(WHEEL_STEP),
                        },
                        ui::PanelRegion::Sidebar if app.focus == Focus::Sidebar => {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => app.select_prev(),
                                _ => app.select_next(),
                            }
                        }
                        _ => {}
                    }
                }
                // Finalize the drag anywhere in the base surface (before
                // the region hit-test: the release point may have left the
                // pane).
                MouseEventKind::Up(crossterm::event::MouseButton::Left) if selectable => {
                    if let Some(text) = app.release_selection() {
                        copy(&text);
                    }
                }
                MouseEventKind::Down(crossterm::event::MouseButton::Left) if selectable => {
                    let in_chat =
                        ui::panel_region(&rects, mouse.column, mouse.row) == ui::PanelRegion::Chat;
                    let point = in_chat.then(|| {
                        app.chat_geometry
                            .as_ref()
                            .and_then(|g| g.position_at(mouse.column, mouse.row))
                    });
                    match point.flatten() {
                        Some(point) => app.begin_selection(point),
                        // A press outside the chat pane is a plain click:
                        // it clears the selection (the release completes
                        // the click; nothing live to release).
                        None => app.clear_selection(),
                    }
                }
                MouseEventKind::Drag(crossterm::event::MouseButton::Left) if selectable => {
                    if let Some(point) = app
                        .chat_geometry
                        .as_ref()
                        .and_then(|g| g.position_at(mouse.column, mouse.row))
                    {
                        app.drag_selection(point);
                    }
                }
                _ => {}
            }
        }
    }
}

fn should_submit(key: &crossterm::event::KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers.is_empty()
}

/// Map one Cyrillic character onto its Latin counterpart for the Russian
/// (ЙЦУКЕН) and Ukrainian keyboard layouts, preserving case for letters
/// (uppercase Cyrillic → uppercase Latin, so the case-sensitive
/// `Char('L')` reset shape keeps working). Characters outside both layouts
/// return `None` and pass through unchanged.
fn cyrillic_latin_counterpart(c: char) -> Option<char> {
    let lower = c.to_lowercase().next().unwrap_or(c);
    let mapped = match lower {
        // Shared ЙЦУКЕН top row (the Latin q..p keys).
        'й' => 'q',
        'ц' => 'w',
        'у' => 'e',
        'к' => 'r',
        'е' => 't',
        'н' => 'y',
        'г' => 'u',
        'ш' => 'i',
        'щ' => 'o',
        'з' => 'p',
        // Home row. ы (Russian) and і (Ukrainian) share the Latin `s` key;
        // ї / є / ґ are the Ukrainian-only keys on `]` / `'` / `\`.
        'ф' => 'a',
        'ы' | 'і' => 's',
        'в' => 'd',
        'а' => 'f',
        'п' => 'g',
        'р' => 'h',
        'о' => 'j',
        'л' => 'k',
        'д' => 'l',
        'ь' => 'm',
        // Bottom row (the Latin z..m keys).
        'я' => 'z',
        'ч' => 'x',
        'с' => 'c',
        'м' => 'v',
        'и' => 'b',
        'т' => 'n',
        // Non-letter keys keep their physical Latin twins so the input
        // behavior matches a Latin keyboard key-for-key.
        'х' => '[',
        'ъ' | 'ї' => ']',
        'ж' => ';',
        'э' | 'є' => '\'',
        'б' => ',',
        'ю' => '.',
        'ё' => '`',
        'ґ' => '\\',
        _ => return None,
    };
    if c.is_uppercase() && mapped.is_ascii_alphabetic() {
        Some(mapped.to_ascii_uppercase())
    } else {
        Some(mapped)
    }
}

/// Normalize a Ctrl-chord reported under a Cyrillic keyboard layout onto
/// the Latin chord the key dispatch matches.
///
/// Crossterm reports the LAYOUT character for modified keys, so under
/// ЙЦУКЕН `Ctrl+A` arrives as `Ctrl+Ф` and every chord match silently
/// failed until the user switched layouts. Applied at the very top of
/// [`handle_key`], BEFORE any chord matching, and ONLY to events carrying
/// CONTROL: plain typing (the textarea, the rename and search editors) is
/// returned untouched, characters outside both layouts pass through
/// unchanged, and every modifier rides along — so a normalized chord
/// behaves byte-for-byte like its Latin original, including the
/// case-sensitive `Char('L')` / `Char('l')` stop/reset split and the
/// Ctrl+Shift+F global-search shape.
fn normalize_cyrillic_ctrl_chord(
    mut key: crossterm::event::KeyEvent,
) -> crossterm::event::KeyEvent {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return key;
    }
    if let KeyCode::Char(c) = key.code {
        if let Some(mapped) = cyrillic_latin_counterpart(c) {
            key.code = KeyCode::Char(mapped);
        }
    }
    key
}

/// Apply key handling to app state. Backend interactions (submit, cancel,
/// reconnect) happen in the event loop by observing state changes, keeping
/// this function synchronous and testable.
fn handle_key(app: &mut chibi_tui::app::App, key: crossterm::event::KeyEvent) {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return;
    }
    // Ctrl-chords arrive with the LAYOUT character under the Russian
    // (ЙЦУКЕН) and Ukrainian keyboard layouts (Ctrl+Ф instead of Ctrl+A),
    // which left every chord dead until the layout was switched. Rewrite
    // the char to its Latin counterpart before ANY matching; events without
    // CONTROL are returned untouched, so plain typing into the textarea
    // never sees a changed keystroke.
    let key = normalize_cyrillic_ctrl_chord(key);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Alt+↑/↓ are a full synonym of Ctrl+↑/↓ thread
    // switching (see the match below); macOS Mission Control hijacks
    // Ctrl+arrows system-wide before they ever reach the terminal.
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // ---- modal error popup captures everything ----
    // (Ctrl+R rename is intentionally unreachable while the popup is open:
    // the popup branch returns before any mode handling.)
    if app.error_popup.is_some() {
        match key.code {
            // Reconnect request: executed by the event loop.
            KeyCode::Char('r') | KeyCode::Char('R') => {
                app.reconnect_requested = Some(ReconnectRequest {});
            }
            KeyCode::Esc => app.dismiss_error(),
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            // Any other key just dismisses the popup (stay in the app).
            _ => app.dismiss_error(),
        }
        return;
    }

    // diagnostics log viewer modal ----
    //
    // While the ^G log viewer is open, ONLY viewer keys work: the cursor
    // walks logical lines (↑/↓ and k/j by one line, PgUp/PgDn by a page,
    // g/G to the ends, where G returns to the live tail), `w` toggles wrap.
    // The viewer adds `/` (opens the search prompt), n/N
    // (next/prev match) and `y` (copy the cursor line). While the search
    // prompt is open it owns the keyboard completely: typing edits the
    // pattern, Enter commits, Esc cancels, everything else is swallowed.
    // While the cursor rests on the newest line the view keeps streaming
    // new arrivals in; one step up pins it to the cursor. Esc closes,
    // Ctrl+C quits (same class as the other popups). Everything else
    // (typing, global chords (^N/^R/^D/^T/^L/^F), thread switching) is
    // swallowed so no keystroke leaks into the textarea and no global
    // binding fires. There is no Enter action: the viewer is strictly
    // read-only, so Enter is swallowed and the loop's submit gates need no
    // extra snapshot flag.
    if matches!(app.mode, Mode::LogViewer { .. }) {
        // The open search prompt takes the keyboard before anything else.
        let search_prompt_open = match &app.mode {
            Mode::LogViewer { state } => state.search_buf.is_some(),
            _ => false,
        };
        if search_prompt_open {
            match key.code {
                KeyCode::Char(c) if !ctrl && !alt => app.log_search_push(c),
                KeyCode::Backspace if !ctrl && !alt => app.log_search_pop(),
                KeyCode::Enter => app.log_commit_search(),
                KeyCode::Esc => app.log_cancel_search(),
                KeyCode::Char('c') if ctrl => app.should_quit = true,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::PageUp => app.log_page_up(),
            KeyCode::PageDown => app.log_page_down(),
            // One-line flavor of the same navigation (plain arrows and the
            // vim letters move the cursor, the viewport follows it). The
            // ctrl/alt flavors stay swallowed, same as before.
            KeyCode::Up | KeyCode::Char('k') if !ctrl && !alt => app.log_cursor_up(1),
            KeyCode::Down | KeyCode::Char('j') if !ctrl && !alt => app.log_cursor_down(1),
            KeyCode::Char('g') if !ctrl && !alt => app.log_jump_top(),
            KeyCode::Char('G') => app.log_jump_bottom(),
            KeyCode::Char('w') if !ctrl && !alt => app.log_toggle_wrap(),
            // Search round-trip: `/` opens the prompt (see above), n/N walk
            // the committed matches with wraparound on both ends.
            KeyCode::Char('/') if !ctrl && !alt => app.log_open_search(),
            KeyCode::Char('n') if !ctrl && !alt => app.log_search_next(),
            KeyCode::Char('N') if !ctrl && !alt => app.log_search_prev(),
            // Copy the full cursor line (OSC 52, with the env fallback).
            KeyCode::Char('y') if !ctrl && !alt => app.log_copy_selected(),
            KeyCode::Esc => {
                app.close_log_viewer();
            }
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            _ => {}
        }
        return;
    }

    // model-picker modal captures everything ----
    //
    // While the ^M picker is open, ONLY picker keys work: ↑/↓ move the
    // selection (clamped at the list edges), PgUp/PgDn page the selection
    // by one viewport of the popup's visible rows (same clamp rule; the
    // page size is the rendered list height fed back by the renderer),
    // Enter confirms the highlighted
    // row (stages the hidden `/model <n>` request, a no-op until the
    // listing arrives), Esc closes without acting (a parked fetch is
    // dropped; an already-confirmed selection stays queued), Ctrl+C quits
    // (same class as the other popups). Everything else (typing, global
    // chords (^N/^R/^D/^T/^L/^F/^G/^O), thread switching) is swallowed so
    // no keystroke leaks into the textarea and no global binding fires.
    // There is no text input here: the listing is short enough to navigate
    // directly (task scope). The picker's Enter is additionally gated in the
    // event loop (`enter_consumed_by_picker`) so it can never ALSO submit
    // the message draft.
    if matches!(app.mode, Mode::ModelPicking { .. }) {
        match key.code {
            KeyCode::Up => app.model_picker_select_prev(),
            KeyCode::Down => app.model_picker_select_next(),
            KeyCode::PageUp => app.model_picker_page_up(),
            KeyCode::PageDown => app.model_picker_page_down(),
            KeyCode::Enter => app.confirm_model_picker(),
            KeyCode::Esc => {
                app.close_model_picker();
            }
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            _ => {}
        }
        return;
    }

    // modal confirm popup captures everything ----
    //
    // While the Ctrl+D confirmation is open, ONLY the destructive decision
    // keys work: Enter/`y` confirm, Esc/`n` cancel, Ctrl+C quits (same
    // class as the error popup's Ctrl+C). Everything else (typing, arrows,
    // Ctrl+N/R/L/F) is swallowed so no keystroke leaks into the textarea
    // and no global binding fires. `q` is deliberately left UNBOUND here
    // (unlike the error popup): a stray `q` must never quit while a
    // destructive confirmation is on screen.
    if matches!(app.mode, Mode::ConfirmDelete) {
        match key.code {
            KeyCode::Enter => {
                app.confirm_delete();
            }
            // Plain y/Y confirm (Ctrl+Y is swallowed like any other combo).
            KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl => {
                app.confirm_delete();
            }
            KeyCode::Esc => {
                app.cancel_delete();
            }
            // Plain n/N cancel (Ctrl+N stays suspended: new-chat must not
            // fire and must not cancel the popup either).
            KeyCode::Char('n') | KeyCode::Char('N') if !ctrl => {
                app.cancel_delete();
            }
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            _ => {}
        }
        return;
    }

    // stop/reset confirm popup captures ----
    // everything ----
    //
    // While the ^L (stop) or ⇧^L (reset) confirmation is open, ONLY the
    // destructive decision keys work: Enter/`y` confirm, Esc/`n`/same-chord
    // cancel (the chord that opened the popup: ^L for stop, ⇧^L for reset),
    // Ctrl+C quits — the exact grammar of the delete confirm. Everything
    // else (typing, arrows, global chords) is swallowed so no keystroke
    // leaks into the textarea and no global binding fires. `q` is
    // deliberately unbound here (same destructive-popup rule as the delete
    // confirm).
    if matches!(app.mode, Mode::ConfirmStopReset { .. }) {
        match key.code {
            KeyCode::Enter => {
                app.confirm_stop_reset();
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl => {
                app.confirm_stop_reset();
            }
            KeyCode::Esc => {
                app.cancel_stop_reset();
            }
            KeyCode::Char('n') | KeyCode::Char('N') if !ctrl => {
                app.cancel_stop_reset();
            }
            KeyCode::Char('l') | KeyCode::Char('L')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // Same-chord cancel: ^L (stop) and ⇧^L in both kitty
                // variants (reset) close the popup without staging anything.
                app.cancel_stop_reset();
            }
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            _ => {}
        }
        return;
    }

    // modal search popup captures everything ----
    //
    // While the Ctrl+F search popup is open, ONLY search keys work: plain
    // chars edit the query (live recompute), Backspace edits backwards,
    // ↑/↓ navigate matches, Enter jumps to the selected match and closes,
    // Esc closes without jumping, Ctrl+C quits (same class as the other
    // popups). Everything else (PgUp/PgDn, arrows, Ctrl+N/R/L/D, q) is
    // swallowed so no keystroke leaks into the textarea and no global
    // binding fires. Chat scroll is driven ONLY by the jump, never by
    // PgUp/PgDn while the popup is open.
    if matches!(app.mode, Mode::Searching { .. }) {
        match key.code {
            KeyCode::Up => app.search_select_prev(),
            KeyCode::Down => app.search_select_next(),
            KeyCode::Enter => {
                app.jump_to_selected();
            }
            KeyCode::Esc => {
                app.cancel_search();
            }
            KeyCode::Backspace => app.search_backspace(),
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            KeyCode::Char(ch) if !ctrl => app.search_push(ch),
            _ => {}
        }
        return;
    }

    // modal GLOBAL search popup captures ----
    // everything ----
    //
    // Same modal-ish isolation as the in-thread search popup: while the
    // Ctrl+Shift+F popup is open, ONLY search keys work. The one behavioral
    // difference is Enter: it ACTIVATES the match's thread (switching the
    // active chat, same mechanics as Ctrl+↑/↓) before recording the jump.
    // Everything else (PgUp/PgDn, arrows, Ctrl+N/R/L/D/F) is swallowed so
    // no keystroke leaks into the textarea and no global binding fires.
    if matches!(app.mode, Mode::SearchingAll { .. }) {
        match key.code {
            KeyCode::Up => app.search_all_select_prev(),
            KeyCode::Down => app.search_all_select_next(),
            KeyCode::Enter => {
                app.jump_to_selected_all();
            }
            KeyCode::Esc => {
                app.cancel_search_all();
            }
            KeyCode::Backspace => app.search_all_backspace(),
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            KeyCode::Char(ch) if !ctrl => app.search_all_push(ch),
            _ => {}
        }
        return;
    }

    // keybindings help modal captures everything ----
    //
    // While the F1 help modal is open, ONLY viewer keys work: ↑/↓ scroll the
    // static keybindings table one line and PgUp/PgDn one page (both clamped
    // at the table's edges over the render-fed viewport — the same seam the
    // picker and the log viewer page by). F1 toggles closed (same-chord
    // semantics) and Esc closes; Ctrl+C quits (same class as the other
    // popups). There is no Enter action: the table is strictly read-only,
    // so Enter is swallowed and the loop's submit gate carries the same
    // help-modal snapshot the other popups have. Everything else (typing,
    // global chords, thread switching) is swallowed so no keystroke leaks
    // into the textarea and no global binding fires.
    if matches!(app.mode, Mode::HelpViewing { .. }) {
        match key.code {
            KeyCode::Up => app.help_scroll_up(),
            KeyCode::Down => app.help_scroll_down(),
            KeyCode::PageUp => app.help_page_up(),
            KeyCode::PageDown => app.help_page_down(),
            KeyCode::Esc | KeyCode::F(1) => {
                app.close_help_modal();
            }
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            _ => {}
        }
        return;
    }

    // ---- global quit / cancel semantics ----
    //
    // Ctrl+C cancels the in-flight request when one exists, otherwise quits,
    // in EVERY mode (an open rename session does not trap Ctrl+C; it cancels
    // the draft implicitly via the normal quit/cancel path).
    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        if app.cancel_rename() {
            return; // Esc-equivalent: drop the draft first, stay consistent.
        }
        if let Some((request_id, thread_id)) = app.cancel_active() {
            app.pending_cancel = Some((request_id, thread_id));
        } else {
            app.should_quit = true;
        }
        return;
    }

    // ---- inline thread rename mode ----
    //
    // Checked BEFORE normal input handling so keystrokes never leak into the
    // prompt textarea while a rename session is open. Entered with Ctrl+R in
    // Normal mode only (re-entry is a no-op inside App::begin_rename).
    if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) && ctrl && app.mode.is_normal() {
        app.begin_rename();
        return;
    }
    if let Mode::Renaming { .. } = app.mode {
        match key.code {
            // Shift+Enter / Alt+Enter insert a
            // newline into the rename draft instead of saving. The sidebar
            // renders `\n` as a space and persistence round-trips it, so
            // multi-line titles are safe end to end.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                if let Mode::Renaming { buf } = &mut app.mode {
                    buf.push('\n');
                }
            }
            KeyCode::Enter => {
                // Save handled here; persistence happens right after in the
                // event loop (same pattern as message submission).
                app.commit_rename();
            }
            KeyCode::Esc => {
                app.cancel_rename();
            }
            KeyCode::Backspace => app.rename_backspace(),
            KeyCode::Up | KeyCode::Down => {
                // Thread navigation stays blocked mid-rename, in ALL
                // modifier flavors: Ctrl+↑/↓ AND Alt+↑/↓ switch threads in
                // Normal mode,
                // but never under an open editor; plain ↑/↓ are consumed by
                // the block as caret no-ops). Switching the active chat
                // under an open rename would be confusing.
            }
            _ => {
                if let KeyCode::Char(ch) = key.code {
                    // Plain characters go straight into the draft…
                    app.rename_push(ch);
                } else {
                    // …everything else (arrows/Home/End/Ctrl+A/E/U/L word ops)
                    // is ignored: the draft is a plain single-line string, so
                    // only typing + backspace exist. Ctrl combos never reach
                    // here except modifiers-only presses, which are no-ops.
                    let _ = chibi_tui::input::Input::from(key);
                }
            }
        }
        return;
    }

    // the SIDEBAR owns the keyboard --------------------
    //
    // While the sidebar holds focus ONLY navigation + service keys work;
    // everything else is swallowed so no keystroke can ever leak into the
    // prompt textarea (Shift+Enter / Alt+Enter newlines, readline edits,
    // paste, all included). This branch sits AFTER the modal popup and
    // rename branches (those always capture first) and BEFORE the editor
    // fall-through:
    //
    // * ↑/↓ move the selection with LIVE active-chat switching: the same
    //   clamp + follow-bottom mechanics as Ctrl+↑/↓ in Normal mode; every
    //   arrow flavor routes here identically.
    // * bare Enter applies and returns focus to Chat (active chat stays
    //   highlighted; the event loop suppresses submission for it via
    //   `enter_consumed_by_sidebar`); Esc returns WITHOUT touching the
    //   draft, deliberately different from Normal-mode Esc (clears input).
    // * PgUp/PgDn STILL scroll the CHAT pane: reading works regardless of
    //   focus (documented choice).
    // * Global service chords stay live with their exact Normal-mode
    //   semantics (parity by contract with the chord match below): ^T
    //   toggles back to Chat, ^F / ^⇧F open the search popups (each close
    //   resets focus to Chat via App), ^N creates a chat (App lands focus
    //   on Chat), ^D opens the guarded confirm popup, ^G opens the
    //   diagnostics log viewer, ^L clears input + screen. ^C quit/cancel
    //   is serviced even earlier (global section).
    //   ^R rename also never reaches this branch: its entry guard sits
    //   above, so renaming from a Sidebar-focused UI works and closing the
    //   session returns focus to Chat (App::commit/cancel_rename).
    if app.focus == Focus::Sidebar {
        match key.code {
            KeyCode::Char('f') if ctrl && key.modifiers.contains(KeyModifiers::SHIFT) => {
                app.begin_search_all();
            }
            KeyCode::Char('t') if ctrl => app.toggle_focus(),
            KeyCode::Char('n') if ctrl => app.new_chat(),
            KeyCode::Char('d') if ctrl => app.begin_delete_confirm(),
            // read-only viewer = service chord; same
            // Normal-mode ^G semantics under sidebar focus (parity contract).
            KeyCode::Char('g') if ctrl => app.begin_log_viewer(),
            // same Normal-mode ^O semantics under sidebar
            // focus (parity contract with the chord match below).
            KeyCode::Char('o') if ctrl => app.toggle_status_strip(),
            // same Normal-mode ^S semantics under sidebar
            // focus (parity contract with the chord match below).
            KeyCode::Char('s') if ctrl => app.toggle_thoughts(),
            // same Normal-mode ^M semantics under
            // sidebar focus (parity contract with the chord match below).
            KeyCode::Char('m') if ctrl => app.begin_model_picker(),
            // same Normal-mode F1 semantics under
            // sidebar focus (parity contract with the chord match below).
            KeyCode::F(1) => app.begin_help_modal(),
            // same Normal-mode ^P semantics under sidebar
            // focus (parity contract with the chord match below).
            KeyCode::Char('p') if ctrl => app.begin_clone_thread(),
            // same pair as the Normal-mode ^L /
            // ⇧^L arms below (guarded confirm popups; idle ^L is a no-op).
            KeyCode::Char('L') if ctrl => app.begin_reset_confirm(),
            KeyCode::Char('l') if ctrl => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    app.begin_reset_confirm();
                } else {
                    app.begin_stop_confirm();
                }
            }
            KeyCode::Char('f') if ctrl => app.begin_search(),
            // Chat-pane scrolling regardless of focus.
            KeyCode::PageUp => app.scroll_up(app.chat_visible_rows),
            KeyCode::PageDown => app.scroll_down(app.chat_visible_rows),
            // Selection navigation with live active-chat switching (all
            // modifier flavors behave identically here).
            KeyCode::Up => app.select_prev(),
            KeyCode::Down => app.select_next(),
            // Apply & return to the editor, with no submit side effect.
            KeyCode::Enter if key.modifiers.is_empty() => app.focus = Focus::Chat,
            // Return without touching the draft (never clears input);
            // a mouse selection is cleared like in Normal mode.
            KeyCode::Esc => {
                app.clear_selection();
                app.focus = Focus::Chat;
            }
            // Everything else is swallowed while the sidebar is focused.
            _ => {}
        }
        return;
    }

    // ---- extended input keybindings (readline-style) ----
    // Paste accepts Ctrl+V and macOS Cmd+V (crossterm reports the Command
    // key as META).
    if matches!(key.code, KeyCode::Char('v'))
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::META)
    {
        paste_clipboard(app);
        return;
    }
    match (key.code, ctrl) {
        // F1: TOGGLE THE KEYBINDINGS HELP MODAL,
        // the centered popup listing every chord the dispatch handles,
        // built from the single-source table in `ui.rs` that the tests pin
        // against the real handlers.
        //
        // Chord verification (feat task discipline, same audit class as the
        // ^G/^O/^S/^M/^P entries): the first candidate `?` was REJECTED —
        // a plain char falls through to the textarea's `(_, _)` arm and
        // inserts into the draft, so binding it would make a literal
        // question mark untypable in prompts. Ctrl+H was REJECTED too:
        // the editor maps Char('h') + CONTROL to backspace (the same
        // arm as Backspace, kept from tui-textarea 0.7), and several terminal
        // setups deliver the physical Backspace key as ASCII BS, i.e.
        // Char('h') + CONTROL — the chord IS the editor's backspace today.
        // F(1) is verified FREE: no binding anywhere in src/ (no
        // KeyCode::F hit at all), no editor mapping (unknown keys are
        // inert in the readline fall-through), and
        // no degradation cliff — F1 has a dedicated escape sequence on
        // legacy terminals as well, so the chord works with or without the
        // kitty keyboard protocol. Mnemonic: the universal help key.
        (KeyCode::F(1), _) => {
            app.begin_help_modal();
            return;
        }
        // Ctrl+L: stop the RUNNING request via
        // the guarded confirm popup; the backend intercepts the staged
        // `/stop` prompt pre-LLM and reuses the telegram handler core
        // (task cancel + subagent counter kill-flush). Idle is a silent
        // no-op: nothing to stop, and the retired screen-wipe semantics
        // taught that a visible idle action invites accidental clears.
        // Chord verification (same audit class as ^F/^G/^O/^S/^M): ^L never
        // reaches the editor — the dispatch claims the chord before the
        // readline fall-through ever sees it, so the chord is free.
        (KeyCode::Char('l'), true) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            // Kitty-protocol variant that reports the unshifted char with
            // the SHIFT flag: reset, same as the ⇧^L arm below.
            app.begin_reset_confirm();
            return;
        }
        (KeyCode::Char('l'), true) => {
            app.begin_stop_confirm();
            return;
        }
        // Shift+Ctrl+L: reset the thread via the
        // guarded confirm popup; the staged `/reset` prompt reaches the
        // backend out-of-band and a confirmed ack clears the local dialog.
        // Kitty protocol delivers the chord as Char('L') + CONTROL (the
        // SHIFT flag may ride along); terminals WITHOUT it degrade to plain
        // ^L (stop) — documented in the README, same class as ^⇧F.
        (KeyCode::Char('L'), true) => {
            app.begin_reset_confirm();
            return;
        }
        (KeyCode::Char('u'), true) => {
            // Delete from cursor to start of line. The old tui-textarea
            // engine mapped Ctrl+U to undo, which surprised readline
            // users; the in-house editor keeps readline semantics.
            app.input.delete_line_by_head();
            return;
        }
        // Ctrl+N: new chat.
        (KeyCode::Char('n'), true) => {
            app.new_chat();
            return;
        }
        // Ctrl+D: delete the active thread (idle only) via the confirm
        // popup. Busy/queued chats are refused with a status toast inside
        // App::begin_delete_confirm: the popup never opens there.
        (KeyCode::Char('d'), true) => {
            app.begin_delete_confirm();
            return;
        }
        // Ctrl+G: open the diagnostics log viewer.
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): the first-choice ^Y candidate was REJECTED:
        // the readline family binds ^Y to paste-from-kill-ring (kept
        // from tui-textarea 0.7, src/textarea.rs:589), and the kill ring
        // IS populated in this app:
        // our own ^U override calls delete_line_by_head() → delete_piece()
        // which stores the killed text (textarea.rs:1022), so ^U→^Y (kill
        // line, paste it back) is live behavior today. Taking ^Y would break
        // that readline kill/yank family (^U/^K/^W/^Y). ^G is verified FREE:
        // no app binding anywhere in src/, no editor mapping, no
        // macOS system hijack, no flow-control semantics, and its readline
        // meaning (abort) has no function in this TUI. Mnemonic: loG.
        (KeyCode::Char('g'), true) => {
            app.begin_log_viewer();
            return;
        }
        // Ctrl+O: TOGGLE THE STATUS STRIP, the dim
        // one-row `cwd: <workspace> · <model>` readout on the chat pane's
        // top border. Visible by default; pure view state (like ^T's
        // Focus), modals swallow the chord like every other one.
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): ^G was already taken by the log viewer, so
        // the other task candidate ^O was verified FREE: no app binding
        // anywhere in src/ (only `Char('o')` hits are plain typing), no
        // editor shortcut (the readline table covers a/b/d/e/f/h/j/k/n/p/
        // u/w/y/<>/[], no 'o'), not part of this app's readline
        // family (^A/^E/^U/^K/^W/^Y/^L; GNU readline's operate-and-get-
        // next is a shell-side binding that never fires inside the TUI), no
        // macOS system hijack (Mission Control only takes ^arrows), and the
        // legacy tty VDISCARD semantics of ^O are inert under raw mode plus
        // the kitty keyboard protocol. Mnemonic: infO.
        (KeyCode::Char('o'), true) => {
            app.toggle_status_strip();
            return;
        }
        // Ctrl+S: TOGGLE THE THOUGHTS BLOCK, the dim
        // reasoning trace rendered above the latest answer. Session-only
        // view state (default ON) like the ^O strip: the flip only changes
        // rendering — nothing is cleared and reasoning is never persisted.
        //
        // Chord verification (feat task discipline, same audit class as the
        // ^G/^O/^M/^P entries): no app binding anywhere in src/ (the only
        // `Char('s')` hits are plain typing), no editor shortcut (the
        // readline table covers a/b/d/e/f/h/j/k/n/p/u/w/y/<>/[], no 's'),
        // not part of this app's readline family (^A/^E/^U/^K/^W/
        // ^Y/^L), no macOS system hijack (Mission Control only takes
        // ^arrows), and the legacy tty IXON flow-control meaning of ^S is
        // inert under raw mode (crossterm enables raw at startup).
        // Mnemonic: thoughtS.
        (KeyCode::Char('s'), true) => {
            app.toggle_thoughts();
            return;
        }
        // Ctrl+M: OPEN THE MODEL PICKER, the
        // centered popup that fetches the bare `/model` listing as a hidden
        // exchange (no transcript bubbles) and switches models by sending
        // `/model <n>` the same hidden way.
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): the readline editor maps Ctrl+M to
        // insert_newline() (the same arm as Enter, kept from tui-textarea
        // 0.7),
        // which this binding deliberately OVERRIDES exactly like the
        // existing ^U undo override: the global match claims the chord and
        // returns before the textarea ever sees it, and no app flow relies
        // on a ^M newline (bare Enter already inserts one). The kitty
        // keyboard protocol pushed at startup (DISAMBIGUATE_ESCAPE_CODES)
        // makes capable terminals deliver ^M as an event distinct from
        // Enter; on legacy terminals ^M degrades to bare Enter, with a
        // non-empty draft that SUBMITS it (README-documented caveat, same
        // class as the Shift+Enter degradation). No macOS system hijack
        // (Mission Control only takes ^arrows); GNU readline's C-M
        // accept-line is a shell-side binding that never fires inside the
        // TUI (same argument as the ^O audit). Mnemonic: Model.
        (KeyCode::Char('m'), true) => {
            app.begin_model_picker();
            return;
        }
        // Ctrl+P: CLONE THE ACTIVE THREAD, the backend
        // command /new_thread_with_current_context sent on the NEW thread's
        // identity so the clone inherits the source's full conversation
        // context. Gated by feature detection: without the command in the
        // handshake capabilities the chord shows an informative popup
        // instead of acting (App::begin_clone_thread owns all the guards).
        //
        // Chord verification (feat task discipline, same audit class as the
        // ^G/^O/^M entries): no app binding anywhere in src/ (the only `p`
        // hits are the splash art color table), and the readline editor maps
        // Ctrl+P to move-cursor-up (kept from tui-textarea 0.7), which this
        // binding deliberately
        // OVERRIDES exactly like the ^U undo override: the global match
        // claims the chord and returns before the textarea ever sees it, and
        // no app flow relies on a ^P caret move (plain ↑ is the caret
        // movement here). No macOS system hijack (Mission Control only takes
        // ^arrows); the legacy tty VDISCARD-style meaning of ^P is inert
        // under raw mode plus the kitty keyboard protocol. Mnemonic: P for
        // photocopy.
        (KeyCode::Char('p'), true) => {
            app.begin_clone_thread();
            return;
        }
        // Ctrl+Shift+F: GLOBAL search across ALL threads. With the
        // kitty keyboard protocol
        // (pushed at startup) Ctrl+Shift+F arrives as Char('f') +
        // CONTROL|SHIFT; on terminals WITHOUT it the Shift modifier is
        // lost and the chord degrades to plain Ctrl+F (in-thread search),
        // documented in the README. Guarded inside App::begin_search_all
        // (Normal mode only), so it can never fire over the confirm/rename/
        // search popups: those branches return before this match runs.
        (KeyCode::Char('f'), true) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.begin_search_all();
            return;
        }
        // Ctrl+F: open the in-thread search popup.
        // Guarded inside App::begin_search (Normal mode + active chat
        // only), so this can never fire over the confirm/rename popups;
        // those branches return before this match runs.
        (KeyCode::Char('f'), true) => {
            app.begin_search();
            return;
        }
        // Ctrl+T: TOGGLE PANE FOCUS, which flips the keyboard
        // between Chat (editor) and Sidebar. Replaces the old wrap-cycling
        // thread switcher, which live-check feedback rejected ("just moves
        // the selection down"). Deliberately a plain Ctrl+letter chord so it
        // works in ANY terminal. Modal modes swallow this like every other
        // chord: the error/confirm/search branches and the rename branch
        // return before this match runs.
        (KeyCode::Char('t'), true) => {
            app.toggle_focus();
            return;
        }
        // Ctrl+A / Ctrl+E reach the editor's built-in readline mappings
        // (head/end of line); they fall through untouched below.
        _ => {}
    }

    // ---- vertical arrows & thread switching --------------------------------
    // * Ctrl+↑ / Ctrl+↓ AND Alt+↑ / Alt+↓ switch the ACTIVE THREAD, carrying
    //   over EXACTLY the semantics plain ↑/↓ had before this rework:
    //   App::select_prev/next bounds-clamp the index and reset the chat
    //   scroll to 0; sidebar focus/dot refresh derives from `active` at draw
    //   time, so it follows for free. Alt is a FULL SYNONYM (not a fallback):
    //   both flavors route to the identical select_prev/select_next call:
    //   zero behavior divergence. Alt exists because macOS Mission Control
    //   hijacks Ctrl+arrows system-wide before they reach the terminal.
    // * Plain ↑ / ↓ move the TEXT CURSOR vertically inside the editor via
    //   the editor's native Up/Down mapping (caret up/down, column
    //   preserved and clamped). They
    //   never submit and never switch threads; the caret auto-follows the
    //   grown editor viewport because ui::draw renders the widget over
    //   the full grown block every frame.
    let input_is_empty = app.input.lines().iter().all(|l| l.is_empty());

    match (key.code, ctrl) {
        (KeyCode::Up, true) => app.select_prev(),
        (KeyCode::Down, true) => app.select_next(),
        // Alt+↑/↓ (ctrl unset): same select_prev/next mechanics as the ctrl
        // flavor above. When BOTH modifiers ride along, the ctrl arm wins:
        // identical to pre-alt behavior.
        (KeyCode::Up, false) if alt => app.select_prev(),
        (KeyCode::Down, false) if alt => app.select_next(),
        // Plain (and Shift-decorated) vertical arrows go straight into the
        // textarea's readline-compatible handler: caret movement only.
        // Alt+↑/↓ never reach this arm (the synonym arms above take them).
        (KeyCode::Up | KeyCode::Down, _) => {
            let converted: chibi_tui::input::Input = key.into();
            app.input.input(converted);
        }
        (KeyCode::PageUp, _) => app.scroll_up(app.chat_visible_rows),
        (KeyCode::PageDown, _) => app.scroll_down(app.chat_visible_rows),
        (KeyCode::Esc, _) if !input_is_empty => {
            // Non-empty input: clear it (and any mouse selection with it).
            app.clear_selection();
            app.clear_input();
        }
        // Esc with an empty input clears the mouse selection (the
        // keyboard's "deselect"); it still never quits.
        (KeyCode::Esc, _) => app.clear_selection(),
        // Only Ctrl+C quits (idle) or cancels (busy).
        // BARE Enter is swallowed here and submitted by the loop's
        // should_submit() gate; Shift+Enter / Alt+Enter fall through to the
        // textarea as newline inserts. On terminals WITHOUT the kitty
        // keyboard protocol, Shift+Enter arrives as bare Enter bytes and
        // degrades to submit, as documented in the README.
        (KeyCode::Enter, _)
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            app.input.insert_newline();
        }
        (KeyCode::Enter, _) => {}
        // Everything else (including Ctrl+A/E, Alt+B/F, Alt+D word ops)
        // goes into the textarea's readline-compatible handler.
        (_, _) => {
            let converted: chibi_tui::input::Input = key.into();
            app.input.input(converted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chibi_tui::app::Chat;
    use chibi_tui::model::Message;

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, modifiers)
    }

    fn press(app: &mut chibi_tui::app::App, code: KeyCode, modifiers: KeyModifiers) {
        handle_key(app, key_event(code, modifiers));
    }

    fn app_with_chats(n: usize) -> chibi_tui::app::App {
        let chats = (0..n)
            .map(|i| Chat::new(format!("chat-{i}")))
            .collect::<Vec<_>>();
        chibi_tui::app::App::new(chats)
    }

    fn submit_text(app: &mut chibi_tui::app::App, text: &str) -> chibi_tui::app::Submitted {
        for ch in text.chars() {
            app.input.input(chibi_tui::input::Input {
                key: chibi_tui::input::Key::Char(ch),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
        let submitted = app.take_input().expect("prompt taken");
        app.begin_request(&submitted);
        submitted
    }

    /// Type plain characters through the full `handle_key` path (as a real
    /// keyboard would), so tests cover the routing, not just the textarea.
    fn type_in(app: &mut chibi_tui::app::App, text: &str) {
        for ch in text.chars() {
            press(app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    // ---- cancel hotkey -----------------------------------------------------

    // bootstrap restore + pointer updates --------

    fn temp_history_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chibi-tui-last-thread-{tag}-{}-{}",
            std::process::id(),
            chibi_tui::history::new_thread_id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn persisted_chat(
        name: &str,
        usage: Option<chibi_tui::protocol::Usage>,
    ) -> chibi_tui::app::Chat {
        let mut chat = Chat::new(name);
        chat.messages.push(Message::user("question"));
        chat.last_usage = usage;
        chat
    }

    #[test]
    fn bootstrap_reopens_the_last_active_thread_with_its_sticky_state() {
        let dir = temp_history_dir("restore");
        let first = persisted_chat("first", None);
        let second = persisted_chat(
            "second",
            Some(chibi_tui::protocol::Usage {
                input_tokens: 900_000,
                output_tokens: 1,
                context_window: Some(1_000_000),
            }),
        );
        history::save_chat_in(Some(&dir), &first).expect("save first");
        history::save_chat_in(Some(&dir), &second).expect("save second");
        history::save_last_thread_in(Some(&dir), &second.id).expect("save pointer");

        let app = bootstrap_app(Some(&dir));
        assert_eq!(
            app.active_thread_id(),
            Some(second.id.as_str()),
            "startup re-opens the remembered thread"
        );
        assert_eq!(app.chat_title(), "second");
        assert_eq!(
            app.last_turn_usage.map(|u| u.input_tokens),
            Some(900_000),
            "sticky ctx state is seeded from the restored thread's snapshot"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bootstrap_with_missing_dangling_or_corrupt_pointer_uses_default_startup() {
        // Missing pointer: plain default startup (first sorted snapshot).
        let dir = temp_history_dir("no-pointer");
        let a = persisted_chat("a", None);
        let b = persisted_chat("b", None);
        history::save_chat_in(Some(&dir), &a).expect("save a");
        history::save_chat_in(Some(&dir), &b).expect("save b");
        let app = bootstrap_app(Some(&dir));
        assert_eq!(app.active, 0, "no pointer: default startup selection");
        assert_eq!(app.chats.len(), 2, "nothing lost either");
        std::fs::remove_dir_all(&dir).ok();

        // Dangling pointer: the remembered id has no thread file.
        let dir = temp_history_dir("dangling");
        let only = persisted_chat("only", None);
        history::save_chat_in(Some(&dir), &only).expect("save only");
        history::save_last_thread_in(Some(&dir), &chibi_tui::history::new_thread_id())
            .expect("dangling pointer");
        let app = bootstrap_app(Some(&dir));
        assert_eq!(app.active, 0, "dangling pointer: default startup");
        assert_eq!(app.active_thread_id(), Some(only.id.as_str()));
        std::fs::remove_dir_all(&dir).ok();

        // Corrupt pointer: unreadable JSON must never panic or disturb the
        // startup selection.
        let dir = temp_history_dir("corrupt");
        let only = persisted_chat("only", None);
        history::save_chat_in(Some(&dir), &only).expect("save only");
        std::fs::write(dir.join("last-thread.json"), "{broken").expect("corrupt pointer");
        let app = bootstrap_app(Some(&dir));
        assert_eq!(app.active, 0, "corrupt pointer: default startup");
        assert_eq!(app.active_thread_id(), Some(only.id.as_str()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switching_threads_updates_the_persisted_pointer() {
        let dir = temp_history_dir("switch");
        let app = app_with_chats(2);
        let first_id = app.chats[0].id.clone();
        let second_id = app.chats[1].id.clone();

        // The run loop's exact seam: observe, then switch, then observe.
        let mut tracked: Option<String> = None;
        note_active_thread(&mut tracked, &app, Some(&dir));
        assert_eq!(
            history::load_last_thread_in(Some(&dir)).as_deref(),
            Some(first_id.as_str()),
            "the startup selection is recorded"
        );

        let mut app = app;
        app.select_next();
        note_active_thread(&mut tracked, &app, Some(&dir));
        assert_eq!(
            history::load_last_thread_in(Some(&dir)).as_deref(),
            Some(second_id.as_str()),
            "a thread switch rewrites the pointer"
        );

        app.select_prev();
        note_active_thread(&mut tracked, &app, Some(&dir));
        assert_eq!(
            history::load_last_thread_in(Some(&dir)).as_deref(),
            Some(first_id.as_str()),
            "switching back is recorded too"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ctrl_c_while_busy_requests_cancel_instead_of_quit() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "long running");
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(app.pending_cancel.is_some(), "cancel requested");
        let (request_id, _) = app.pending_cancel.clone().unwrap();
        assert_eq!(Some(request_id.as_str()), app.active_request_id());
        assert_eq!(request_id, submitted.request_id);
        assert!(!app.should_quit, "busy Ctrl+C must not quit");
    }

    /// Ctrl+C with queued prompts still cancels only the in-flight request;
    /// the queue is untouched (it drains via terminal events).
    #[test]
    fn ctrl_c_with_queued_prompts_cancels_inflight_only() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "in flight");
        // Enqueue a second prompt while busy: type the text, then submit via
        // the same path the event loop uses (take_input enqueues because the
        // chat is busy).
        for ch in "queued".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert!(app.take_input().is_none(), "busy chat enqueues");
        assert_eq!(app.active_queue_len(), 1);

        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(
            app.pending_cancel.is_some(),
            "cancel targets in-flight only"
        );
        assert_eq!(app.active_queue_len(), 1, "queue survives the cancel");
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_c_when_idle_quits() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.pending_cancel.is_none());
        assert!(app.should_quit);
    }

    // ^P routing --------------------------------------

    #[test]
    fn ctrl_p_routes_to_clone_flow_when_supported() {
        let mut app = app_with_chats(1);
        app.set_backend_commands(vec!["/new_thread_with_current_context".to_owned()]);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);

        assert!(
            app.take_clone_submission().is_some(),
            "^P stages the clone request"
        );
    }

    #[test]
    fn ctrl_p_without_capability_shows_popup_and_stages_nothing() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);

        assert!(app.error_popup.is_some(), "informative popup");
        assert!(app.take_clone_submission().is_none());
    }

    #[test]
    fn ctrl_p_works_from_sidebar_focus() {
        let mut app = app_with_chats(1);
        app.set_backend_commands(vec!["/new_thread_with_current_context".to_owned()]);
        app.focus = Focus::Sidebar;
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);

        assert!(
            app.take_clone_submission().is_some(),
            "sidebar holds focus but the service chord still fires"
        );
    }

    #[test]
    fn esc_clears_input_or_is_ignored_but_never_quits() {
        let mut app = app_with_chats(1);

        // Esc while busy: ignored (no quit, no clear — nothing to clear).
        submit_text(&mut app, "in flight");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.should_quit, "Esc during a request must not quit");

        // After cancel resolves, still idle: Esc is still ignored.
        app.resolve_cancel_locally();
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(
            !app.should_quit,
            "Esc in idle must never quit — only Ctrl+C quits"
        );

        // Non-empty input: Esc clears it.
        for ch in "hello".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(
            app.input.lines().iter().all(|l| l.is_empty()),
            "Esc clears non-empty input"
        );
        assert!(!app.should_quit, "Esc after clearing input must not quit");
    }

    // Single-letter commands (q/j/k/N) were removed — they conflicted with the
    // first character of typed words. Navigation uses ↑/↓; new chat uses Ctrl+N;
    // quit uses Ctrl+C only.

    #[test]
    fn ctrl_n_creates_new_chat() {
        let mut app = app_with_chats(1);
        assert_eq!(app.chats.len(), 1);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(app.chats.len(), 2, "Ctrl+N creates a new chat");
    }

    //------------------------------------------------------------------------

    fn busy_app_with_commands(commands: &[&str]) -> chibi_tui::app::App {
        let mut app = app_with_chats(1);
        app.set_backend_commands(commands.iter().map(|c| (*c).to_owned()).collect());
        submit_text(&mut app, "in flight");
        app
    }

    #[test]
    fn ctrl_l_busy_opens_stop_confirm_and_idle_is_noop() {
        // Busy chat: ^L opens the stop confirmation.
        let mut app = busy_app_with_commands(&["/stop", "/reset"]);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Stop
            }
        ));

        // Idle chat: silent no-op (retired wipe semantics stay retired).
        let mut app = app_with_chats(1);
        app.set_backend_commands(vec!["/stop".to_owned()]);
        type_in(&mut app, "draft survives");
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(app.mode.is_normal(), "idle ^L must not open the popup");
        assert_eq!(app.input.lines().join(""), "draft survives");
        assert!(app.take_control_submission().is_none());
    }

    #[test]
    fn ctrl_l_without_backend_support_shows_toast() {
        let mut app = busy_app_with_commands(&["/reset"]);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(
            app.mode.is_normal(),
            "no popup without the advertised command"
        );
        assert!(app.status_message.is_some(), "transient toast explains");
    }

    #[test]
    fn shift_ctrl_l_opens_reset_confirm_in_both_kitty_variants() {
        // Kitty protocol: shifted chord arrives as Char('L') + CONTROL.
        let mut app = app_with_chats(1);
        app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);
        press(
            &mut app,
            KeyCode::Char('L'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Reset
            }
        ));

        // Variant where the unshifted char rides with the SHIFT flag.
        let mut app = app_with_chats(1);
        app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);
        press(
            &mut app,
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Reset
            }
        ));

        // Plain ^L stays STOP even while the reset capability exists.
        let mut app = busy_app_with_commands(&["/stop", "/reset"]);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Stop
            }
        ));
    }

    #[test]
    fn stop_confirm_keys_follow_the_delete_confirm_grammar() {
        let mut app = busy_app_with_commands(&["/stop", "/reset"]);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);

        // Esc cancels; 'n' cancels; Enter confirms; 'y' confirms.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.mode.is_normal());
        assert!(app.take_control_submission().is_none());

        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(app.mode.is_normal());

        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        // Deliberately n/y-free: plain n/y ARE the popup's decision keys.
        type_in(&mut app, "swallowed draft");
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
            "plain typing must not close the popup"
        );
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.mode.is_normal());
        let staged = app
            .take_control_submission()
            .expect("stop staged on confirm");
        assert_eq!(staged.prompt, "/stop");
        assert_eq!(staged.thread_id, app.chats[app.active].id);
        // The chat lifecycle stays on the RUNNING request, not the control.
        assert_ne!(
            app.chats[app.active].lifecycle.request_id(),
            Some(staged.request_id.as_str()),
            "the killed request keeps the chat's lifecycle until its own cancel resolves"
        );
    }

    #[test]
    fn reset_confirm_stages_reset_command_and_cancels_cleanly() {
        let mut app = app_with_chats(1);
        app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);
        press(
            &mut app,
            KeyCode::Char('L'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        let staged = app
            .take_control_submission()
            .expect("reset staged on confirm");
        assert_eq!(staged.prompt, "/reset");
        assert_eq!(staged.thread_id, app.chats[app.active].id);
        assert!(app.mode.is_normal());
    }

    #[test]
    fn same_chord_cancels_both_stop_and_reset_popups() {
        // Stop popup opened by ^L: the same chord closes it, nothing staged.
        let mut app = busy_app_with_commands(&["/stop", "/reset"]);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Stop
            }
        ));
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(app.mode.is_normal(), "same chord must cancel the popup");
        assert!(
            app.take_control_submission().is_none(),
            "cancelled stop must not stage /stop"
        );

        // Reset popup opened by Shift+Ctrl+L (kitty variant): same chord cancels.
        let mut app = busy_app_with_commands(&["/stop", "/reset"]);
        press(
            &mut app,
            KeyCode::Char('L'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Reset
            }
        ));
        press(
            &mut app,
            KeyCode::Char('L'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(app.mode.is_normal(), "same chord must cancel the popup");
        assert!(
            app.take_control_submission().is_none(),
            "cancelled reset must not stage /reset"
        );

        // Variant where the unshifted char rides with SHIFT: same-chord cancel too.
        let mut app = busy_app_with_commands(&["/stop", "/reset"]);
        press(
            &mut app,
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Reset
            }
        ));
        press(
            &mut app,
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(app.mode.is_normal());
        assert!(app.take_control_submission().is_none());
    }

    #[test]
    fn stop_and_reset_are_gated_on_the_handshake_command_list() {
        // Mocks and offline sessions never advertise the commands.
        let mut app = app_with_chats(1);
        press(
            &mut app,
            KeyCode::Char('L'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(app.mode.is_normal(), "no popup on an older backend");
        assert!(app.take_control_submission().is_none());
    }

    // ---- should_submit -------------------------------------------------------

    #[test]
    fn should_submit_enter_returns_true() {
        let enter = key_event(KeyCode::Enter, KeyModifiers::NONE);
        assert!(should_submit(&enter));
    }

    #[test]
    fn should_submit_letter_returns_false() {
        for ch in ['a', 'q', 'j', 'k', 'n', 'x'] {
            let key = key_event(KeyCode::Char(ch), KeyModifiers::NONE);
            assert!(!should_submit(&key), "should_submit({ch:?}) must be false");
        }
    }

    #[test]
    fn should_submit_ctrl_c_returns_false() {
        let ctrl_c = key_event(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!should_submit(&ctrl_c));
    }

    #[test]
    fn should_submit_ctrl_n_returns_false() {
        let ctrl_n = key_event(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(!should_submit(&ctrl_n));
    }

    #[test]
    fn should_submit_arrow_keys_return_false() {
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right] {
            let key = key_event(code, KeyModifiers::NONE);
            assert!(
                !should_submit(&key),
                "should_submit({code:?}) must be false"
            );
        }
    }

    /// only a BARE Enter submits. Shift+Enter and
    /// Alt+Enter must route into the textarea as newline inserts instead.
    #[test]
    fn should_submit_shift_and_alt_enter_return_false() {
        for mods in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let key = key_event(KeyCode::Enter, mods);
            assert!(
                !should_submit(&key),
                "should_submit(Enter+{mods:?}) must be false — it inserts a newline"
            );
        }
    }

    /// Enter+SHIFT / Enter+ALT reach the textarea's
    /// newline insertion through the full `handle_key` path (bare Enter stays
    /// swallowed — submission belongs to the event loop).
    #[test]
    fn shift_and_alt_enter_insert_newline_into_input() {
        for mods in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut app = app_with_chats(1);
            type_in(&mut app, "line one");
            press(&mut app, KeyCode::Enter, mods);
            press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
            press(&mut app, KeyCode::Enter, mods);
            type_in(&mut app, "two");

            // "line one" ⏎ "l" ⏎ "two" — three buffer lines.
            assert_eq!(app.input.lines(), ["line one", "l", "two"]);
        }
    }

    /// Regression guard: bare Enter is still swallowed by `handle_key` (no
    /// newline inserted) and remains the ONLY submitting variant.
    #[test]
    fn bare_enter_is_swallowed_by_handle_key() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "draft");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.input.lines(), ["draft"], "bare Enter must not insert");

        // And via should_submit it would submit (loop-level gate):
        assert!(should_submit(&key_event(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
    }

    /// Rename-mode interplay with Shift+Enter (chosen approach,
    /// documented in README): while renaming, Shift+Enter / Alt+Enter insert
    /// a literal newline into the rename draft; BARE Enter still saves.
    /// The loop's `enter_consumed_by_rename` gate keeps every Enter away
    /// from message submission either way.
    #[test]
    fn rename_mode_shift_enter_inserts_newline_bare_enter_saves() {
        let mut app = app_with_chats(1);
        app.begin_rename();
        assert!(matches!(app.mode, Mode::Renaming { .. }));

        // begin_rename() prefills the draft with the current title.
        type_in(&mut app, "multi");
        press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_in(&mut app, "line");

        match &app.mode {
            Mode::Renaming { buf } => {
                assert_eq!(buf, "chat-0multi\nline", "Shift+Enter inserts \\n")
            }
            other => panic!("rename mode dropped by Shift+Enter: {other:?}"),
        }

        // Bare Enter saves; the trimmed multi-line title persists.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.mode.is_normal());
        assert_eq!(app.chat_title(), "chat-0multi\nline");
    }

    /// A rejected save (whitespace-only title) keeps the old name even when
    /// the draft contained newlines — trimming still wins over `\n`s.
    #[test]
    fn rename_mode_whitespace_only_multiline_draft_rejected() {
        let mut app = app_with_chats(1);
        app.begin_rename();
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT); // "\n"
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert!(app.mode.is_normal(), "save attempted");
        assert_eq!(app.chat_title(), "chat-0", "empty multiline draft rejected");
    }

    // Single-letter commands (q/j/k/N) were removed — they conflicted with the
    // first character of typed words. Navigation uses ↑/↓; new chat uses Ctrl+N;
    // quit uses Ctrl+C only.

    // ---- error popup key capture --------------------------------------------

    #[test]
    fn popup_r_requests_reconnect() {
        let mut app = app_with_chats(1);
        app.show_error("broken pipe");
        press(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);

        assert_eq!(app.reconnect_requested, Some(ReconnectRequest {}));
        assert!(
            app.error_popup.is_some(),
            "popup stays until reconnect resolves"
        );
        assert!(!app.should_quit);
    }

    #[test]
    fn popup_esc_dismisses_but_stays_alive() {
        let mut app = app_with_chats(1);
        app.show_error("boom");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(
            app.error_popup.is_none(),
            "Esc must dismiss the error popup"
        );
        assert!(!app.should_quit, "Esc must not quit from the popup");
    }

    #[test]
    fn popup_q_and_ctrl_c_quit() {
        for (code, mods) in [
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut app = app_with_chats(1);
            app.show_error("boom");
            press(&mut app, code, mods);
            assert!(app.should_quit, "{code:?} must quit from the popup");
        }
    }

    #[test]
    fn popup_other_keys_dismiss_without_quitting() {
        let mut app = app_with_chats(1);
        app.show_error("boom");
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(app.error_popup.is_none(), "dismissed");
        assert!(!app.should_quit, "dismiss keeps the app alive");
        assert!(app.reconnect_requested.is_none());
    }

    #[test]
    fn popup_captures_navigation_and_typing() {
        let mut app = app_with_chats(2);
        app.show_error("boom");

        // Any key other than R/Esc/q/Ctrl+C dismisses without side effects:
        // no chat switch happens even though Down normally navigates.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "chat navigation blocked by popup");
        assert!(app.error_popup.is_none(), "Down dismissed the popup");
        assert!(!app.should_quit);

        // Ctrl+↑/↓ (thread switching in Normal mode)
        // are just another dismiss key under the popup — no thread change.
        app.show_error("boom again");
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0, "Ctrl+Down must not switch chats via popup");
        assert!(app.error_popup.is_none(), "Ctrl+Down dismissed the popup");
        app.show_error("boom thrice");
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0, "Ctrl+Up must not switch chats via popup");

        // the Alt synonym is swallowed identically.
        app.show_error("boom quater");
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(app.active, 0, "Alt+Down must not switch chats via popup");
        assert!(app.error_popup.is_none(), "Alt+Down dismissed the popup");
        app.show_error("boom quinquies");
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.active, 0, "Alt+Up must not switch chats via popup");
    }

    // ---- extended input keybindings ------------------------------------------

    #[test]
    fn ctrl_l_idle_neither_clears_input_nor_stages_anything() {
        let mut app = app_with_chats(1);
        for ch in "hello".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(app.input.lines().join(""), "hello");
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(
            app.input.lines().join("") == "hello",
            "idle ^L is a silent no-op: no input clear, no popup, no request"
        );
        assert!(app.take_control_submission().is_none());
    }

    #[test]
    fn ctrl_v_pastes_clipboard_into_input() {
        let mut app = app_with_chats(1);
        // Only meaningful when a clipboard exists; either way it must not
        // panic or corrupt state.
        press(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        if chibi_tui::clipboard::get_text().is_some() {
            let pasted = app.input.lines().join("");
            println!("clipboard content length: {}", pasted.len());
        }
    }

    #[test]
    fn ctrl_u_deletes_to_line_start_like_readline() {
        let mut app = app_with_chats(1);
        for ch in "keepme".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        // Cursor sits after the last char; Ctrl+U wipes to start of line.
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(
            app.input.lines().iter().all(|l| l.is_empty()),
            "Ctrl+U clears the line (readline semantics, not undo)"
        );
    }

    #[test]
    fn ctrl_a_and_ctrl_e_reach_textarea_readline_mappings() {
        let mut app = app_with_chats(1);
        for ch in "abc".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        // Ctrl+A → head of line.
        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(app.input.cursor(), (0, 0), "Ctrl+A moves to line head");
        // Ctrl+E → end of line.
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(app.input.cursor(), (0, 3), "Ctrl+E moves to line end");
    }

    /// Release events are ignored everywhere (macOS emits them).
    #[test]
    fn release_events_are_ignored() {
        let mut app = app_with_chats(1);
        let release = crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            crossterm::event::KeyEventKind::Release,
        );
        handle_key(&mut app, release);
        assert!(!app.should_quit);
    }

    // ---- per-thread async: submission routing ------------------------------

    /// send_submitted routes a bundle to the mock source; LivePlaceholder is
    /// rejected gracefully with an error event instead of panicking.
    #[tokio::test]
    async fn send_submitted_placeholder_emits_error_not_panic() {
        let submitted = chibi_tui::app::Submitted {
            request_id: "r-1".to_owned(),
            thread_id: "t-1".to_owned(),
            prompt: "hello".to_owned(),
        };
        let (tx, mut rx) = mpsc::channel(4);
        let mut placeholder = Source::LivePlaceholder;
        send_submitted(&mut placeholder, &submitted, tx);
        match rx.recv().await.expect("error event") {
            BackendEvent::Error {
                message, thread_id, ..
            } => {
                assert!(message.contains("offline"), "{message}");
                assert_eq!(thread_id.as_deref(), Some("t-1"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_submitted_mock_receives_prompt() {
        let submitted = chibi_tui::app::Submitted {
            request_id: "r-2".to_owned(),
            thread_id: "t-2".to_owned(),
            prompt: "mock me".to_owned(),
        };
        let (tx, mut rx) = mpsc::channel(4);
        let mut mock_source = Source::Mock(Box::new(chibi_tui::backend::MockBackend::new()));
        send_submitted(&mut mock_source, &submitted, tx);
        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("in time")
            .expect("event");
        assert!(matches!(evt, BackendEvent::Queued { .. }));
    }

    // key routing -----------------------------------

    #[test]
    fn ctrl_r_opens_rename_session() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(
            app.mode,
            chibi_tui::app::Mode::Renaming {
                buf: "chat-0".to_owned()
            }
        );
    }

    /// While renaming, plain keystrokes must land in the DRAFT, never in the
    /// prompt textarea (key routing checks rename mode BEFORE normal input).
    #[test]
    fn keystrokes_go_to_draft_not_input_while_renaming() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "precious prompt");

        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        for ch in "X".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }

        assert_eq!(app.rename_buf(), Some("chat-0X"));
        assert_eq!(
            app.input.lines().join(""),
            "precious prompt",
            "prompt textarea untouched while renaming"
        );
    }

    /// Enter inside rename mode saves and must NEVER submit a message — even
    /// when the draft is rejected (empty), nothing leaks into the chat.
    #[test]
    fn enter_while_renaming_saves_and_never_submits() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        // Wipe the prefilled draft to make the save REJECTED (empty name).
        while app.rename_buf().is_some_and(|b| !b.is_empty()) {
            app.rename_backspace();
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.mode, chibi_tui::app::Mode::Normal, "session closed");
        assert_eq!(app.chats[0].name, "chat-0", "rejected: old name kept");
        assert!(
            app.chats[0].messages.is_empty(),
            "no message bubble leaked from the rename Enter"
        );
        assert!(app.take_input().is_none(), "input buffer still empty");
    }

    #[test]
    fn esc_while_renaming_cancels_and_restores_input_state() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "draft message");

        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        for ch in "junk".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(app.chats[0].name, "chat-0", "rename discarded");
        assert_eq!(
            app.input.lines().join(""),
            "draft message",
            "normal input state restored verbatim"
        );
    }

    /// Chat navigation is blocked mid-rename in ALL modifier flavors so the
    /// active chat cannot silently change under the open editor
    /// (plain ↑/↓ AND Ctrl+↑/↓).
    #[test]
    fn arrows_do_not_switch_chats_while_renaming() {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);

        // Plain arrows.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "navigation suppressed during rename");
        assert!(!matches!(app.mode, chibi_tui::app::Mode::Normal));

        // the new thread-switch bindings must not leak
        // into rename mode either.
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0, "Ctrl+arrows suppressed during rename");
        assert!(matches!(app.mode, Mode::Renaming { .. }), "session intact");

        // the Alt synonym is blocked mid-rename too.
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.active, 0, "Alt+arrows suppressed during rename");
        assert!(matches!(app.mode, Mode::Renaming { .. }), "session intact");
    }

    /// Ctrl+C keeps its global meaning in rename mode: with an open session
    /// it first cancels the draft; only a second Ctrl+C acts on the request.
    #[test]
    fn ctrl_c_while_renaming_cancels_draft_first() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "in flight");
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);

        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(
            app.pending_cancel.is_none(),
            "first Ctrl+C drops the draft, not the request"
        );
        assert_eq!(
            app.mode,
            chibi_tui::app::Mode::Normal,
            "session closed by Ctrl+C"
        );
        assert!(app.chats[0].lifecycle.request_id().is_some());

        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        let (request_id, _) = app.pending_cancel.expect("second Ctrl+C cancels request");
        assert_eq!(request_id, submitted.request_id);
    }

    /// The error popup captures keys BEFORE rename mode can be entered:
    /// pressing Ctrl+R while a popup is open routes to popup handling.
    #[test]
    fn popup_blocks_ctrl_r_rename_entry() {
        let mut app = app_with_chats(1);
        app.show_error("boom");
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);

        assert!(
            matches!(app.mode, chibi_tui::app::Mode::Normal),
            "Ctrl+R must not open rename mode through an error popup"
        );
    }

    /// Ctrl+R is consumed as the hotkey in Normal mode — it must not insert
    /// anything into the prompt buffer (regression guard for routing order).
    #[test]
    fn ctrl_r_in_normal_mode_never_touches_input_buffer() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        app.cancel_rename();
        assert!(
            app.input.lines().iter().all(|l| l.is_empty()),
            "Ctrl+R itself must leave the prompt empty"
        );
    }

    // thread switching + caret movement ------------

    /// Ctrl+↑ / Ctrl+↓ switch the active thread with EXACTLY the old plain-
    /// arrow semantics: bounds-clamped selection move + chat-scroll reset.
    /// (The sidebar dot/focus refresh derives from `active` at draw time.)
    #[test]
    fn ctrl_up_and_ctrl_down_switch_active_thread_like_plain_arrows_did() {
        let mut app = app_with_chats(3);

        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.active, 2);
        // Bounds clamp at the list end — no wrap-around.
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.active, 2);

        // Scroll-reset semantics: stored chat scroll clears on any switch.
        app.scroll = 9;
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(app.active, 1);
        assert_eq!(app.scroll, 0, "thread switch resets chat scroll");

        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0);
        // Bounds clamp at the top — saturating, no panic.
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0);
    }

    /// Alt+↑ / Alt+↓ are a FULL SYNONYM of Ctrl+↑/↓ —
    /// identical semantics in both directions, bounds-clamped at the list
    /// edges, chat-scroll reset on every switch. (macOS Mission Control
    /// hijacks Ctrl+arrows system-wide, so this is the stock-macOS path.)
    #[test]
    fn alt_up_and_alt_down_switch_active_thread_like_ctrl_arrows() {
        let mut app = app_with_chats(3);

        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(app.active, 2);
        // Bounds clamp at the list end — no wrap-around.
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(app.active, 2);

        // Scroll-reset semantics: stored chat scroll clears on any switch.
        app.scroll = 9;
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.active, 1);
        assert_eq!(app.scroll, 0, "thread switch resets chat scroll");

        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.active, 0);
        // Bounds clamp at the top — saturating, no panic.
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.active, 0);
    }

    /// Alt-synonym parity: Alt+arrows work with a live multi-line
    /// draft and never disturb it — buffer verbatim, nothing submitted/
    /// queued/in-flight. And plain ↑/↓ STILL move only the caret (regression:
    /// the alt synonym must not leak into the plain-arrow path).
    #[test]
    fn alt_arrows_switch_threads_without_disturbing_multiline_draft() {
        let mut app = app_with_chats(2);
        type_in(&mut app, "line one");
        press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_in(&mut app, "line two");
        let draft_before = app.input.lines().to_vec();
        assert_eq!(draft_before, ["line one", "line two"]);

        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(app.active, 1, "Alt+Down switched threads");
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.active, 0, "Alt+Up switched back");

        assert_eq!(app.input.lines(), draft_before, "draft untouched");
        assert!(app.chats.iter().all(|c| c.messages.is_empty()));
        assert!(app.active_request_id().is_none());
        assert_eq!(app.active_queue_len(), 0);

        // Plain arrows remain caret-only after alt navigation — the caret
        // still sits at the end of row 1 (the draft is 2 lines tall).
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "plain Down must not switch threads");
        assert_eq!(
            app.input.cursor(),
            (1, 8),
            "plain Down must stay caret-level (clamped on the last row)"
        );
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "plain Up must not switch threads");
        assert_eq!(
            app.input.cursor(),
            (0, 8),
            "plain Up must move the caret to the previous row"
        );
    }

    /// Ctrl+arrows work with a live multi-line draft and never disturb it:
    /// buffer verbatim, nothing submitted/queued/in-flight.
    #[test]
    fn ctrl_arrows_switch_threads_without_disturbing_multiline_draft() {
        let mut app = app_with_chats(2);
        type_in(&mut app, "line one");
        press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_in(&mut app, "line two");
        let draft_before = app.input.lines().to_vec();
        assert_eq!(draft_before, ["line one", "line two"]);

        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.active, 1, "Ctrl+Down switched threads");
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0, "Ctrl+Up switched back");

        assert_eq!(app.input.lines(), draft_before, "draft untouched");
        assert!(app.chats.iter().all(|c| c.messages.is_empty()));
        assert!(app.active_request_id().is_none());
        assert_eq!(app.active_queue_len(), 0);
    }

    /// Simulated Windows Terminal / partial-kitty event shapes: terminals
    /// with incomplete modifier reporting may deliver the Ctrl+↑/↓ chords
    /// with EXTRA modifiers riding along (SHIFT when the terminal reports
    /// the raw shift state, ALT when both chord flavors are registered).
    /// The thread-switch arms match CONTROL with `KeyModifiers::contains`,
    /// so every superset shape must still switch threads. (The plain legacy
    /// shapes — bare CONTROL on Up/Down, the `ESC[1;5A`/`ESC[1;5B`
    /// encodings — are pinned by the ctrl-arrows tests above; both encodings
    /// decode into the very same `KeyEvent`.)
    #[test]
    fn ctrl_arrow_shapes_with_extra_riding_modifiers_still_switch_threads() {
        for mods in [
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT,
        ] {
            let mut app = app_with_chats(3);
            press(&mut app, KeyCode::Down, mods);
            assert_eq!(app.active, 1, "Down + {mods:?} must switch threads");
            assert_eq!(
                app.scroll, 0,
                "Down + {mods:?} keeps the scroll-reset semantics"
            );
            app.scroll = 7;
            press(&mut app, KeyCode::Up, mods);
            assert_eq!(app.active, 0, "Up + {mods:?} must switch threads back");
            assert_eq!(app.scroll, 0, "Up + {mods:?} resets the chat scroll");
        }
    }

    /// The Alt synonym under the same partial-kitty shapes: SHIFT riding on
    /// Alt+↑/↓ must not break the thread switch (kitty-capable terminals
    /// report the shift state on the alt flavor too).
    #[test]
    fn alt_arrow_shapes_with_shift_riding_along_still_switch_threads() {
        for mods in [
            KeyModifiers::ALT | KeyModifiers::SHIFT,
            KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        ] {
            let mut app = app_with_chats(3);
            press(&mut app, KeyCode::Down, mods);
            assert_eq!(app.active, 1, "Alt-flavored Down + {mods:?} switches");
            press(&mut app, KeyCode::Up, mods);
            assert_eq!(app.active, 0, "Alt-flavored Up + {mods:?} switches back");
        }
    }

    /// Startup/teardown pairing contract: the kitty enhancement flags are
    /// popped by the `TerminalRestore` guard exactly when they were pushed,
    /// so the terminal's kitty flag stack can never leak into the user's
    /// shell after exit (nor under-pop an outer entry). Bracketed paste is
    /// disabled by the same guard exactly when it was enabled — a terminal
    /// MODE, not a stack, so the guard must never toggle it blind.
    #[test]
    fn terminal_restore_pops_the_kitty_flags_exactly_when_they_were_pushed() {
        let restore = TerminalRestore::new(push_kitty_flags(), !cfg!(windows));
        assert_eq!(
            restore.pop_kitty_flags,
            push_kitty_flags(),
            "the guard's pop decision must mirror the push decision"
        );
        assert_eq!(
            restore.disable_bracketed_paste,
            !cfg!(windows),
            "the guard's disable decision must mirror the enable decision"
        );
        // The guard built from the real startup decision is internally
        // consistent by construction; the false/true branches are covered
        // by the mirror assertion above (a mismatch would mean either a
        // leaked flag stack or an under-pop on some platform).
    }

    /// A bracketed multi-line paste inserts ALL lines into the draft as hard
    /// newlines and NEVER submits — the paste event carries no key, so bare
    /// Enter (the only submit path) can never fire from a paste. Regression
    /// for the hand-tested bug: a three-line paste used to fire three
    /// submits because the terminal delivered the paste as raw keystrokes
    /// (no bracketed-paste mode) and each embedded newline decoded into a
    /// submitting Enter.
    #[test]
    fn bracketed_multiline_paste_inserts_lines_without_submitting() {
        let mut app = app_with_chats(1);
        handle_paste(&mut app, "line one\nline two\nline three");
        assert_eq!(
            app.input.lines(),
            ["line one", "line two", "line three"],
            "the whole paste lands in the draft, newlines intact"
        );
        assert_eq!(app.input.cursor(), (2, 10));
        // No submit happened: the draft is still there for the user to send.
        assert!(app.take_input().is_some());
    }

    /// Windows (`\r\n`) and classic-Mac (`\r`) line endings normalize to
    /// `\n` on paste — a foreign clipboard never leaves stray `\r` chars in
    /// the draft.
    #[test]
    fn paste_normalizes_crlf_and_bare_cr_to_newlines() {
        let mut app = app_with_chats(1);
        handle_paste(&mut app, "alpha\r\nbeta\rgamma");
        assert_eq!(app.input.lines(), ["alpha", "beta", "gamma"]);
    }

    /// Pasting into a non-empty draft splits lines at the caret: the tail of
    /// the caret's line wraps to the last pasted line, exactly like
    /// tui-textarea 0.7's `insert_str`.
    #[test]
    fn paste_mid_text_splits_lines_at_caret() {
        let mut app = app_with_chats(1);
        for ch in "hello".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        // Walk the caret back into the middle of "hello" (between 'l' and 'o').
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        handle_paste(&mut app, "X\nY");
        assert_eq!(app.input.lines(), ["hellX", "Yo"]);
        assert_eq!(app.input.cursor(), (1, 1));
    }

    /// Paste is editor-scoped: while the sidebar owns the keyboard, a
    /// bracketed paste must be swallowed — it can never fill a draft the
    /// sidebar's own Enter would submit.
    #[test]
    fn paste_into_sidebar_focus_is_swallowed() {
        let mut app = app_with_chats(1);
        app.focus = Focus::Sidebar;
        handle_paste(&mut app, "should not land\nanywhere");
        assert!(app.input.lines().iter().all(|l| l.is_empty()));
    }

    /// Plain ↑/↓ move the text cursor vertically inside the editor — never
    /// switching threads. Column preserved across rows (readline-native
    /// mapping), clamped at the first/last row. This exercises the grown
    /// (MAX_INPUT_LINES-capped) block's caret navigation path.
    #[test]
    fn plain_vertical_arrows_move_caret_not_thread() {
        let mut app = app_with_chats(3);
        type_in(&mut app, "one");
        press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT); // caret → row 1
        type_in(&mut app, "two");
        assert_eq!(app.input.cursor(), (1, 3));
        assert_eq!(app.input.lines(), ["one", "two"]);

        // Down on the LAST row: clamped no-op; absolutely no thread change.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active, 0);
        assert_eq!(app.input.cursor(), (1, 3));

        // Up moves to the previous row, column preserved.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "plain Up must not switch threads");
        assert_eq!(app.input.cursor(), (0, 3));

        // Up on the FIRST row: clamped no-op.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0);
        assert_eq!(app.input.cursor(), (0, 3));

        // Down returns the caret to where it was.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.input.cursor(), (1, 3));
        assert_eq!(app.active, 0);
    }

    /// Required regression guard: in a SINGLE-LINE input, plain ↑ / ↓ neither
    /// change the active thread nor leak into the submission machinery —
    /// buffer stays verbatim, no request begins/cancels/quits, and the
    /// loop-level gate still rejects every arrow variant.
    #[test]
    fn plain_arrows_single_line_no_thread_change_and_no_submit() {
        let mut app = app_with_chats(2);
        type_in(&mut app, "abc");
        assert_eq!(app.input.cursor(), (0, 3));

        for code in [KeyCode::Up, KeyCode::Down] {
            press(&mut app, code, KeyModifiers::NONE);
            assert_eq!(app.active, 0, "{code:?} must not switch threads");
            assert_eq!(app.input.lines(), ["abc"], "{code:?} must not edit");
            assert!(
                app.active_request_id().is_none(),
                "{code:?} must not submit"
            );
            assert!(app.pending_cancel.is_none());
            assert!(!app.should_quit);
        }
        // And the event-loop gate agrees: arrows are never submit keys.
        for code in [KeyCode::Up, KeyCode::Down] {
            assert!(!should_submit(&key_event(code, KeyModifiers::NONE)));
        }
    }

    // key routing --------------------------------------

    /// THE round-trip criterion through the full key path: Ctrl+T toggles
    /// pane FOCUS — Chat → Sidebar → Chat. It must not move the selection
    /// itself (the old wrap-cycling semantics were rejected outright).
    #[test]
    fn ctrl_t_toggles_pane_focus_and_round_trips() {
        let mut app = app_with_chats(3);
        assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "default focus");

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(
            app.focus,
            chibi_tui::app::Focus::Chat,
            "Ctrl+T twice round-trips"
        );

        // Pure focus flip: no navigation happened.
        assert_eq!(app.active, 0);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(app.chats.len(), 3);

        // Arrows remain clamped in Normal mode (regression for the removed
        // wrap semantics — clamping never depended on Ctrl+T).
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.active, 1);
        app.active = 2;
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.active, 2, "Ctrl+Down still clamps at the last thread");
    }

    /// Sidebar-focused arrows navigate the selection with LIVE active-chat
    /// switching and CLAMP at the edges; each switch resets the chat scroll
    /// to follow-bottom (same mechanics as today's normal-mode arrows).
    #[test]
    fn sidebar_arrows_navigate_live_switch_and_clamp() {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

        app.scroll = 42; // detach from bottom before switching
        assert!(!app.at_bottom());

        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active, 1, "sidebar ↓ selects the next chat");
        assert_eq!(app.scroll, 0, "chat view reset to follow-bottom");

        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active, 2);
        // Clamp at the last edge…
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active, 2, "↓ clamps at the last thread");
        // …and back up to the first.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "↑ clamps at the first thread");
    }

    /// Bare Enter on a Sidebar-focused UI applies and returns focus to Chat
    /// WITHOUT submitting the draft or disturbing it (the event loop's
    /// `enter_consumed_by_sidebar` gate suppresses submission for exactly
    /// this key — handle_key must leave no side effects behind).
    #[test]
    fn sidebar_enter_returns_focus_without_submitting() {
        let mut app = app_with_chats(3);
        type_in(&mut app, "half typed");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
        assert_eq!(app.input.lines(), ["half typed"], "draft untouched");
        assert!(app.chats.iter().all(|c| c.messages.is_empty()), "no submit");
        assert!(app.active_request_id().is_none());
        assert_eq!(app.active_queue_len(), 0);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        // Loop-gate parity: bare Enter stays a submit-shaped key; only the
        // was_sidebar_focused snapshot can consume it — covered there.
        assert!(should_submit(&key_event(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
    }

    /// Esc on a Sidebar-focused UI returns focus to Chat WITHOUT clearing
    /// the draft — deliberately different from Normal-mode Esc (which clears
    /// non-empty input).
    #[test]
    fn sidebar_esc_returns_focus_and_preserves_draft() {
        let mut app = app_with_chats(3);
        type_in(&mut app, "keep me");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
        assert_eq!(
            app.input.lines(),
            ["keep me"],
            "Esc must NOT clear the draft"
        );
    }

    /// Typing and text-editing keystrokes are SWALLOWED while the sidebar is
    /// focused — nothing reaches the textarea: plain chars, backspace,
    /// Shift+Enter newline inserts, even Space.
    #[test]
    fn typing_is_swallowed_while_sidebar_focused() {
        let mut app = app_with_chats(3);
        type_in(&mut app, "draft");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT); // newline insert
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT); // alt-newline

        assert_eq!(app.input.lines(), ["draft"], "textarea untouched");
        assert_eq!(
            app.focus,
            chibi_tui::app::Focus::Sidebar,
            "swallowed keys keep the focus"
        );
    }

    /// PgUp/PgDn STILL scroll the CHAT pane while the sidebar holds focus
    /// (documented choice: reading works regardless of focus).
    #[test]
    fn pgup_pgdn_scroll_chat_while_sidebar_focused() {
        let mut app = app_with_chats(2);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(app.scroll, 20, "one page up by visible rows");
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(app.scroll, 0, "back to follow-bottom");
    }

    /// Global service chords stay live with their exact Normal-mode
    /// semantics while the sidebar holds focus: ^F / ^⇧F open searches,
    /// ^D opens the guarded confirm popup, ^L opens the stop confirm popup,
    /// ^R enters rename — and closing any of them hands focus back to Chat.
    #[test]
    fn service_chords_stay_live_while_sidebar_focused() {
        // Each chord gets its own fresh app so lifecycles can't interfere.

        // ^F in-thread search opens and closes back onto Chat.
        let mut app = app_with_chats(1);
        submit_text(&mut app, "hello world");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
        app.search_push('h');
        app.search_push('e');
        app.search_push('l');
        app.search_push('l');
        app.search_push('o');
        assert!(!app.search_matches().is_empty());
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // jump & close
        assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "popup close resets");

        // ^⇧F global search opens from the sidebar too.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::SearchingAll { .. }
        ));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // popup dismisses
        assert!(app.mode.is_normal());
        assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "popup close resets");

        // ^D opens the confirm popup from the sidebar (idle chat).
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
        assert_eq!(app.chats.len(), 1, "nothing deleted yet");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // cancel via modal arm
        assert!(app.mode.is_normal());
        assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "popup close resets");

        // ^L opens the stop confirm while Sidebar focused (idle chat would be
        // a no-op; a busy chat opens the popup — parity with Normal mode).
        let mut app = app_with_chats(1);
        app.set_backend_commands(vec!["/stop".to_owned(), "/reset".to_owned()]);
        submit_text(&mut app, "in flight");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Stop
            }
        ));
        app.cancel_stop_reset();

        // ^R renames from the sidebar; committing returns focus to Chat.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Renaming { .. }));
        app.rename_push('z');
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save via modal arm
        assert!(app.mode.is_normal());
        assert_eq!(app.chat_title(), "chat-0z");
        assert_eq!(
            app.focus,
            chibi_tui::app::Focus::Chat,
            "rename close resets"
        );
    }

    /// ^N new-chat works under Sidebar focus and lands focus on Chat with
    /// the fresh chat selected (editor-bound action).
    #[test]
    fn ctrl_n_from_sidebar_lands_focus_on_chat() {
        let mut app = app_with_chats(2);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);

        assert_eq!(app.focus, chibi_tui::app::Focus::Chat, "^N lands on Chat");
        assert_eq!(app.active, 0, "the new chat is selected (and on top)");
        assert_eq!(app.chats.len(), 3);
        assert!(app.at_bottom(), "scroll reset to follow-bottom");
    }

    /// Busy chats participate normally under Sidebar navigation: switching
    /// away from a running chat is allowed and the
    /// highlighted thread follows the work — nothing in the old cycle path
    /// cared about lifecycles either.
    #[test]
    fn sidebar_navigation_across_busy_chats() {
        let mut app = app_with_chats(2);
        submit_text(&mut app, "in flight"); // chat 0 busy
        assert!(app.is_busy());

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active, 1, "switching away from a busy chat is allowed");
        assert!(!app.is_busy(), "active chat is the idle one");
        assert!(app.any_busy(), "background chat still runs");

        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0);
        assert!(app.is_busy(), "back onto the busy chat");
    }

    /// Single chat: toggling is still a valid pure focus flip — and with the
    /// sidebar focused the arrow navigation is a graceful clamp-noop.
    #[test]
    fn ctrl_t_single_chat_toggles_focus_without_navigating() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
        assert_eq!(app.active, 0);

        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "clamped nav around one chat");
        assert!(app.status_message.is_none(), "no toast spam");
        assert!(app.error_popup.is_none(), "no popup");
        assert!(!app.should_quit);
    }

    /// Zero chats: toggle and navigation stay silent no-ops through the full
    /// key path — no panic anywhere.
    #[test]
    fn ctrl_t_zero_chats_is_silent_noop() {
        let mut app = chibi_tui::app::App::new(Vec::new());
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active, 0);
        assert!(!app.should_quit);
        assert!(app.status_message.is_none());

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
    }

    /// Modal swallowing: with the GLOBAL search popup open, Ctrl+T goes to
    /// the popup (swallowed, popup stays, focus unchanged) — never to the
    /// focus flip or navigation.
    #[test]
    fn ctrl_t_swallowed_by_global_search_popup() {
        let mut app = app_with_chats(3);
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::SearchingAll { .. }
        ));

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        assert!(
            matches!(app.mode, chibi_tui::app::Mode::SearchingAll { .. }),
            "popup must stay open"
        );
        assert_eq!(
            app.focus,
            chibi_tui::app::Focus::Chat,
            "no focus flip through the popup"
        );
        assert_eq!(app.active, 0, "Ctrl+T must not navigate through the popup");
    }

    /// Modal swallowing: the delete-confirm popup swallows Ctrl+T too.
    #[test]
    fn ctrl_t_swallowed_by_delete_confirm_popup() {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
        assert_eq!(app.active, 0, "no navigation through the popup");
        assert_eq!(app.chats.len(), 3, "nothing deleted");
    }

    /// Modal swallowing: the in-thread search popup swallows Ctrl+T too.
    #[test]
    fn ctrl_t_swallowed_by_in_thread_search_popup() {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
        assert_eq!(app.active, 0, "no navigation through the popup");
    }

    /// Renaming blocks Ctrl+T like every other chord: its branch takes
    /// precedence (the rename branch returns before the focus branch runs),
    /// so the chord never flips focus while the rename editor is open.
    /// The letter lands in the draft instead — the rename arm pushes ANY
    /// `Char` regardless of modifiers, exactly like Ctrl+N pushes 'n' there
    /// (documented branch behavior, unchanged by this feature).
    #[test]
    fn ctrl_t_blocked_while_renaming() {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Renaming { .. }));

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

        assert!(
            matches!(app.mode, chibi_tui::app::Mode::Renaming { .. }),
            "rename session must stay open"
        );
        assert_eq!(app.active, 0, "no thread switch mid-rename");
        assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
        // The rename branch consumed the chord: the letter went into the
        // draft (the arm accepts any Char), never into navigation.
        assert_eq!(
            app.rename_buf(),
            Some("chat-0t"),
            "rename branch precedence: 't' lands in the draft"
        );
    }

    /// Ctrl+T works with a live multi-line draft and never disturbs it:
    /// buffer verbatim, nothing submitted/queued/in-flight.
    #[test]
    fn ctrl_t_switches_focus_without_disturbing_multiline_draft() {
        let mut app = app_with_chats(2);
        type_in(&mut app, "line one");
        press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_in(&mut app, "line two");
        let draft_before = app.input.lines().to_vec();
        assert_eq!(draft_before, ["line one", "line two"]);

        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar, "focus flipped");

        assert_eq!(app.input.lines(), draft_before, "draft untouched");
        assert!(app.chats.iter().all(|c| c.messages.is_empty()));
        assert!(app.active_request_id().is_none());
        assert_eq!(app.active_queue_len(), 0);
    }

    // key routing ----------------------------------

    #[test]
    fn ctrl_d_opens_confirm_popup_on_idle_chat() {
        let mut app = app_with_chats(2);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
    }

    #[test]
    fn ctrl_d_on_busy_chat_shows_status_and_never_opens_popup() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "in flight");
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert!(app.status_message.is_some(), "refusal toast shown");
        assert_eq!(app.chats.len(), 1, "nothing deleted");
    }

    /// Enter and `y` confirm through the full key path; both remove the
    /// chat and record the pending file deletion for the event loop.
    #[test]
    fn confirm_popup_enter_and_y_confirm_deletion() {
        for code in [KeyCode::Enter, KeyCode::Char('y')] {
            let mut app = app_with_chats(3);
            press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
            assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
            press(&mut app, code, KeyModifiers::NONE);
            assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
            assert_eq!(app.chats.len(), 2, "{code:?} confirmed deletion");
            assert!(app.pending_delete.is_some(), "{code:?} set the delete");
        }
    }

    /// Esc and `n` cancel through the full key path: nothing is deleted and
    /// no file removal is requested.
    #[test]
    fn confirm_popup_esc_and_n_cancel_deletion() {
        for code in [KeyCode::Esc, KeyCode::Char('n')] {
            let mut app = app_with_chats(3);
            press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
            press(&mut app, code, KeyModifiers::NONE);
            assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
            assert_eq!(app.chats.len(), 3, "{code:?} cancelled, nothing deleted");
            assert!(app.pending_delete.is_none());
            assert_eq!(app.chats[0].name, "chat-0");
        }
    }

    /// The confirm popup swallows every other key: no textarea leakage, no
    /// global bindings (Ctrl+N/R/L/F, arrows nav), no accidental quit via q.
    #[test]
    fn confirm_popup_isolates_keystrokes_and_suspends_global_bindings() {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);

        // Typing must not reach the textarea (letters a/b/c avoid the
        // popup's own y/n confirm-cancel keys).
        type_in(&mut app, "abc");
        assert!(app.input.lines().iter().all(|l| l.is_empty()));
        // Global bindings suspended.
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        // the Alt synonym is swallowed by the popup too.
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.chats.len(), 3, "Ctrl+N must not fire mid-popup");
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete),
            "popup must stay open"
        );
        assert!(
            !matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
            "Ctrl+L must not fire"
        );
        assert!(!app.should_quit);
        // q is deliberately unbound inside the popup — a stray q must not
        // quit while a destructive confirmation is on screen.
        press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.should_quit, "q must not quit from the confirm popup");

        // The popup is still fully functional afterwards.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(app.chats.len(), 3);
    }

    // key routing ----------------------------------

    #[test]
    fn ctrl_f_opens_search_popup() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
        assert_eq!(app.search_query(), Some(""));
    }

    /// Typing while the search popup is open must go to the QUERY buffer,
    /// never into the message draft (modal-ish isolation).
    #[test]
    fn search_typing_goes_to_query_not_input() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "precious draft");
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        type_in(&mut app, "needle");

        assert_eq!(app.search_query(), Some("needle"));
        assert_eq!(
            app.input.lines().join(""),
            "precious draft",
            "message draft untouched while searching"
        );
    }

    #[test]
    fn search_up_down_navigate_and_enter_jumps() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::user("needle one"));
        app.chats[0].messages.push(Message::assistant("needle two"));
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        type_in(&mut app, "needle");
        assert_eq!(app.search_matches().len(), 2);
        assert_eq!(app.search_selected(), 0);

        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.search_selected(), 1);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.search_selected(), 0);

        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal, "Enter closes popup");
        assert!(app.pending_search_jump.is_some(), "jump recorded");
        assert_eq!(app.chats[0].messages.len(), 2, "messages untouched");
    }

    /// Esc closes the search popup WITHOUT a jump; the chat view stays put.
    #[test]
    fn search_esc_closes_without_jumping() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::user("needle"));
        app.scroll_up(12);
        let scroll_before = app.scroll;
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        type_in(&mut app, "needle");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert!(app.pending_search_jump.is_none(), "Esc must not jump");
        assert_eq!(app.scroll, scroll_before, "view unchanged by Esc");
    }

    /// PgUp/PgDn are disabled inside the search popup — the chat scroll is
    /// driven by the jump, never by page keys.
    #[test]
    fn search_popup_swallows_pgup_pgdn_and_arrows() {
        let mut app = app_with_chats(2);
        app.scroll_up(30);
        let scroll_before = app.scroll;
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);

        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        // the Alt synonym is swallowed by the popup too.
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE); // navigation, not caret
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        // the Alt synonym is swallowed by the popup too.
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);

        assert!(
            matches!(app.mode, chibi_tui::app::Mode::Searching { .. }),
            "popup must stay open"
        );
        assert_eq!(app.active, 0, "thread switch must not fire");
        assert_eq!(app.chats.len(), 2, "Ctrl+N must not fire");
        assert_eq!(app.scroll, scroll_before, "PgUp/PgDn must not scroll");
        assert!(
            !matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
            "Ctrl+L must not fire"
        );
        assert!(!app.should_quit);
        assert!(app.pending_search_jump.is_none());
    }

    /// Ctrl+C quits from the search popup (same class as the other popups).
    #[test]
    fn search_popup_ctrl_c_quits() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
        assert_eq!(app.chats.len(), 1, "quit must not mutate chats");
    }

    /// The confirm-delete popup and the search popup cannot coexist: Ctrl+F
    /// is swallowed while the delete confirmation is open, and Ctrl+D is
    /// swallowed while searching.
    #[test]
    fn search_and_delete_popups_cannot_coexist() {
        // Ctrl+F over the delete confirm popup: swallowed.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete));
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete),
            "Ctrl+F must not open search over the delete popup"
        );

        // Ctrl+D over the search popup: swallowed.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::Searching { .. }),
            "Ctrl+D must not open delete popup over search"
        );
        assert_eq!(app.chats.len(), 1, "nothing deleted");
    }

    /// The search popup's Enter must never submit the message draft — the
    /// loop-level `enter_consumed_by_search` gate mirrors the delete-popup
    /// handling (asserted via handle_key: mode closes, jump recorded, and
    /// the draft survives untouched; submission is loop-gated identically to
    /// `enter_consumed_by_delete`).
    #[test]
    fn search_enter_never_touches_draft_or_messages() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "draft text");
        app.chats[0].messages.push(Message::user("needle"));
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        type_in(&mut app, "needle");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert!(app.pending_search_jump.is_some());
        assert_eq!(
            app.input.lines().join(""),
            "draft text",
            "draft must survive the search Enter"
        );
        assert_eq!(app.chats[0].messages.len(), 1, "no message appended");
        assert!(app.active_request_id().is_none());
        assert_eq!(app.active_queue_len(), 0);
    }

    /// Search works on a BUSY chat (read-only): opening the popup and
    /// jumping leaves the in-flight request untouched.
    #[test]
    fn search_opens_while_busy_and_leaves_lifecycle_alone() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::user("needle"));
        let submitted = submit_text(&mut app, "in flight");
        assert!(app.is_busy());

        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        type_in(&mut app, "needle");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(
            app.chats[0].lifecycle.request_id(),
            Some(submitted.request_id.as_str()),
            "in-flight request untouched by search"
        );
    }

    /// Ctrl+C inside the confirm popup quits the app (same class as the
    /// error popup) — it must NOT delete the chat.
    #[test]
    fn confirm_popup_ctrl_c_quits_without_deleting() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
        assert_eq!(app.chats.len(), 1, "quit must not delete the chat");
        assert!(app.pending_delete.is_none());
    }

    /// Deleting the last chat reaches the clean empty state through the full
    /// key path; the confirm Enter never submits a pre-typed draft.
    #[test]
    fn confirm_delete_last_chat_reaches_empty_state_via_keys() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "draft text");
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert!(app.chats.is_empty());
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert!(app.pending_delete.is_some());
        assert!(app.active_request_id().is_none());
        // The draft survives the deletion untouched (thread ops never clear
        // the prompt buffer in this app).
        assert_eq!(app.input.lines().join(""), "draft text");
    }

    /// With a neighbour present, the confirm Enter must never submit the
    /// draft to it — the popup owns the Enter.
    #[test]
    fn confirm_enter_never_submits_draft_to_neighbour() {
        let mut app = app_with_chats(2);
        type_in(&mut app, "draft text");
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.chats.len(), 1);
        assert_eq!(app.chats[0].name, "chat-1", "neighbour selected");
        assert!(
            app.chats[0].messages.is_empty(),
            "the draft must not be submitted to the neighbour"
        );
        assert!(app.chats[0].queue.is_empty());
        assert!(app.active_request_id().is_none());
    }

    // key routing ----------------------------

    /// Ctrl+Shift+F opens the GLOBAL search popup (kitty-protocol chord:
    /// Char('f') + CONTROL|SHIFT).
    #[test]
    fn ctrl_shift_f_opens_global_search_popup() {
        let mut app = app_with_chats(2);
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::SearchingAll { .. }
        ));
        assert_eq!(app.search_all_query(), Some(""));
    }

    /// Regression guard: plain Ctrl+F must STILL open the in-thread search —
    /// the new Shift chord must not steal it.
    #[test]
    fn ctrl_f_still_opens_in_thread_search_not_global() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
        assert!(!matches!(
            app.mode,
            chibi_tui::app::Mode::SearchingAll { .. }
        ));
    }

    /// Typing while the global search popup is open must go to the QUERY
    /// buffer, never into the message draft (modal-ish isolation).
    #[test]
    fn global_search_typing_goes_to_query_not_input() {
        let mut app = app_with_chats(2);
        type_in(&mut app, "precious draft");
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        type_in(&mut app, "needle");

        assert_eq!(app.search_all_query(), Some("needle"));
        assert_eq!(
            app.input.lines().join(""),
            "precious draft",
            "message draft untouched while searching"
        );
    }

    #[test]
    fn global_search_up_down_navigate_and_enter_jumps() {
        let mut app = app_with_chats(2);
        app.chats[0].messages.push(Message::user("needle one"));
        app.chats[1].messages.push(Message::assistant("needle two"));
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        type_in(&mut app, "needle");
        assert_eq!(app.search_all_matches().len(), 2);
        assert_eq!(app.search_all_selected(), 0);

        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.search_all_selected(), 1);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.search_all_selected(), 0);

        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal, "Enter closes popup");
        assert_eq!(app.active, 0, "first match belongs to chat 0");
        assert!(app.pending_global_search_jump.is_some(), "jump recorded");
        assert!(
            app.pending_search_jump.is_none(),
            "in-thread jump state untouched"
        );
        assert_eq!(app.chats[0].messages.len(), 1, "messages untouched");
    }

    /// Enter switches the active chat to the match's thread — including a
    /// NON-active one (the Ctrl+↑/↓ selection mechanics).
    #[test]
    fn global_search_enter_switches_to_non_active_thread() {
        let mut app = app_with_chats(3);
        app.chats[2]
            .messages
            .push(Message::user("needle in chat two"));
        app.active = 0;
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        type_in(&mut app, "needle");
        assert_eq!(app.search_all_matches()[0].chat_index, 2);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(app.active, 2, "target thread activated");
        assert_eq!(app.scroll, 0, "thread switch resets chat scroll");
    }

    /// Esc closes the global search popup WITHOUT switching threads or
    /// jumping; the view stays put.
    #[test]
    fn global_search_esc_closes_without_switching_or_jumping() {
        let mut app = app_with_chats(2);
        app.chats[1].messages.push(Message::user("needle"));
        app.active = 0;
        app.scroll_up(12);
        let scroll_before = app.scroll;
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        type_in(&mut app, "needle");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(app.active, 0, "no thread switch on Esc");
        assert_eq!(app.scroll, scroll_before, "view unchanged by Esc");
        assert!(
            app.pending_global_search_jump.is_none(),
            "Esc must not jump"
        );
        assert!(app.pending_search_jump.is_none());
    }

    /// The global search popup swallows every other key: no textarea
    /// leakage, no global bindings (Ctrl+N/R/L/D/F, arrows nav, PgUp/PgDn),
    /// no accidental quit via q.
    #[test]
    fn global_search_popup_swallows_global_bindings() {
        let mut app = app_with_chats(2);
        app.scroll_up(30);
        let scroll_before = app.scroll;
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );

        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        // the Alt synonym is swallowed by the popup too.
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        // Ctrl+F must not open the in-thread search over the global one.
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);

        assert!(
            matches!(app.mode, chibi_tui::app::Mode::SearchingAll { .. }),
            "popup must stay open"
        );
        assert_eq!(app.active, 0, "thread switch must not fire");
        assert_eq!(app.chats.len(), 2, "Ctrl+N must not fire");
        assert_eq!(app.scroll, scroll_before, "PgUp/PgDn must not scroll");
        assert!(
            !matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
            "Ctrl+L must not fire"
        );
        assert!(!app.should_quit);
        assert!(app.pending_global_search_jump.is_none());
    }

    /// Ctrl+C quits from the global search popup (same class as the other
    /// popups).
    #[test]
    fn global_search_popup_ctrl_c_quits() {
        let mut app = app_with_chats(1);
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
        assert_eq!(app.chats.len(), 1, "quit must not mutate chats");
    }

    /// The confirm-delete popup and the global search popup cannot coexist:
    /// Ctrl+Shift+F is swallowed while the delete confirmation is open, and
    /// Ctrl+D is swallowed while searching globally.
    #[test]
    fn global_search_and_delete_popups_cannot_coexist() {
        // Ctrl+Shift+F over the delete confirm popup: swallowed.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete));
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete),
            "Ctrl+Shift+F must not open global search over the delete popup"
        );

        // Ctrl+D over the global search popup: swallowed.
        let mut app = app_with_chats(1);
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::SearchingAll { .. }),
            "Ctrl+D must not open delete popup over global search"
        );
        assert_eq!(app.chats.len(), 1, "nothing deleted");
    }

    /// The global search popup's Enter must never submit the message draft —
    /// the loop-level `enter_consumed_by_search_all` gate mirrors the
    /// in-thread search handling (asserted via handle_key: mode closes, the
    /// target thread activates, jump recorded, draft survives untouched).
    #[test]
    fn global_search_enter_never_touches_draft_or_messages() {
        let mut app = app_with_chats(2);
        type_in(&mut app, "draft text");
        app.chats[1].messages.push(Message::user("needle"));
        press(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        type_in(&mut app, "needle");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.mode, chibi_tui::app::Mode::Normal);
        assert_eq!(app.active, 1, "target thread activated");
        assert!(app.pending_global_search_jump.is_some());
        assert_eq!(
            app.input.lines().join(""),
            "draft text",
            "draft must survive the global search Enter"
        );
        assert_eq!(app.chats[1].messages.len(), 1, "no message appended");
        assert!(app.active_request_id().is_none());
        assert_eq!(app.active_queue_len(), 0);
    }

    // ^G log viewer ------------------------------

    #[test]
    fn ctrl_g_opens_log_viewer_from_normal_mode() {
        let marker = format!(
            "routing-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        chibi_tui::diag::append(&marker);
        let mut app = app_with_chats(1);

        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);

        assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
        let state = match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state,
            other => panic!("expected LogViewer, got {other:?}"),
        };
        assert!(state.at_tail(), "opens live-tailing: cursor on the tail");
        assert!(
            state.lines.iter().any(|e| e.text == *marker),
            "the modal snapshot carries the buffered marker"
        );
    }

    /// Chord-freedom regression: ^G is consumed as the hotkey — it must not
    /// insert anything into the prompt textarea.
    #[test]
    fn ctrl_g_in_normal_mode_never_touches_input_buffer() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        app.close_log_viewer();
        assert!(app.input.lines().iter().all(|l| l.is_empty()));
    }

    #[test]
    fn ctrl_g_opens_log_viewer_from_sidebar_focus_too() {
        let mut app = app_with_chats(2);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);

        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);

        assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
        // Closing returns to the editor pane (modal close semantics).
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
        assert!(app.mode.is_normal());
    }

    #[test]
    fn log_viewer_pgup_pgdn_arrows_and_letters_navigate() {
        let mut app = app_with_chats(1);
        for i in 0..60 {
            chibi_tui::diag::append(format!("filler-{i}"));
        }
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);

        // The diag stream is process-global (other tests append in
        // parallel), so indices are relative to the open-time snapshot.
        let cursor = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.cursor,
            other => panic!("expected LogViewer, got {other:?}"),
        };
        let tail = cursor(&app);
        assert!(tail >= 59, "the 60 filler lines are in the snapshot");

        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(app.log_visible_rows, 20, "page = one modal page");
        assert_eq!(cursor(&app), tail - 20, "PgUp pages up, cursor follows");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(cursor(&app), tail - 21, "↑ steps one line");
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(cursor(&app), tail - 22, "k steps one line too");
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(cursor(&app), tail - 2);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(cursor(&app), tail - 1, "↓ steps one line, still pinned");
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        let at_tail = match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.at_tail(),
            other => panic!("expected LogViewer, got {other:?}"),
        };
        assert!(at_tail, "j lands back on the live tail (re-armed)");

        press(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(cursor(&app), 0, "g jumps to the top");
        press(&mut app, KeyCode::Char('G'), KeyModifiers::NONE);
        let at_tail = match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.at_tail(),
            other => panic!("expected LogViewer, got {other:?}"),
        };
        assert!(at_tail, "G jumps back to the live tail");
    }

    /// `w` toggles wrap from the keyboard; the cursor stays over the same
    /// logical line (full render-level behavior covered in ui.rs).
    #[test]
    fn log_viewer_w_toggles_wrap() {
        let mut app = app_with_chats(1);
        chibi_tui::diag::append("some line");
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        let wrap = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.wrap,
            other => panic!("expected LogViewer, got {other:?}"),
        };
        assert!(!wrap(&app));
        press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
        assert!(wrap(&app), "`w` turns wrap on");
        press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
        assert!(!wrap(&app), "`w` turns wrap back off");
    }

    #[test]
    fn log_viewer_esc_closes_and_keeps_state_intact() {
        let mut app = app_with_chats(2);
        app.scroll_up(7);
        let scroll_before = app.scroll;
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        assert!(app.mode.is_normal(), "Esc closes the viewer");
        assert_eq!(app.scroll, scroll_before, "chat view untouched");
        assert_eq!(app.focus, chibi_tui::app::Focus::Chat);
    }

    /// Modal isolation: while the log viewer is open, typing and every
    /// global binding is swallowed — nothing reaches the textarea, nothing
    /// fires, and the popup stays open.
    #[test]
    fn log_viewer_swallows_typing_and_global_chords() {
        let mut app = app_with_chats(3);
        for i in 0..20 {
            chibi_tui::diag::append(format!("filler-{i}"));
        }
        type_in(&mut app, "draft");
        app.scroll_up(30);
        let scroll_before = app.scroll;
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        let cursor = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.cursor,
            other => panic!("expected LogViewer, got {other:?}"),
        };
        let cursor_before = cursor(&app);

        // Typing must not reach the textarea and must not move the cursor
        // (letters avoid the viewer's own keys, x/y/z are not bound here).
        type_in(&mut app, "xyz");
        assert_eq!(cursor(&app), cursor_before);
        // Global chords suspended: nav, new chat, rename entry, wipe,
        // delete, searches, focus toggle.
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        // Left/Right are not nav here: swallowed too.
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);

        assert!(
            matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }),
            "popup must stay open"
        );
        assert_eq!(app.chats.len(), 3, "Ctrl+N must not fire");
        assert_eq!(app.scroll, scroll_before, "PgUp-style chat scroll blocked");
        assert!(
            !matches!(app.mode, chibi_tui::app::Mode::ConfirmStopReset { .. }),
            "Ctrl+L must not fire"
        );
        assert!(!app.should_quit);
        assert_eq!(
            app.input.lines().join(""),
            "draft",
            "typing must not leak into (nor disturb) the prompt textarea"
        );
        assert_eq!(
            cursor(&app),
            cursor_before,
            "swallowed keys must not move the read position"
        );
    }

    // `/` search, n/N, `y` copy ----------

    /// Hand-built pinned viewer state: hermetic against the global diag
    /// stream that parallel tests append to.
    fn log_viewer_state(
        cursor: usize,
        lines: Vec<&str>,
        search: Option<chibi_tui::app::LogSearch>,
    ) -> chibi_tui::app::LogViewerState {
        chibi_tui::app::LogViewerState {
            cursor,
            wrap: false,
            row_offset: 0,
            lines: lines
                .into_iter()
                .map(|s| chibi_tui::diag::LogEntry::parse(s.to_owned()))
                .collect(),
            snapshot_total: chibi_tui::diag::total_appended(),
            search_buf: None,
            search,
            copy_note: None,
            copy_note_at: None,
        }
    }

    fn viewer_search(app: &chibi_tui::app::App) -> &chibi_tui::app::LogSearch {
        match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => {
                state.search.as_ref().expect("search committed")
            }
            other => panic!("expected LogViewer, got {other:?}"),
        }
    }

    /// `/` opens the prompt, typing fills it, Enter commits; n/N walk the
    /// hits with wraparound on both ends (line-level, both wrap modes use
    /// the same logical lines, so this holds under wrap too).
    #[test]
    fn log_viewer_search_open_commit_and_navigate_wraparound() {
        let mut app = app_with_chats(1);
        app.mode = chibi_tui::app::Mode::LogViewer {
            state: log_viewer_state(
                3,
                vec!["alpha one", "beta ALPHA two", "gamma", "alpha three"],
                None,
            ),
        };

        // Open the prompt and type the pattern.
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        for ch in "alpha".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        let buf = match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => {
                state.search_buf.clone().expect("prompt open")
            }
            other => panic!("expected LogViewer, got {other:?}"),
        };
        assert_eq!(buf, "alpha", "prompt holds the typed pattern");

        // Enter commits: case-insensitive, line-level, cursor jumps to the
        // nearest hit at or after its line (3).
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let search = viewer_search(&app);
        assert_eq!(search.pattern, "alpha");
        assert_eq!(search.matches, vec![0, 1, 3]);
        assert_eq!(search.current, None, "no hit selected before the first n");
        let cursor = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.cursor,
            other => panic!("expected LogViewer, got {other:?}"),
        };
        assert_eq!(cursor(&app), 3);

        // n walks forward and wraps at the tail hit.
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(cursor(&app), 0);
        assert_eq!(viewer_search(&app).current, Some(0));
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(cursor(&app), 1);
        assert_eq!(viewer_search(&app).current, Some(1));
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(cursor(&app), 3);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(cursor(&app), 0, "n wraps around at the end");

        // N steps back (wrapping to the tail hit from the top).
        press(&mut app, KeyCode::Char('N'), KeyModifiers::NONE);
        assert_eq!(cursor(&app), 3);
        assert_eq!(viewer_search(&app).current, Some(2));
    }

    /// Esc on the open prompt cancels it and keeps a previously committed
    /// search intact; Enter on an empty prompt switches the search off.
    #[test]
    fn log_viewer_search_esc_cancels_and_empty_commit_switches_off() {
        let previous = Some(chibi_tui::app::LogSearch {
            pattern: "gamma".to_owned(),
            matches: vec![2],
            current: Some(0),
        });
        let mut app = app_with_chats(1);
        app.mode = chibi_tui::app::Mode::LogViewer {
            state: log_viewer_state(0, vec!["alpha one", "gamma line"], previous.clone()),
        };

        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => {
                assert!(state.search_buf.is_none(), "prompt closed by Esc");
                assert_eq!(&state.search, &previous, "old search untouched");
            }
            other => panic!("expected LogViewer, got {other:?}"),
        }

        // Empty pattern commit = search off.
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => {
                assert!(state.search.is_none(), "empty pattern switches off");
            }
            other => panic!("expected LogViewer, got {other:?}"),
        }
    }

    /// While the search prompt is open it owns the keyboard: nav letters
    /// and global chords land in the buffer, nothing else moves.
    #[test]
    fn log_viewer_search_prompt_swallows_everything_else() {
        let mut app = app_with_chats(1);
        app.mode = chibi_tui::app::Mode::LogViewer {
            state: log_viewer_state(1, vec!["alpha one", "beta two"], None),
        };
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        // Letters that are viewer keys outside the prompt, plus a global
        // chord and arrows: all swallowed, only plain chars type.
        press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('G'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // commit "wnG"
        match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => {
                let search = state.search.as_ref().expect("committed");
                assert_eq!(search.pattern, "wnG", "only plain chars reached the buffer");
                assert!(
                    search.matches.is_empty(),
                    "no line matches, search stays active with zero hits"
                );
                assert_eq!(state.cursor, 1, "cursor never moved while typing");
                assert!(!state.wrap, "w did not toggle wrap inside the prompt");
            }
            other => panic!("expected LogViewer, got {other:?}"),
        }
    }

    /// `y` copies the cursor line and the header gets the brief `copied`
    /// feedback (the OSC 52 bytes themselves are asserted in clipboard.rs;
    /// here the write goes to the test process stdout, which is harmless).
    #[test]
    fn log_viewer_y_sets_copied_feedback() {
        let mut app = app_with_chats(1);
        app.mode = chibi_tui::app::Mode::LogViewer {
            state: log_viewer_state(1, vec!["first", "second line"], None),
        };
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => {
                assert_eq!(state.copy_note.as_deref(), Some("copied"));
                assert!(state.copy_note_at.is_some());
            }
            other => panic!("expected LogViewer, got {other:?}"),
        }
    }

    /// Ctrl+C keeps its popup-class meaning: quit from the log viewer.
    #[test]
    fn log_viewer_ctrl_c_quits() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
        assert_eq!(app.chats.len(), 1, "quit must not mutate chats");
    }

    /// Other modals own ^G: the confirm popup swallows it; the search
    /// popups swallow the Ctrl flavor (plain chars feed the query); the
    /// rename editor consumes any Char into its draft (documented branch
    /// behavior, same as Ctrl+N pushing 'n' there).
    #[test]
    fn ctrl_g_swallowed_by_other_modals() {
        // Delete-confirm popup: swallowed, popup stays.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);

        // In-thread search popup: swallowed, query untouched.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
        assert_eq!(app.search_query(), Some(""));

        // Rename session: 'g' lands in the draft (any-Char branch).
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Renaming { .. }));
        assert_eq!(app.rename_buf(), Some("chat-0g"));
    }

    // ^O toggle ---------------------------------------

    /// ^O toggles the status strip, default visible, round-trip.
    #[test]
    fn ctrl_o_toggles_status_strip_round_trip() {
        let mut app = app_with_chats(1);
        assert!(
            app.status_strip_visible,
            "strip must start visible (task contract)"
        );
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(!app.status_strip_visible);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(app.status_strip_visible);
    }

    /// View state like Focus: the strip survives modal open/close, and the
    /// modal branch swallows ^O while a popup is open (no toggle leaks).
    #[test]
    fn ctrl_o_survives_modals_and_is_swallowed_while_one_is_open() {
        let mut app = app_with_chats(1);
        assert!(app.status_strip_visible);

        // Open the search popup: strip stays visible underneath it.
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
        assert!(app.status_strip_visible);

        // ^O inside the popup is swallowed — mode and visibility unchanged.
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
        assert!(
            app.status_strip_visible,
            "swallowed ^O must not toggle the strip"
        );

        // Closing the modal keeps the visibility flag (no reset on close).
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.mode.is_normal());
        assert!(app.status_strip_visible);
    }

    /// Sidebar-focus parity contract: service chords behave identically
    /// under both panes — ^O toggles the strip from the sidebar too.
    #[test]
    fn ctrl_o_toggles_strip_under_sidebar_focus() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(!app.status_strip_visible);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(app.status_strip_visible);
    }

    // ^S toggle -----------------------------------------

    /// ^S toggles the thoughts block, default ON, round-trip; the toggle is
    /// render-only and never clears the retained reasoning.
    #[test]
    fn ctrl_s_toggles_thoughts_round_trip() {
        let mut app = app_with_chats(1);
        assert!(app.thoughts_visible, "thoughts must start visible (ON)");
        app.chats[0].last_thoughts = Some("chain of thought".into());
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(!app.thoughts_visible);
        assert_eq!(
            app.chats[0].last_thoughts.as_deref(),
            Some("chain of thought"),
            "toggle must not clear the retained thoughts"
        );
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(app.thoughts_visible);
    }

    /// Modal branches swallow ^S like every other chord (no toggle leaks
    /// while a popup is open), and closing keeps the flag.
    #[test]
    fn ctrl_s_survives_modals_and_is_swallowed_while_one_is_open() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(!app.thoughts_visible);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::Searching { .. }));
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(
            !app.thoughts_visible,
            "swallowed ^S must not toggle while a modal is open"
        );
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.mode.is_normal());
        assert!(!app.thoughts_visible, "modal close must keep the flag");
    }

    /// Sidebar-focus parity contract: service chords behave identically
    /// under both panes — ^S toggles thoughts from the sidebar too.
    #[test]
    fn ctrl_s_toggles_thoughts_under_sidebar_focus() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(!app.thoughts_visible);
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(app.thoughts_visible);
    }

    // ^M chord + modal isolation -----------------

    use chibi_tui::app::{ModelPickerPhase, ModelPickerState};
    use chibi_tui::model_picker::ModelEntry;

    /// A Ready picker injected directly (the listing resolution itself is
    /// covered by the app-level tests; here only KEY ROUTING matters).
    fn inject_ready_picker(app: &mut chibi_tui::app::App, rows: usize) {
        let entries = (1..=rows)
            .map(|n| ModelEntry {
                number: n,
                name: format!("model-{n}"),
                provider: Some("prov".to_owned()),
                active: false,
            })
            .collect();
        app.mode = chibi_tui::app::Mode::ModelPicking {
            state: ModelPickerState {
                phase: ModelPickerPhase::Ready,
                entries,
                selected: 0,
            },
        };
    }

    /// Park the injected picker's selection on `selected` (0-based) without
    /// touching the rows (paging tests need non-zero start offsets).
    fn park_picker_selection(app: &mut chibi_tui::app::App, selected: usize) {
        if let chibi_tui::app::Mode::ModelPicking { state } = &mut app.mode {
            state.selected = selected;
        }
    }

    #[test]
    fn ctrl_m_opens_the_model_picker_in_normal_mode() {
        let mut app = app_with_chats(1);
        assert!(app.mode.is_normal());
        press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ModelPicking { .. }
        ));
        // The event loop drains the staged hidden fetch on the same tick.
        let bundle = app.take_picker_submission().expect("fetch staged");
        assert_eq!(bundle.prompt, "/model");
        // Esc hands the keyboard back.
        press(&mut app, KeyCode::Esc, KeyModifiers::empty());
        assert!(app.mode.is_normal());
    }

    #[test]
    fn ctrl_m_works_from_sidebar_focus_and_returns_focus_on_close() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, chibi_tui::app::Focus::Sidebar);
        press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ModelPicking { .. }
        ));
        press(&mut app, KeyCode::Esc, KeyModifiers::empty());
        assert!(app.mode.is_normal());
        assert_eq!(
            app.focus,
            chibi_tui::app::Focus::Chat,
            "modal closed → editor"
        );
    }

    #[test]
    fn the_picker_modal_swallows_everything_but_nav_confirm_cancel() {
        let mut app = app_with_chats(1);
        for ch in "precious draft".chars() {
            app.input.input(chibi_tui::input::Input {
                key: chibi_tui::input::Key::Char(ch),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
        inject_ready_picker(&mut app, 3);

        // Typing leaks nowhere; global chords and thread switching are
        // swallowed; the mode never moves. PgUp is a picker nav key now:
        // it pages the LIST, never the chat pane.
        press(&mut app, KeyCode::Char('x'), KeyModifiers::empty());
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ModelPicking { .. }
        ));
        assert_eq!(app.chats.len(), 1, "no new chat from swallowed ^N");
        assert!(app.error_popup.is_none());
        assert!(
            app.status_strip_visible,
            "swallowed ^O must not toggle the strip"
        );
        assert_eq!(
            app.input.lines().join(""),
            "precious draft",
            "no keystroke leaked into the textarea"
        );
        assert_eq!(
            app.scroll, 0,
            "picker PgUp pages the list, never the chat pane"
        );

        // The ONLY working keys: ↑/↓ and PgUp/PgDn navigate, Enter confirms,
        // Esc cancels, Ctrl+C quits.
        press(&mut app, KeyCode::Down, KeyModifiers::empty());
        press(&mut app, KeyCode::Down, KeyModifiers::empty());
        press(&mut app, KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.model_picker_selected(), 1);
        press(&mut app, KeyCode::Enter, KeyModifiers::empty());
        assert!(app.mode.is_normal(), "Enter confirmed and closed the popup");
        let bundle = app.take_picker_submission().expect("selection staged");
        assert_eq!(bundle.prompt, "/model 2");
    }

    #[test]
    fn the_picker_enters_confirm_never_touch_the_message_draft() {
        let mut app = app_with_chats(1);
        for ch in "half typed prompt".chars() {
            app.input.input(chibi_tui::input::Input {
                key: chibi_tui::input::Key::Char(ch),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
        inject_ready_picker(&mut app, 1);
        press(&mut app, KeyCode::Enter, KeyModifiers::empty());
        assert!(
            app.take_picker_submission().is_some(),
            "the picker consumed the Enter"
        );
        assert_eq!(
            app.input.lines().join(""),
            "half typed prompt",
            "the draft must not be submitted or altered"
        );
        assert!(app.chats[0].messages.is_empty());
    }

    #[test]
    fn the_picker_enter_is_a_noop_while_the_listing_is_loading() {
        let mut app = app_with_chats(1);
        app.begin_model_picker();
        // The event loop would drain the staged fetch on the same tick.
        assert!(app.take_picker_submission().is_some(), "precondition");
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ModelPicking { .. }
        ));
        press(&mut app, KeyCode::Enter, KeyModifiers::empty());
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::ModelPicking { .. }),
            "nothing to confirm while Loading — the popup stays open"
        );
        assert!(app.take_picker_submission().is_none());
    }

    #[test]
    fn the_picker_esc_closes_without_staging_anything() {
        let mut app = app_with_chats(1);
        inject_ready_picker(&mut app, 2);
        press(&mut app, KeyCode::Esc, KeyModifiers::empty());
        assert!(app.mode.is_normal());
        assert!(app.take_picker_submission().is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn the_picker_ctrl_c_quits_like_the_other_popups() {
        let mut app = app_with_chats(1);
        inject_ready_picker(&mut app, 2);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
    }

    #[test]
    fn picker_page_down_jumps_a_full_page_from_the_top() {
        let mut app = app_with_chats(1);
        inject_ready_picker(&mut app, 104);
        app.picker_visible_rows = 12;
        press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(
            app.model_picker_selected(),
            12,
            "one page of visible rows from the top"
        );
        press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(app.model_picker_selected(), 24, "each press steps one page");
        press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
        assert_eq!(
            app.model_picker_selected(),
            12,
            "page up steps back one page"
        );
    }

    #[test]
    fn picker_page_navigation_steps_one_page_from_the_middle() {
        let mut app = app_with_chats(1);
        inject_ready_picker(&mut app, 104);
        app.picker_visible_rows = 12;
        for _ in 0..5 {
            press(&mut app, KeyCode::Down, KeyModifiers::empty());
        }
        assert_eq!(app.model_picker_selected(), 5, "mid-page starting offset");
        press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(
            app.model_picker_selected(),
            17,
            "page down from mid-page steps a full page"
        );
        press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
        assert_eq!(
            app.model_picker_selected(),
            5,
            "page up returns to the row a page above"
        );
    }

    #[test]
    fn picker_page_down_clamps_at_the_bottom_edge() {
        let mut app = app_with_chats(1);
        inject_ready_picker(&mut app, 20);
        app.picker_visible_rows = 12;
        park_picker_selection(&mut app, 15);
        press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(
            app.model_picker_selected(),
            19,
            "clamped to the last row, no wraparound"
        );
        press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(
            app.model_picker_selected(),
            19,
            "paging past the edge stays clamped"
        );
        press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
        assert_eq!(app.model_picker_selected(), 7);
        for _ in 0..2 {
            press(&mut app, KeyCode::PageUp, KeyModifiers::empty());
        }
        assert_eq!(
            app.model_picker_selected(),
            0,
            "page up clamps at the top edge too"
        );
    }

    #[test]
    fn picker_paging_applies_to_the_filtered_entries_list() {
        let mut app = app_with_chats(1);
        inject_ready_picker(&mut app, 104);
        // The picker has no query input today; `entries` IS the navigable
        // list (a future filter would prune it the same way). Paging must
        // measure against this list, never a hardcoded 104-row listing.
        if let chibi_tui::app::Mode::ModelPicking { state } = &mut app.mode {
            state.entries.truncate(15);
        }
        app.picker_visible_rows = 12;
        press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(app.model_picker_selected(), 12);
        press(&mut app, KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(
            app.model_picker_selected(),
            14,
            "clamped to the filtered list's last row"
        );
    }

    #[test]
    fn ctrl_m_is_swallowed_while_other_modals_own_the_keyboard() {
        // ^G log viewer open: ^M must NOT open the picker on top.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
        press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }),
            "the viewer keeps the keyboard"
        );
        press(&mut app, KeyCode::Esc, KeyModifiers::empty());

        // ^D confirm popup open: ^M swallowed as well.
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
        press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);
        press(&mut app, KeyCode::Esc, KeyModifiers::empty());
        assert!(app.mode.is_normal());
        assert!(
            app.take_picker_submission().is_none(),
            "no picker fetch ever staged through the modals"
        );
    }

    // table ↔ dispatch pinning -----------------
    //
    // The modal's content is the const table `chibi_tui::ui::HOTKEY_ROWS`.
    // These tests pin that table against the REAL key handlers from both
    // sides: (1) a dispatch enumeration below must equal the table row by
    // row, and (2) every Normal-mode chord the table advertises must be
    // demonstrably CLAIMED by `handle_key` — pressed on a draft-bearing app,
    // none of them may land in the textarea as plain typing (the `(_, _)`
    // fall-through would insert it). A chord added to the dispatch without
    // a table row fails (1); a table row whose chord the dispatch dropped
    // fails (2).

    /// The dispatch's chord→action map as the help table must render it.
    /// Maintained next to `handle_key`: a new binding updates BOTH this and
    /// `ui::HOTKEY_ROWS` in the same commit, or the suite goes red.
    const DISPATCH_CHORDS: &[(&str, &str, &str)] = &[
        ("Global", "F1", "open / close this keybindings help"),
        (
            "Global",
            "Ctrl+C",
            "cancel the active request · quit when idle",
        ),
        ("Global", "Ctrl+N", "new chat"),
        (
            "Global",
            "Ctrl+P",
            "clone the active thread (needs backend support)",
        ),
        ("Global", "Ctrl+R", "rename the active thread"),
        (
            "Global",
            "Ctrl+D",
            "delete the active thread (confirmation)",
        ),
        ("Global", "Ctrl+F", "find in the active thread"),
        ("Global", "Ctrl+Shift+F", "find in all threads"),
        ("Global", "Ctrl+T", "toggle pane focus (chat / sidebar)"),
        ("Global", "Ctrl+G", "open the diagnostics log viewer"),
        ("Global", "Ctrl+O", "toggle the status strip"),
        ("Global", "Ctrl+S", "toggle the thoughts block"),
        ("Global", "Ctrl+M", "open the model picker"),
        (
            "Global",
            "Ctrl+L",
            "stop the running request (confirmation)",
        ),
        (
            "Global",
            "Shift+Ctrl+L",
            "reset this thread · clear the dialog (confirmation)",
        ),
        ("Global", "Ctrl+↑/↓ · Alt+↑/↓", "switch the active thread"),
        ("Global", "PgUp / PgDn", "scroll the chat view"),
        (
            "Global",
            "Wheel ↑ / ↓",
            "scroll the chat · select in the focused sidebar",
        ),
        (
            "Global",
            "Drag-select",
            "highlight chat text · release copies it",
        ),
        (
            "Global",
            "Ctrl-chords",
            "work under RU / UA keyboard layouts",
        ),
        ("Global", "Esc", "clear the input · dismiss popups"),
        ("Input", "Enter", "send the message (queues while busy)"),
        ("Input", "⇧↵ / ⌥↵", "insert a newline"),
        ("Input", "↑ / ↓", "move the caret in the draft"),
        ("Input", "Ctrl+A / Ctrl+E", "caret to line start / end"),
        ("Input", "Ctrl+U", "delete to the start of the line"),
        ("Input", "Ctrl+V / Cmd+V", "paste the clipboard"),
        ("Input", "Backspace", "delete backwards"),
        ("Input", "text", "type into the draft"),
        (
            "Sidebar (Ctrl+T)",
            "↑ / ↓",
            "select a thread (switches live)",
        ),
        ("Sidebar (Ctrl+T)", "Enter · Esc", "return to the editor"),
        ("Rename (Ctrl+R)", "Enter", "save the title"),
        (
            "Rename (Ctrl+R)",
            "⇧↵ / ⌥↵",
            "insert a newline into the title",
        ),
        ("Rename (Ctrl+R)", "Backspace", "delete backwards"),
        ("Rename (Ctrl+R)", "Esc", "cancel the rename"),
        ("Delete (Ctrl+D)", "Enter · y", "confirm the delete"),
        ("Delete (Ctrl+D)", "Esc · n", "cancel"),
        ("Model picker (Ctrl+M)", "↑ / ↓", "move the selection"),
        ("Model picker (Ctrl+M)", "PgUp / PgDn", "page the selection"),
        (
            "Model picker (Ctrl+M)",
            "Enter",
            "apply the highlighted model",
        ),
        ("Model picker (Ctrl+M)", "Esc", "close without switching"),
        (
            "Find in thread (Ctrl+F)",
            "text",
            "edit the query (live filter)",
        ),
        ("Find in thread (Ctrl+F)", "Backspace", "delete backwards"),
        ("Find in thread (Ctrl+F)", "↑ / ↓", "walk the matches"),
        (
            "Find in thread (Ctrl+F)",
            "Enter",
            "jump to the match · close",
        ),
        ("Find in thread (Ctrl+F)", "Esc", "close without jumping"),
        (
            "Find everywhere (Ctrl+⇧F)",
            "text",
            "edit the query (live filter)",
        ),
        ("Find everywhere (Ctrl+⇧F)", "Backspace", "delete backwards"),
        ("Find everywhere (Ctrl+⇧F)", "↑ / ↓", "walk the matches"),
        (
            "Find everywhere (Ctrl+⇧F)",
            "Enter",
            "activate the match's thread · jump",
        ),
        ("Find everywhere (Ctrl+⇧F)", "Esc", "close without jumping"),
        (
            "Log viewer (Ctrl+G)",
            "↑ / ↓ · k / j",
            "move the cursor one line",
        ),
        ("Log viewer (Ctrl+G)", "PgUp / PgDn", "page the view"),
        ("Log viewer (Ctrl+G)", "g / G", "jump to the top / the tail"),
        ("Log viewer (Ctrl+G)", "w", "toggle wrap"),
        ("Log viewer (Ctrl+G)", "/", "search the log"),
        ("Log viewer (Ctrl+G)", "n / N", "next / previous match"),
        ("Log viewer (Ctrl+G)", "y", "copy the cursor line"),
        ("Log viewer (Ctrl+G)", "Esc", "close"),
        ("Log search (/)", "text", "edit the pattern"),
        ("Log search (/)", "Backspace", "delete backwards"),
        ("Log search (/)", "Enter", "commit the search"),
        ("Log search (/)", "Esc", "cancel the search"),
        ("Error popup", "R", "reconnect"),
        ("Error popup", "q", "quit"),
        ("Error popup", "Esc · any key", "dismiss"),
        ("Help (F1)", "↑ / ↓ · PgUp / PgDn", "scroll the list"),
        ("Help (F1)", "F1 · Esc", "close"),
    ];

    #[test]
    fn help_modal_table_matches_the_enumerated_dispatch() {
        let table = chibi_tui::ui::HOTKEY_ROWS;
        assert_eq!(
            table.len(),
            DISPATCH_CHORDS.len(),
            "table row count drifted from the dispatch enumeration"
        );
        for (i, (row, (group, chord, action))) in
            table.iter().zip(DISPATCH_CHORDS.iter()).enumerate()
        {
            assert_eq!(row.group, *group, "row {i} group");
            assert_eq!(row.chord, *chord, "row {i} chord");
            assert_eq!(row.action, *action, "row {i} action");
        }
    }

    /// What the probe asserts about the draft after the chord was pressed.
    #[derive(Clone, Copy, PartialEq)]
    enum DraftEffect {
        /// The documented action does not touch the draft.
        Unchanged,
        /// The documented action empties the draft (Esc, ^U).
        Cleared,
        /// The documented action appends a newline (⇧↵ / ⌥↵).
        Newline,
        /// The documented action deletes backwards (Backspace).
        Shrink,
    }

    /// Concrete key events behind one Normal-mode table row, plus where the
    /// press must happen (sidebar focus or editor focus) and the expected
    /// draft effect. `text` rows are excluded: typing into the draft IS
    /// their documented action, and the existing `type_in` coverage pins it.
    /// The paste row (`Ctrl+V / Cmd+V`) is excluded too, but for the
    /// OPPOSITE reason: pressing it hits the REAL system clipboard through
    /// arboard, and parallel native clipboard access across the test
    /// threads SIGSEGVs the test process on macOS (measured 2/10 full-suite
    /// runs with the press, 0/10 at the baseline and without it). The row
    /// stays pinned by the enumeration test above plus the dedicated
    /// `ctrl_v_pastes_clipboard_into_input`, so coverage parity with the
    /// pre-probe suite is preserved — one arboard caller, not three.
    fn normal_mode_probes() -> Vec<(String, Vec<crossterm::event::KeyEvent>, DraftEffect, bool)> {
        use crossterm::event::KeyModifiers as M;
        let e = |code, m| key_event(code, m);
        vec![
            (
                "F1".into(),
                vec![e(KeyCode::F(1), M::NONE)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+C".into(),
                vec![e(KeyCode::Char('c'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+N".into(),
                vec![e(KeyCode::Char('n'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+P".into(),
                vec![e(KeyCode::Char('p'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+R".into(),
                vec![e(KeyCode::Char('r'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+D".into(),
                vec![e(KeyCode::Char('d'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+F".into(),
                vec![e(KeyCode::Char('f'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+Shift+F".into(),
                vec![e(KeyCode::Char('f'), M::CONTROL | M::SHIFT)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+T".into(),
                vec![e(KeyCode::Char('t'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+G".into(),
                vec![e(KeyCode::Char('g'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+O".into(),
                vec![e(KeyCode::Char('o'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+S".into(),
                vec![e(KeyCode::Char('s'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+M".into(),
                vec![e(KeyCode::Char('m'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+L".into(),
                vec![e(KeyCode::Char('l'), M::CONTROL)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+↑/↓ · Alt+↑/↓".into(),
                vec![e(KeyCode::Up, M::CONTROL), e(KeyCode::Up, M::ALT)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "PgUp / PgDn".into(),
                vec![e(KeyCode::PageUp, M::NONE), e(KeyCode::PageDown, M::NONE)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Esc".into(),
                vec![e(KeyCode::Esc, M::NONE)],
                DraftEffect::Cleared,
                false,
            ),
            (
                "Enter".into(),
                vec![e(KeyCode::Enter, M::NONE)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "⇧↵ / ⌥↵".into(),
                vec![e(KeyCode::Enter, M::SHIFT), e(KeyCode::Enter, M::ALT)],
                DraftEffect::Newline,
                false,
            ),
            (
                "↑ / ↓".into(),
                vec![e(KeyCode::Up, M::NONE), e(KeyCode::Down, M::NONE)],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+A / Ctrl+E".into(),
                vec![
                    e(KeyCode::Char('a'), M::CONTROL),
                    e(KeyCode::Char('e'), M::CONTROL),
                ],
                DraftEffect::Unchanged,
                false,
            ),
            (
                "Ctrl+U".into(),
                vec![e(KeyCode::Char('u'), M::CONTROL)],
                DraftEffect::Cleared,
                false,
            ),
            (
                "Backspace".into(),
                vec![e(KeyCode::Backspace, M::NONE)],
                DraftEffect::Shrink,
                false,
            ),
            (
                "↑ / ↓".into(),
                vec![e(KeyCode::Up, M::NONE), e(KeyCode::Down, M::NONE)],
                DraftEffect::Unchanged,
                true,
            ),
            (
                "Enter · Esc".into(),
                vec![e(KeyCode::Enter, M::NONE), e(KeyCode::Esc, M::NONE)],
                DraftEffect::Unchanged,
                true,
            ),
        ]
    }

    #[test]
    fn help_modal_normal_mode_rows_are_claimed_by_the_live_dispatch() {
        use chibi_tui::app::Focus;
        for (chord, events, effect, sidebar) in normal_mode_probes() {
            for event in events {
                let mut app = app_with_chats(3);
                type_in(&mut app, "draft");
                if sidebar {
                    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
                    assert_eq!(app.focus, Focus::Sidebar, "{chord}: setup");
                }
                press(&mut app, event.code, event.modifiers);
                // Newline-preserving join: a ⇧↵ probe must SEE the
                // multi-line draft (["draft", ""] → "draft\n").
                let after = app.input.lines().join("\n");
                match effect {
                    DraftEffect::Unchanged => {
                        assert_eq!(after, "draft", "{chord}: typed into the draft");
                    }
                    DraftEffect::Cleared => {
                        assert_eq!(after, "", "{chord}: draft not cleared");
                    }
                    DraftEffect::Newline => {
                        assert_eq!(after, "draft\n", "{chord}: newline not inserted");
                    }
                    DraftEffect::Shrink => {
                        assert_eq!(after, "draf", "{chord}: backspace not applied");
                    }
                }
            }
        }
    }

    /// Every in-modal row of the table must be captured by ITS popup: open
    /// the popup, press the row's chord, the popup must still own the
    /// keyboard afterwards (or close when the row says so), and nothing may
    /// leak into the draft. Drift here means the modal lost a key the help
    /// promises. Edge cases lean on the dispatch's honest no-ops: the
    /// picker's Enter on a `Loading` listing and the search popups' Enter
    /// with zero matches are guarded no-ops that keep the popup open.
    #[test]
    fn help_modal_in_modal_rows_are_captured_by_their_popups() {
        type PopupCase<'a> = (
            &'a dyn Fn(&mut chibi_tui::app::App),
            &'a [(KeyCode, KeyModifiers)],
            &'a [(KeyCode, KeyModifiers)],
        );
        let cases: Vec<PopupCase<'_>> = vec![
            (
                &|app: &mut chibi_tui::app::App| {
                    press(app, KeyCode::Char('r'), KeyModifiers::CONTROL);
                },
                &[
                    (KeyCode::Char('x'), KeyModifiers::NONE),
                    (KeyCode::Backspace, KeyModifiers::NONE),
                ],
                &[(KeyCode::Esc, KeyModifiers::NONE)],
            ),
            (
                &|app: &mut chibi_tui::app::App| {
                    press(app, KeyCode::Char('d'), KeyModifiers::CONTROL);
                },
                &[(KeyCode::Char('x'), KeyModifiers::NONE)],
                &[(KeyCode::Esc, KeyModifiers::NONE)],
            ),
            (
                &|app: &mut chibi_tui::app::App| {
                    press(app, KeyCode::Char('m'), KeyModifiers::CONTROL);
                },
                &[
                    (KeyCode::Up, KeyModifiers::NONE),
                    (KeyCode::Down, KeyModifiers::NONE),
                    (KeyCode::PageUp, KeyModifiers::NONE),
                    (KeyCode::PageDown, KeyModifiers::NONE),
                    (KeyCode::Enter, KeyModifiers::NONE),
                    (KeyCode::Char('x'), KeyModifiers::NONE),
                ],
                &[(KeyCode::Esc, KeyModifiers::NONE)],
            ),
            (
                &|app: &mut chibi_tui::app::App| {
                    press(app, KeyCode::Char('f'), KeyModifiers::CONTROL);
                },
                &[
                    (KeyCode::Char('x'), KeyModifiers::NONE),
                    (KeyCode::Backspace, KeyModifiers::NONE),
                    (KeyCode::Up, KeyModifiers::NONE),
                    (KeyCode::Down, KeyModifiers::NONE),
                    (KeyCode::Enter, KeyModifiers::NONE),
                ],
                &[(KeyCode::Esc, KeyModifiers::NONE)],
            ),
            (
                &|app: &mut chibi_tui::app::App| {
                    press(
                        app,
                        KeyCode::Char('f'),
                        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                    );
                },
                &[
                    (KeyCode::Char('x'), KeyModifiers::NONE),
                    (KeyCode::Backspace, KeyModifiers::NONE),
                    (KeyCode::Up, KeyModifiers::NONE),
                    (KeyCode::Down, KeyModifiers::NONE),
                    (KeyCode::Enter, KeyModifiers::NONE),
                ],
                &[(KeyCode::Esc, KeyModifiers::NONE)],
            ),
            (
                &|app: &mut chibi_tui::app::App| {
                    press(app, KeyCode::Char('g'), KeyModifiers::CONTROL);
                },
                &[
                    (KeyCode::Up, KeyModifiers::NONE),
                    (KeyCode::Down, KeyModifiers::NONE),
                    (KeyCode::PageUp, KeyModifiers::NONE),
                    (KeyCode::PageDown, KeyModifiers::NONE),
                    (KeyCode::Char('g'), KeyModifiers::NONE),
                    (KeyCode::Char('G'), KeyModifiers::NONE),
                    (KeyCode::Char('w'), KeyModifiers::NONE),
                    (KeyCode::Char('/'), KeyModifiers::NONE),
                    (KeyCode::Char('n'), KeyModifiers::NONE),
                    (KeyCode::Char('N'), KeyModifiers::NONE),
                    (KeyCode::Char('y'), KeyModifiers::NONE),
                    (KeyCode::Char('x'), KeyModifiers::NONE),
                ],
                &[(KeyCode::Esc, KeyModifiers::NONE)],
            ),
        ];
        for (open, stay, close) in cases {
            for (code, mods) in stay {
                let mut app = app_with_chats(2);
                type_in(&mut app, "draft");
                open(&mut app);
                let which = std::mem::discriminant(&app.mode);
                press(&mut app, *code, *mods);
                assert_eq!(
                    std::mem::discriminant(&app.mode),
                    which,
                    "{code:?} must stay inside the popup"
                );
                assert!(
                    app.input.lines().join("").starts_with("draft"),
                    "{code:?} leaked into the draft"
                );
            }
            for (code, mods) in close {
                let mut app = app_with_chats(2);
                open(&mut app);
                press(&mut app, *code, *mods);
                assert!(app.mode.is_normal(), "{code:?} must close the popup");
            }
        }
    }

    #[test]
    fn f1_toggles_the_help_modal_and_esc_closes() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "draft");
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(matches!(app.mode, chibi_tui::app::Mode::HelpViewing { .. }));
        // Same-chord toggle: F1 closes, F1 reopens.
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(app.mode.is_normal());
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(matches!(app.mode, chibi_tui::app::Mode::HelpViewing { .. }));
        // Esc closes too; the draft survives the round trip.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.mode.is_normal());
        assert_eq!(app.input.lines().join(""), "draft");
        // Enter inside the modal is swallowed: it can never submit.
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.mode, chibi_tui::app::Mode::HelpViewing { .. }));
        assert_eq!(app.input.lines().join(""), "draft");
    }

    #[test]
    fn f1_opens_the_help_modal_from_sidebar_focus() {
        use chibi_tui::app::Focus;
        let mut app = app_with_chats(2);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, Focus::Sidebar);
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(matches!(app.mode, chibi_tui::app::Mode::HelpViewing { .. }));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.mode.is_normal());
        assert_eq!(app.focus, Focus::Chat, "closing a modal returns the editor");
    }

    #[test]
    fn help_modal_does_not_open_over_other_popups() {
        // The modal family is strictly one-at-a-time: entry is Normal-only.
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }));
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::LogViewer { .. }),
            "the viewer keeps the keyboard; F1 is swallowed"
        );
    }

    #[test]
    fn help_modal_scroll_keys_page_the_window() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        let total = chibi_tui::ui::help_modal_total_lines();
        assert!(total > 10, "the table must have content: {total}");
        let page = app.help_visible_rows.max(1) as usize;
        let scroll = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::HelpViewing { state } => state.scroll,
            _ => panic!("modal must stay open"),
        };
        assert_eq!(scroll(&app), 0, "opens at the top");

        // ↓ walks one line at a time; ↑ back to the top clamp.
        app.help_scroll_down();
        assert_eq!(scroll(&app), 1);
        app.help_scroll_up();
        assert_eq!(scroll(&app), 0);
        app.help_scroll_up();
        assert_eq!(scroll(&app), 0, "clamped at the top, no wraparound");

        // PgDn pages by the render-fed viewport; PgUp returns; bottom clamp
        // pins the window at the table's last row.
        app.help_page_down();
        assert_eq!(scroll(&app), page);
        app.help_page_up();
        assert_eq!(scroll(&app), 0);
        let many = (total / page) + 2;
        for _ in 0..many {
            app.help_page_down();
        }
        assert_eq!(
            scroll(&app),
            total.saturating_sub(page),
            "clamped at the bottom edge"
        );
        // One-line scrolls respect the same bottom clamp.
        for _ in 0..3 {
            app.help_scroll_down();
        }
        assert_eq!(scroll(&app), total.saturating_sub(page));
    }

    // ---- Cyrillic Ctrl-chord normalization -----------------------------------

    /// The full Russian (ЙЦУКЕН) letter mapping, case-preserving on both
    /// sides: lowercase layout char → lowercase Latin twin, uppercase →
    /// uppercase (the case-sensitive `Char('L')` reset shape depends on it).
    #[test]
    fn cyrillic_table_maps_the_full_russian_layout_case_preserving() {
        let ru: &[(char, char)] = &[
            ('й', 'q'),
            ('ц', 'w'),
            ('у', 'e'),
            ('к', 'r'),
            ('е', 't'),
            ('н', 'y'),
            ('г', 'u'),
            ('ш', 'i'),
            ('щ', 'o'),
            ('з', 'p'),
            ('ф', 'a'),
            ('ы', 's'),
            ('в', 'd'),
            ('а', 'f'),
            ('п', 'g'),
            ('р', 'h'),
            ('о', 'j'),
            ('л', 'k'),
            ('д', 'l'),
            ('ь', 'm'),
            ('я', 'z'),
            ('ч', 'x'),
            ('с', 'c'),
            ('м', 'v'),
            ('и', 'b'),
            ('т', 'n'),
        ];
        for &(cyr, lat) in ru {
            assert_eq!(cyrillic_latin_counterpart(cyr), Some(lat), "{cyr}");
            let upper = cyr.to_uppercase().next().unwrap_or(cyr);
            assert_eq!(
                cyrillic_latin_counterpart(upper),
                Some(lat.to_ascii_uppercase()),
                "{upper}"
            );
        }
    }

    /// Ukrainian-only keys: і shares the Latin `s` key with the Russian ы,
    /// ї / є / ґ sit on the `]` / `'` / `\` keys of the layout.
    #[test]
    fn cyrillic_table_covers_the_ukrainian_only_keys() {
        assert_eq!(cyrillic_latin_counterpart('і'), Some('s'));
        assert_eq!(cyrillic_latin_counterpart('І'), Some('S'));
        assert_eq!(cyrillic_latin_counterpart('ї'), Some(']'));
        assert_eq!(cyrillic_latin_counterpart('Ї'), Some(']'));
        assert_eq!(cyrillic_latin_counterpart('є'), Some('\''));
        assert_eq!(cyrillic_latin_counterpart('Є'), Some('\''));
        assert_eq!(cyrillic_latin_counterpart('ґ'), Some('\\'));
        assert_eq!(cyrillic_latin_counterpart('Ґ'), Some('\\'));
    }

    /// Normalization touches ONLY Ctrl-chords: characters outside both
    /// layouts pass through unchanged, and events without CONTROL — plain
    /// typing, Alt-decorated keys — are never rewritten.
    #[test]
    fn cyrillic_normalization_touches_ctrl_chords_only_and_passes_unknown_through() {
        let norm = |c: char, mods: KeyModifiers| {
            normalize_cyrillic_ctrl_chord(key_event(KeyCode::Char(c), mods))
        };
        // Lowercase and uppercase shapes, modifiers preserved.
        let key = norm('ф', KeyModifiers::CONTROL);
        assert_eq!(key.code, KeyCode::Char('a'));
        assert!(key.modifiers.contains(KeyModifiers::CONTROL));
        let key = norm('Д', KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(key.code, KeyCode::Char('L'));
        assert!(key.modifiers.contains(KeyModifiers::SHIFT));
        // Characters outside both layouts pass through unchanged…
        assert_eq!(norm('λ', KeyModifiers::CONTROL).code, KeyCode::Char('λ'));
        assert_eq!(norm('q', KeyModifiers::CONTROL).code, KeyCode::Char('q'));
        // …and so does EVERYTHING without CONTROL.
        assert_eq!(norm('ф', KeyModifiers::NONE).code, KeyCode::Char('ф'));
        assert_eq!(norm('ф', KeyModifiers::ALT).code, KeyCode::Char('ф'));
    }

    #[test]
    fn cyrillic_ctrl_s_and_ukrainian_ctrl_i_toggle_thoughts() {
        let mut app = app_with_chats(1);
        assert!(app.thoughts_visible);
        // ы is the ЙЦУКЕН twin of the Latin `s` key (see the table test)…
        press(&mut app, KeyCode::Char('ы'), KeyModifiers::CONTROL);
        assert!(!app.thoughts_visible, "Ctrl+ы (RU) toggles like Ctrl+S");
        // …і its Ukrainian counterpart.
        press(&mut app, KeyCode::Char('і'), KeyModifiers::CONTROL);
        assert!(app.thoughts_visible, "Ctrl+і (UA) toggles like Ctrl+S too");
    }

    /// Layout-equivalence: a Cyrillic chord must leave the app in EXACTLY
    /// the state its Latin original would — including the uppercase
    /// Shift-decorated shapes, which the editor treats as unknown ctrl
    /// combos (a no-op here) in BOTH flavors.
    #[test]
    fn cyrillic_chords_reach_the_identical_state_as_their_latin_originals() {
        // Lowercase Ctrl+A vs Ctrl+ф (the ЙЦУКЕН `a`-position key): caret
        // to the line head in both.
        let mut cyr = app_with_chats(1);
        type_in(&mut cyr, "abcdef");
        press(&mut cyr, KeyCode::Char('ф'), KeyModifiers::CONTROL);
        let mut lat = app_with_chats(1);
        type_in(&mut lat, "abcdef");
        press(&mut lat, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(cyr.input.cursor(), lat.input.cursor(), "lowercase shape");
        assert_eq!(cyr.input.cursor(), (0, 0), "Ctrl+A semantics reached");

        // Uppercase Shift-decorated Ctrl+А vs Ctrl+Shift+A: identical
        // (byte-for-byte the same normalized event, same no-op outcome).
        let mut cyr = app_with_chats(1);
        type_in(&mut cyr, "abcdef");
        press(
            &mut cyr,
            KeyCode::Char('А'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let mut lat = app_with_chats(1);
        type_in(&mut lat, "abcdef");
        press(
            &mut lat,
            KeyCode::Char('A'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(cyr.input.cursor(), lat.input.cursor(), "uppercase shape");
        assert_eq!(
            cyr.input.lines(),
            lat.input.lines(),
            "no keystroke rewritten beyond the layout translation"
        );
    }

    /// The task's headline example: Ctrl+Ф (the ЙЦУКЕН key on the Latin `a`
    /// position) reaches the textarea's readline head-of-line mapping like
    /// Ctrl+A does.
    #[test]
    fn cyrillic_ctrl_f_moves_the_caret_to_line_head_like_ctrl_a() {
        let mut app = app_with_chats(1);
        for ch in "abc".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('ф'), KeyModifiers::CONTROL);
        assert_eq!(app.input.cursor(), (0, 0), "Ctrl+ф behaves like Ctrl+A");
    }

    /// The case-sensitive stop/reset split survives the translation:
    /// lowercase д → the plain ^L stop confirm, uppercase Д → the
    /// `Char('L')` reset shape (Shift+Ctrl+L).
    #[test]
    fn cyrillic_stop_and_reset_chords_keep_their_case_semantics() {
        let mut app = busy_app_with_commands(&["/stop", "/reset"]);
        press(&mut app, KeyCode::Char('д'), KeyModifiers::CONTROL);
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Stop
            }
        ));

        let mut app = busy_app_with_commands(&["/stop", "/reset"]);
        press(
            &mut app,
            KeyCode::Char('Д'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(matches!(
            app.mode,
            chibi_tui::app::Mode::ConfirmStopReset {
                action: chibi_tui::app::StopResetAction::Reset
            }
        ));
    }

    #[test]
    fn cyrillic_ctrl_c_cancels_the_inflight_request_like_latin_ctrl_c() {
        let mut app = app_with_chats(1);
        let submitted = submit_text(&mut app, "in flight");
        press(&mut app, KeyCode::Char('с'), KeyModifiers::CONTROL);
        let (request_id, _) = app.pending_cancel.expect("cancel requested");
        assert_eq!(request_id, submitted.request_id);
        assert!(!app.should_quit, "busy Ctrl+с must not quit");
    }

    /// Plain Cyrillic typing must never be rewritten: the textarea receives
    /// the layout characters verbatim (normalization is Ctrl-chord-only).
    #[test]
    fn plain_cyrillic_typing_lands_in_the_draft_untouched() {
        let mut app = app_with_chats(1);
        type_in(&mut app, "привет, мир");
        assert_eq!(app.input.lines().join(""), "привет, мир");
        assert!(!app.should_quit);
        assert!(app.active_request_id().is_none());
    }

    // ---- mouse wheel routing --------------------------------------------

    /// Hand-built wheel notch at a terminal position, as crossterm delivers it.
    fn wheel(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    use crossterm::event::MouseEventKind;

    /// 120×40 frame: sidebar cols 0..=25, chat cols 26.., chrome rows 37..39.
    const WHEEL_AREA: Rect = Rect::new(0, 0, 120, 40);

    #[test]
    fn wheel_up_over_the_chat_unpins_from_follow_bottom_by_the_wheel_step() {
        let mut app = app_with_chats(1);
        assert!(app.at_bottom(), "opens pinned to the tail");
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollUp, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(app.scroll, 3, "one notch = WHEEL_STEP rows");
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollUp, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(app.scroll, 6);
        assert!(!app.at_bottom());
    }

    #[test]
    fn wheel_down_over_the_chat_returns_to_follow_bottom() {
        let mut app = app_with_chats(1);
        app.scroll_up(6);
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(app.scroll, 3);
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(app.scroll, 0, "saturates at 0 = follow-bottom re-pinned");
        assert!(app.at_bottom());
        // further down-notches stay pinned: scroll can never go negative.
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn wheel_over_the_sidebar_without_focus_does_nothing() {
        use chibi_tui::app::Focus;
        let mut app = app_with_chats(3);
        assert_eq!(app.focus, Focus::Chat);
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 10, 5),
            WHEEL_AREA,
        );
        assert_eq!(app.active, 0, "hovering must never switch threads");
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, 10, 5), WHEEL_AREA);
        assert_eq!(app.active, 0);
        assert_eq!(app.scroll, 0, "sidebar hover never scrolls the chat either");
    }

    #[test]
    fn wheel_over_the_focused_sidebar_moves_the_selection() {
        use chibi_tui::app::Focus;
        let mut app = app_with_chats(3);
        app.focus = Focus::Sidebar;
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 10, 5),
            WHEEL_AREA,
        );
        assert_eq!(app.active, 1, "wheel down = next thread (live switching)");
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, 10, 5), WHEEL_AREA);
        assert_eq!(app.active, 0, "wheel up = previous thread");
        // The top clamp holds: no wraparound past the first thread.
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, 10, 5), WHEEL_AREA);
        assert_eq!(app.active, 0);
    }

    #[test]
    fn wheel_over_the_chrome_rows_is_ignored() {
        let mut app = app_with_chats(2);
        // spinner / input / hints rows (y >= 37) are no panel.
        for row in [37, 38, 39] {
            handle_mouse(
                &mut app,
                wheel(MouseEventKind::ScrollUp, 60, row),
                WHEEL_AREA,
            );
            handle_mouse(
                &mut app,
                wheel(MouseEventKind::ScrollDown, 60, row),
                WHEEL_AREA,
            );
        }
        assert_eq!(app.scroll, 0);
        assert_eq!(app.active, 0);
    }

    #[test]
    fn non_wheel_mouse_events_are_ignored() {
        let mut app = app_with_chats(2);
        app.scroll_up(9);
        let kinds = [
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            MouseEventKind::Moved,
        ];
        for kind in kinds {
            handle_mouse(&mut app, wheel(kind, 60, 10), WHEEL_AREA);
            handle_mouse(&mut app, wheel(kind, 10, 5), WHEEL_AREA);
        }
        assert_eq!(app.scroll, 9, "clicks/drag/motion never touch the scroll");
        assert_eq!(app.active, 0);
    }

    #[test]
    fn wheel_inside_the_open_help_modal_scrolls_the_table() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        let scroll = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::HelpViewing { state } => state.scroll,
            _ => panic!("modal must stay open"),
        };
        // The modal owns the wheel wherever the cursor is — same isolation
        // as the keyboard.
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(scroll(&app), 3, "three lines per notch");
        handle_mouse(&mut app, wheel(MouseEventKind::ScrollUp, 10, 5), WHEEL_AREA);
        assert_eq!(scroll(&app), 0, "clamped at the top edge");
    }

    #[test]
    fn wheel_inside_the_open_log_viewer_moves_the_cursor() {
        let mut app = app_with_chats(1);
        app.mode = chibi_tui::app::Mode::LogViewer {
            state: log_viewer_state(
                5,
                vec![
                    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
                ],
                None,
            ),
        };
        let cursor = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.cursor,
            _ => panic!("viewer must stay open"),
        };
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollUp, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(cursor(&app), 2, "wheel up walks three logical lines up");
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, 10),
            WHEEL_AREA,
        );
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(cursor(&app), 8);
    }

    #[test]
    fn wheel_inside_the_open_model_picker_moves_the_selection() {
        let mut app = app_with_chats(1);
        inject_ready_picker(&mut app, 10);
        let selected = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::ModelPicking { state } => state.selected,
            _ => panic!("picker must stay open"),
        };
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollDown, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(selected(&app), 3, "wheel down walks three rows");
        // The bottom clamp holds: no wraparound past the last row.
        for _ in 0..10 {
            handle_mouse(
                &mut app,
                wheel(MouseEventKind::ScrollDown, 60, 10),
                WHEEL_AREA,
            );
        }
        assert_eq!(selected(&app), 9);
        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollUp, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(selected(&app), 6);
    }

    // ---- mouse text selection dispatch --------------------------------------

    use chibi_tui::app::ChatSelection;

    /// Render one frame so the renderer caches `App::chat_geometry` (the
    /// hit-test seam the mouse selection maps positions through).
    fn render_for_geometry(app: &mut chibi_tui::app::App) {
        let backend = ratatui::backend::TestBackend::new(WHEEL_AREA.width, WHEEL_AREA.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| chibi_tui::ui::draw(f, app, &chibi_tui::theme::Theme::tokyo_night()))
            .unwrap();
    }

    /// Left-button press / drag / release at a terminal position.
    fn button(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> MouseEvent {
        wheel(kind, column, row)
    }

    /// THE dispatch flow: press in the chat pane anchors, drag extends,
    /// release finalizes and copies the plain text through the injected
    /// clipboard seam.
    #[test]
    fn press_drag_release_selects_and_copies_through_the_seam() {
        let mut app = app_with_chats(1);
        app.chats[0]
            .messages
            .push(Message::assistant("hello world from selection"));
        render_for_geometry(&mut app);

        let copied = std::cell::RefCell::new(Vec::<String>::new());
        {
            let sink = &copied;
            let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
            // Press at the first content cell of the message row, drag
            // eleven columns right ("hello world"), release.
            handle_mouse_with_copy(
                &mut app,
                button(
                    MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    26,
                    2,
                ),
                WHEEL_AREA,
                &copy,
            );
            assert!(
                app.selection.as_ref().is_some_and(|s| s.dragging),
                "press starts the live drag"
            );
            handle_mouse_with_copy(
                &mut app,
                button(
                    MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                    37,
                    2,
                ),
                WHEEL_AREA,
                &copy,
            );
            handle_mouse_with_copy(
                &mut app,
                button(
                    MouseEventKind::Up(crossterm::event::MouseButton::Left),
                    40,
                    2,
                ),
                WHEEL_AREA,
                &copy,
            );
        }

        let sel = app.selection.expect("real selection held after release");
        assert!(!sel.dragging, "released");
        assert_eq!(
            copied.borrow().as_slice(),
            ["hello world"],
            "the release copied the selected plain text"
        );
    }

    /// A press+release without drag is a plain click: the selection clears
    /// and nothing is copied.
    #[test]
    fn plain_click_clears_and_copies_nothing() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::assistant("text here"));
        render_for_geometry(&mut app);

        let copied = std::cell::RefCell::new(Vec::<String>::new());
        let sink = &copied;
        let copy = |text: &str| sink.borrow_mut().push(text.to_owned());
        // Start a real selection first, then plain-click it away.
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                26,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                30,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                30,
                2,
            ),
            WHEEL_AREA,
            &copy,
        );
        assert!(app.selection.is_some(), "drag held a selection");
        assert_eq!(
            copied.borrow().as_slice(),
            ["text"],
            "the drag release copied the selection"
        );

        // Fresh sink for the click phase.
        let click_copied = std::cell::RefCell::new(Vec::<String>::new());
        let click_sink = &click_copied;
        let click_copy = |text: &str| click_sink.borrow_mut().push(text.to_owned());
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                30,
                2,
            ),
            WHEEL_AREA,
            &click_copy,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                30,
                2,
            ),
            WHEEL_AREA,
            &click_copy,
        );
        assert!(app.selection.is_none(), "plain click cleared");
        assert!(click_sink.borrow().is_empty(), "plain click copied nothing");
    }

    /// Esc clears the selection (the keyboard's deselect), including a
    /// live drag, and never quits.
    #[test]
    fn esc_clears_the_mouse_selection() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::assistant("text here"));
        render_for_geometry(&mut app);
        let noop = |_text: &str| {};

        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                26,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                34,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        assert!(app.selection.is_some());

        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.selection.is_none(), "Esc cleared the selection");
        assert!(!app.should_quit);
    }

    /// A press outside the chat pane (sidebar / chrome rows) is a plain
    /// click: it clears the selection instead of starting one.
    #[test]
    fn press_outside_the_chat_pane_clears_the_selection() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::assistant("text here"));
        render_for_geometry(&mut app);
        let noop = |_text: &str| {};

        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                26,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                30,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        assert!(app.selection.is_some(), "precondition");

        // Sidebar press (no focus — no thread switch either).
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                10,
                5,
            ),
            WHEEL_AREA,
            &noop,
        );
        assert!(app.selection.is_none(), "sidebar press cleared");
        assert_eq!(app.active, 0, "no hover switching");
    }

    /// Wheel scrolling during a live drag leaves the head in document-row
    /// space (the documented choice: the next drag event re-extends the
    /// selection; scroll alone never moves it) and the scroll itself works.
    #[test]
    fn wheel_during_a_drag_scrolls_without_moving_the_head() {
        let mut app = app_with_chats(1);
        app.chats[0]
            .messages
            .push(Message::assistant("hello world"));
        render_for_geometry(&mut app);
        let noop = |_text: &str| {};

        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                26,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                34,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        let head_before = app.selection.expect("live drag").head;

        handle_mouse(
            &mut app,
            wheel(MouseEventKind::ScrollUp, 60, 10),
            WHEEL_AREA,
        );
        assert_eq!(app.scroll, WHEEL_STEP, "the wheel still scrolls");
        assert_eq!(
            app.selection.expect("drag survives the wheel").head,
            head_before,
            "scroll does not move the head (document-row space choice)"
        );
    }

    /// A selection press while a modal owns the screen (delete-confirm
    /// popup open) is ignored — the popup owns the pointer too — and a
    /// selection made before the popup stays held underneath.
    #[test]
    fn selection_presses_are_ignored_while_a_modal_is_open() {
        let mut app = app_with_chats(1);
        app.chats[0].messages.push(Message::assistant("text here"));
        render_for_geometry(&mut app);
        let noop = |_text: &str| {};

        // Make a selection, then open the delete-confirm popup.
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                26,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                30,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        assert!(app.selection.is_some(), "precondition");
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, chibi_tui::app::Mode::ConfirmDelete);

        // Press under the popup: ignored (the selection is untouched —
        // the popup branch consumes the mouse like the keyboard).
        handle_mouse_with_copy(
            &mut app,
            button(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                30,
                2,
            ),
            WHEEL_AREA,
            &noop,
        );
        assert!(
            matches!(app.selection, Some(ChatSelection { dragging: true, .. })),
            "popup press must not start a new selection nor clear"
        );
    }
}
