//! Chibi TUI — terminal client for Chibi.
//!
//! Backend selection:
//! * default — [`chibi_tui::LiveBackend`]: spawns the real JSONL peer
//!   (`chibi ide --stdio`-compatible), handshakes, streams real answers;
//!   chats persist under the platform data dir (`<data>/chibi-tui/threads/`)
//!   and are restored on startup.
//! * `--mock` — [`chibi_tui::backend::MockBackend`] with pre-filled demo
//!   chats, no processes, no I/O: development / screenshot mode.
//!
//! UX layer (task 7):
//! * `Ctrl+C` cancels the in-flight request when one exists; quits only when
//!   idle. `Esc` clears non-empty input and dismisses the error popup.
//! * backend failures surface as a modal popup (`R` reconnect, `Esc`
//!   dismisses, `q`/`Ctrl+C` quit) instead of crashing or being silently dropped;
//! * connection state is shown in the status bar
//!   (`● connected / connecting… / disconnected (press R)`);
//! * readline-style input keys (`Ctrl+A/E/U/L`, word ops via tui-textarea)
//!   plus clipboard paste (`Ctrl+V`, macOS Cmd+V).
//!
//! All logic lives in the library crate (`lib.rs`); this binary only wires
//! the terminal.

use std::io;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use futures_util::StreamExt;
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
    // feat_shift_enter_newline: ask for the kitty keyboard protocol so
    // capable terminals deliver distinct Shift+Enter / Alt+Enter modifiers
    // instead of bare-Enter bytes. Best-effort: an unsupported terminal
    // ignores the escape sequence and the app degrades to submit-on-Enter
    // (documented in the README). The result is deliberately discarded:
    // setup must never abort the app over an optional enhancement, and the
    // teardown guard below restores whatever actually got enabled.
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    // Every exit path below (splash abort, `?` failures, normal end, panic
    // unwind) restores the terminal exactly once through this guard.
    let _terminal_restore = TerminalRestore;
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
        app = chibi_tui::app::App::new({
            let mut chats = history::load_chats_from(restore_dir.as_deref());
            if chats.is_empty() {
                chats.push(chibi_tui::app::Chat::new("New chat 1"));
            }
            chats
        });
        // Missing-backend setup screen loop: it runs before the main event
        // loop exists, so a retry (`r` on the screen) is just another
        // connect attempt. A quit from the screen leaves the app cleanly.
        loop {
            match chibi_tui::LiveBackend::connect(&cli.workspace).await {
                Ok(live) => {
                    app.connection = Connection::Connected;
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

    // feat_status_line: wire the strip's cwd source for BOTH backends (the
    // CLI flag defaults to the process cwd, so mock mode has it too). Read
    // reactively by the renderer each frame; the workspace is CLI-only
    // today, so the value never changes at runtime.
    app.workspace_root = Some(cli.workspace.to_string_lossy().into_owned());

    // Single channel: backend tasks push progress events, UI consumes.
    let (event_tx, mut event_rx) = mpsc::channel::<BackendEvent>(64);

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

/// RAII teardown: restores the terminal when dropped. Instantiated right
/// after terminal setup so EVERY exit path — splash abort (`return Ok(())`),
/// `?` failures, normal end, and panic unwind — leaves raw mode disabled,
/// the alternate screen left, mouse capture off, and (feat_shift_enter_newline)
/// kitty keyboard-enhancement flags popped.
struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
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
) {
    if let Source::Live(old) = source {
        let _ = old.shutdown().await;
    }
    *source = Source::LivePlaceholder;
    app.connection = Connection::Connecting;
    match chibi_tui::LiveBackend::connect(workspace).await {
        Ok(live) => {
            app.connection = Connection::Connected;
            app.dismiss_error();
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
                            // feat_rename_thread: snapshot BEFORE the key
                            // lands so the loop can tell a rename commit
                            // (Renaming --Enter--> Normal) apart from a plain
                            // message submission. Rename detection uses
                            // `matches!(.. Renaming)`, NOT `!is_normal()`,
                            // because the delete-confirm popup (feat_thread_
                            // delete) also leaves Normal mode, and its Enter
                            // is a deletion, never a rename.
                            let name_before = app.chat_title();
                            let was_renaming = matches!(app.mode, Mode::Renaming { .. });
                            let was_confirming_delete =
                                matches!(app.mode, Mode::ConfirmDelete);
                            // feat_search_thread: snapshot BEFORE the key
                            // lands too. The search popup's Enter jumps and
                            // closes, so by the time the loop runs the mode
                            // is already Normal again; only the snapshot can
                            // tell that Enter apart from a plain submit.
                            let was_searching = matches!(app.mode, Mode::Searching { .. });
                            // feat_search_all_threads: same snapshot for the
                            // GLOBAL search popup: its Enter activates the
                            // match's thread AND closes the popup, so the
                            // loop must know that Enter belonged to it.
                            let was_searching_all =
                                matches!(app.mode, Mode::SearchingAll { .. });
                            // feat_focus_panes: snapshot of the SIDEBAR-focus
                            // flag. Bare Enter while the sidebar holds focus
                            // means "apply & return to the editor" (handle_key
                            // flips focus back), so it must never also submit
                            // the message draft the editor still holds.
                            let was_sidebar_focused = app.focus == Focus::Sidebar;
                            // feat_model_picker_lite: the picker's Enter
                            // confirms the selected model: it belongs to
                            // the popup, never to message submission.
                            let was_model_picking =
                                matches!(app.mode, Mode::ModelPicking { .. });

                            handle_key(&mut app, key);

                            // feat_model_picker_lite: a hidden exchange
                            // staged by the key handlers (`^M` open or
                            // Enter selection) is sent through the SAME
                            // `send_submitted` path as any prompt; it just
                            // carries its own ids and adds no bubbles.
                            if let Some(hidden) = app.take_picker_submission() {
                                send_submitted(source, &hidden, event_tx.clone());
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
                            // feat_thread_delete: the Enter that confirmed the
                            // delete popup belongs to the popup too, so it must
                            // never submit the message draft to the neighbour.
                            let enter_consumed_by_delete =
                                was_confirming_delete && key.code == KeyCode::Enter;
                            // feat_search_thread: the Enter that jumped to the
                            // selected match belongs to the popup too, so it
                            // must never submit the message draft.
                            let enter_consumed_by_search =
                                was_searching && key.code == KeyCode::Enter;
                            // feat_search_all_threads: same gate for the
                            // global search popup's Enter: it activates the
                            // target thread and must never submit the draft
                            // to that (or any) chat.
                            let enter_consumed_by_search_all =
                                was_searching_all && key.code == KeyCode::Enter;
                            // feat_focus_panes: the sidebar's bare Enter is
                            // consumed too: it returns focus to the editor
                            // pane and never submits.
                            let enter_consumed_by_sidebar =
                                was_sidebar_focused && key.code == KeyCode::Enter;
                            // feat_model_picker_lite: the picker's Enter is
                            // consumed by the popup (model switch staged).
                            let enter_consumed_by_picker =
                                was_model_picking && key.code == KeyCode::Enter;

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
                                    connect_live(source, &mut app, workspace).await;
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
                            // feat_rename_thread: a rename commit takes priority
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
                                && !enter_consumed_by_search
                                && !enter_consumed_by_search_all
                                && !enter_consumed_by_sidebar
                                && !enter_consumed_by_picker
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

                            // bugfix_ctrl_l_screen_clear: ^L wipes the VISIBLE
                            // screen. Crossterm's Clear(All) is emitted HERE,
                            // outside ratatui's diff-based draw; Terminal::
                            // clear() also resets ratatui's cached back buffer,
                            // so the immediately-following draw repaints every
                            // cell and nothing stale lingers. App::request_
                            // clear_screen already snapped the chat view to
                            // follow-bottom before this fires.
                            if app.take_clear_screen_request() {
                                terminal.clear()?;
                                terminal.draw(|f| ui::draw(f, &mut app, theme))?;
                            }
                        }
                    }
                    Some(Ok(CtEvent::Resize(_, _))) => {
                        // Repaint immediately on the new size so the layout
                        // (sidebar width, code panels) adapts without waiting
                        // for the next event.
                        terminal.draw(|f| ui::draw(f, &mut app, theme))?;
                    }
                    Some(Ok(_)) => {}                     // mouse etc.
                    Some(Err(e)) => return Err(io::Error::other(e)),
                    None => {} // stream ended; keep looping until quit
                }
            }

            // ---- backend events ----
            Some(evt) = event_rx.recv() => {
                // Per-thread async: after a chat's terminal event, start its
                // next queued prompt (FIFO), including for background chats.
                let drain_thread_id = match &evt {
                    BackendEvent::QueueDrain { thread_id } => Some(thread_id.clone()),
                    _ => None,
                };
                if let Some(thread_id) = drain_thread_id {
                    if let Some(next) = app.dequeue_next_for(&thread_id) {
                        send_submitted(source, &next, event_tx.clone());
                    }
                    // feat_model_picker_lite: after the visible FIFO had its
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
                // feat_thread_delete: the transient status toast (busy
                // refusal) auto-expires on the same 100 ms cadence.
                app.tick_status_message();
            }
        }

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

/// Paste text from the system clipboard into the input field at the cursor.
/// Failures are silent — a locked clipboard must not disturb typing.
fn paste_clipboard(app: &mut chibi_tui::app::App) {
    if let Some(text) = chibi_tui::clipboard::get_text() {
        let cleaned: String = text.chars().filter(|&c| c != '\r' && c != '\n').collect();
        if !cleaned.is_empty() {
            app.input.insert_str(cleaned);
        }
    }
}

/// Returns true when the given key event should trigger submission of the
/// current input buffer.
///
/// Submit only on BARE Enter — never on every keystroke, and never when a
/// modifier rides along: Shift+Enter and Alt+Enter insert a newline into the
/// textarea instead (feat_shift_enter_newline). This keeps typing fluid and
/// prevents single-letter commands (q/j/k/N) from accidentally submitting
/// when the buffer is still empty.
///
/// Terminal caveat: without the kitty keyboard protocol many terminals send
/// bare-Enter bytes for Shift+Enter, so those presses degrade to submit.
/// `main` pushes crossterm's keyboard-enhancement flags at startup so
/// capable terminals deliver distinct SHIFT/ALT modifiers.
fn should_submit(key: &crossterm::event::KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers.is_empty()
}

/// Apply key handling to app state. Backend interactions (submit, cancel,
/// reconnect) happen in the event loop by observing state changes, keeping
/// this function synchronous and testable.
fn handle_key(app: &mut chibi_tui::app::App, key: crossterm::event::KeyEvent) {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // feat_alt_arrows_nav: Alt+↑/↓ are a full synonym of Ctrl+↑/↓ thread
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

    // ---- feat_stderr_log_modal: diagnostics log viewer modal ----
    //
    // While the ^G log viewer is open, ONLY viewer keys work: PgUp/PgDn (and
    // ↑/↓) scroll. PgUp/↑ detach from the live tail, PgDn/↓ return towards
    // it (reaching the bottom re-arms live-tail), Esc closes, Ctrl+C quits
    // (same class as the other popups). Everything else (typing, global
    // chords (^N/^R/^D/^T/^L/^F), thread switching) is swallowed so no
    // keystroke leaks into the textarea and no global binding fires. There
    // is no Enter action: the viewer is strictly read-only, so Enter is
    // swallowed and the loop's submit gates need no extra snapshot flag.
    if matches!(app.mode, Mode::LogViewer { .. }) {
        match key.code {
            KeyCode::PageUp => app.log_scroll_up(app.log_visible_rows),
            KeyCode::PageDown => app.log_scroll_down(app.log_visible_rows),
            // One-line flavor of the same scroll (plain arrows are nav here).
            KeyCode::Up => app.log_scroll_up(1),
            KeyCode::Down => app.log_scroll_down(1),
            KeyCode::Esc => {
                app.close_log_viewer();
            }
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            _ => {}
        }
        return;
    }

    // ---- feat_model_picker_lite: model-picker modal captures everything ----
    //
    // While the ^M picker is open, ONLY picker keys work: ↑/↓ move the
    // selection (clamped at the list edges), Enter confirms the highlighted
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
            KeyCode::Enter => app.confirm_model_picker(),
            KeyCode::Esc => {
                app.close_model_picker();
            }
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            _ => {}
        }
        return;
    }

    // ---- feat_thread_delete: modal confirm popup captures everything ----
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

    // ---- feat_search_thread: modal search popup captures everything ----
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

    // ---- feat_search_all_threads: modal GLOBAL search popup captures ----
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

    // ---- inline thread rename mode (feature: feat_rename_thread) ----
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
            // feat_shift_enter_newline: Shift+Enter / Alt+Enter insert a
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
                // modifier flavors (feat_ctrl_arrows_nav + feat_alt_arrows_
                // nav: Ctrl+↑/↓ AND Alt+↑/↓ switch threads in Normal mode,
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
                    let _ = tui_textarea::Input::from(key);
                }
            }
        }
        return;
    }

    // ---- feat_focus_panes: the SIDEBAR owns the keyboard --------------------
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
            // feat_stderr_log_modal: read-only viewer = service chord; same
            // Normal-mode ^G semantics under sidebar focus (parity contract).
            KeyCode::Char('g') if ctrl => app.begin_log_viewer(),
            // feat_status_line: same Normal-mode ^O semantics under sidebar
            // focus (parity contract with the chord match below).
            KeyCode::Char('o') if ctrl => app.toggle_status_strip(),
            // feat_model_picker_lite: same Normal-mode ^M semantics under
            // sidebar focus (parity contract with the chord match below).
            KeyCode::Char('m') if ctrl => app.begin_model_picker(),
            KeyCode::Char('l') if ctrl => {
                // Same pair as the global ^L arm below (clear + wipe intent).
                app.clear_input();
                app.request_clear_screen();
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
            // Return without touching the draft (never clears input).
            KeyCode::Esc => app.focus = Focus::Chat,
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
        (KeyCode::Char('l'), true) => {
            // Readline `^L` kept clearing the input, but in a chat TUI the
            // hint bar's `^L clear` reads as "wipe the visible screen"
            // (bugfix_ctrl_l_screen_clear). Both now: input is still cleared,
            // and a one-shot wipe intent is recorded for the event loop:
            // the real crossterm Clear(All) + full repaint is emitted there,
            // outside ratatui's diff-based draw.
            app.clear_input();
            app.request_clear_screen();
            return;
        }
        (KeyCode::Char('u'), true) => {
            // Delete from cursor to start of line. tui-textarea maps Ctrl+U
            // to undo, which surprises readline users. Override it here.
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
        // Ctrl+G: open the diagnostics log viewer (feat_stderr_log_modal).
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): the first-choice ^Y candidate was REJECTED:
        // tui-textarea 0.7 maps Ctrl+Y to paste-from-internal-yank (src/
        // textarea.rs:589), and the yank buffer IS populated in this app:
        // our own ^U override calls delete_line_by_head() → delete_piece()
        // which stores the killed text (textarea.rs:1022), so ^U→^Y (kill
        // line, paste it back) is live behavior today. Taking ^Y would break
        // that readline kill/yank family (^U/^K/^W/^Y). ^G is verified FREE:
        // no app binding anywhere in src/, no tui-textarea 0.7 mapping, no
        // macOS system hijack, no flow-control semantics, and its readline
        // meaning (abort) has no function in this TUI. Mnemonic: loG.
        (KeyCode::Char('g'), true) => {
            app.begin_log_viewer();
            return;
        }
        // Ctrl+O: TOGGLE THE STATUS STRIP (feat_status_line), the dim
        // one-row `cwd: <workspace> · <model>` readout on the chat pane's
        // top border. Hidden by default; pure view state (like ^T's Focus),
        // modals swallow the chord like every other one.
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): ^G was already taken by the log viewer, so
        // the other task candidate ^O was verified FREE: no app binding
        // anywhere in src/ (only `Char('o')` hits are plain typing), no
        // tui-textarea 0.7 shortcut (its Ctrl table covers a/b/d/e/f/h/j/k/
        // n/p/r/u/v/w/x/y/<>/[], no 'o'), not part of this app's readline
        // family (^A/^E/^U/^K/^W/^Y/^L; GNU readline's operate-and-get-
        // next is a shell-side binding that never fires inside the TUI), no
        // macOS system hijack (Mission Control only takes ^arrows), and the
        // legacy tty VDISCARD semantics of ^O are inert under raw mode plus
        // the kitty keyboard protocol. Mnemonic: infO.
        (KeyCode::Char('o'), true) => {
            app.toggle_status_strip();
            return;
        }
        // Ctrl+M: OPEN THE MODEL PICKER (feat_model_picker_lite), the
        // centered popup that fetches the bare `/model` listing as a hidden
        // exchange (no transcript bubbles) and switches models by sending
        // `/model <n>` the same hidden way.
        //
        // Chord verification (feat task discipline, see the executor report
        // for the full audit): tui-textarea 0.7 maps Ctrl+M to
        // insert_newline() (textarea.rs:274-286, the same arm as Enter),
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
        // Ctrl+Shift+F: GLOBAL search across ALL threads
        // (feat_search_all_threads). With the kitty keyboard protocol
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
        // Ctrl+F: open the in-thread search popup (feat_search_thread).
        // Guarded inside App::begin_search (Normal mode + active chat
        // only), so this can never fire over the confirm/rename popups;
        // those branches return before this match runs.
        (KeyCode::Char('f'), true) => {
            app.begin_search();
            return;
        }
        // Ctrl+T: TOGGLE PANE FOCUS (feat_focus_panes), which flips the keyboard
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
        // Ctrl+A / Ctrl+E reach tui-textarea's built-in readline mappings
        // (head/end of line); they fall through untouched below.
        _ => {}
    }

    // ---- vertical arrows & thread switching (feat_ctrl_arrows_nav + --------
    // feat_alt_arrows_nav) ----------------------------------------------------
    //
    // * Ctrl+↑ / Ctrl+↓ AND Alt+↑ / Alt+↓ switch the ACTIVE THREAD, carrying
    //   over EXACTLY the semantics plain ↑/↓ had before this rework:
    //   App::select_prev/next bounds-clamp the index and reset the chat
    //   scroll to 0; sidebar focus/dot refresh derives from `active` at draw
    //   time, so it follows for free. Alt is a FULL SYNONYM (not a fallback):
    //   both flavors route to the identical select_prev/select_next call:
    //   zero behavior divergence. Alt exists because macOS Mission Control
    //   hijacks Ctrl+arrows system-wide before they reach the terminal.
    // * Plain ↑ / ↓ move the TEXT CURSOR vertically inside the editor via
    //   tui-textarea's native Up/Down mapping (CursorMove::Up/Down). They
    //   never submit and never switch threads; the caret auto-follows the
    //   feat_input_grow viewport because ui::draw renders the widget over
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
            let converted: tui_textarea::Input = key.into();
            app.input.input(converted);
        }
        (KeyCode::PageUp, _) => app.scroll_up(app.chat_visible_rows),
        (KeyCode::PageDown, _) => app.scroll_down(app.chat_visible_rows),
        (KeyCode::Esc, _) if !input_is_empty => {
            // Non-empty input: clear it.
            app.clear_input();
        }
        // Esc is ignored when input is empty; it never quits.
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
            let converted: tui_textarea::Input = key.into();
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
            app.input.input(tui_textarea::Input {
                key: tui_textarea::Key::Char(ch),
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

    // ---- bugfix_ctrl_l_screen_clear ------------------------------------------

    #[test]
    fn ctrl_l_wipes_screen_intent_and_resets_scroll_to_bottom() {
        let mut app = app_with_chats(1);
        for ch in "hello".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.scroll_up(30);
        assert!(!app.at_bottom(), "precondition: chat view is scrolled up");

        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);

        assert!(
            app.input.lines().iter().all(|l| l.is_empty()),
            "^L keeps clearing the input (readline compatibility)"
        );
        assert_eq!(
            app.scroll, 0,
            "screen wipe snaps the chat view back to follow-bottom"
        );
        assert!(app.at_bottom());
        assert!(
            app.take_clear_screen_request(),
            "one-shot wipe intent handed to the event loop"
        );

        // Esc is unchanged: input-clear only, never a screen wipe.
        for ch in "again".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.input.lines().iter().all(|l| l.is_empty()));
        assert!(!app.clear_screen_requested, "Esc must not request a wipe");
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

    /// feat_shift_enter_newline: only a BARE Enter submits. Shift+Enter and
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

    /// feat_shift_enter_newline: Enter+SHIFT / Enter+ALT reach the textarea's
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

    /// feat_shift_enter_newline rename-mode interplay (chosen approach,
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

        // feat_ctrl_arrows_nav: Ctrl+↑/↓ (thread switching in Normal mode)
        // are just another dismiss key under the popup — no thread change.
        app.show_error("boom again");
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0, "Ctrl+Down must not switch chats via popup");
        assert!(app.error_popup.is_none(), "Ctrl+Down dismissed the popup");
        app.show_error("boom thrice");
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0, "Ctrl+Up must not switch chats via popup");

        // feat_alt_arrows_nav: the Alt synonym is swallowed identically.
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
    fn ctrl_l_clears_input() {
        let mut app = app_with_chats(1);
        for ch in "hello".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(app.input.lines().join(""), "hello");
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(app.input.lines().iter().all(|l| l.is_empty()));
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
        use tui_textarea::CursorMove;
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
        let _ = CursorMove::Forward; // keep import used when assertions change
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

    // ---- feat_rename_thread: key routing -----------------------------------

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
    /// (feat_ctrl_arrows_nav: plain ↑/↓ AND Ctrl+↑/↓).
    #[test]
    fn arrows_do_not_switch_chats_while_renaming() {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);

        // Plain arrows.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "navigation suppressed during rename");
        assert!(!matches!(app.mode, chibi_tui::app::Mode::Normal));

        // feat_ctrl_arrows_nav: the new thread-switch bindings must not leak
        // into rename mode either.
        press(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(app.active, 0, "Ctrl+arrows suppressed during rename");
        assert!(matches!(app.mode, Mode::Renaming { .. }), "session intact");

        // feat_alt_arrows_nav: the Alt synonym is blocked mid-rename too.
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

    // ---- feat_ctrl_arrows_nav: thread switching + caret movement ------------

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

    /// feat_alt_arrows_nav: Alt+↑ / Alt+↓ are a FULL SYNONYM of Ctrl+↑/↓ —
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

    /// feat_alt_arrows_nav parity: Alt+arrows work with a live multi-line
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

    /// Plain ↑/↓ move the text cursor vertically inside the editor — never
    /// switching threads. Column preserved across rows (tui-textarea native
    /// CursorMove), clamped at the first/last row. This exercises the grown
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

    // ---- feat_focus_panes: key routing --------------------------------------

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
    /// ^D opens the guarded confirm popup, ^L clears input + records the
    /// screen-wipe intent, ^R enters rename — and closing any of them hands
    /// focus back to Chat.
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

        // ^L clears input + wipes screen intent while Sidebar focused.
        let mut app = app_with_chats(1);
        type_in(&mut app, "gone");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(app.input.lines().iter().all(|l| l.is_empty()));
        assert!(app.take_clear_screen_request(), "wipe intent recorded");

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
        assert_eq!(app.active, 2, "the new chat is selected");
        assert_eq!(app.chats.len(), 3);
        assert!(app.at_bottom(), "scroll reset to follow-bottom");
    }

    /// Busy chats participate normally under Sidebar navigation: switching
    /// away from a running chat is allowed (feat_per_thread_async) and the
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

    // ---- feat_thread_delete: key routing ----------------------------------

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
        // feat_alt_arrows_nav: the Alt synonym is swallowed by the popup too.
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.chats.len(), 3, "Ctrl+N must not fire mid-popup");
        assert!(
            matches!(app.mode, chibi_tui::app::Mode::ConfirmDelete),
            "popup must stay open"
        );
        assert!(!app.clear_screen_requested, "Ctrl+L must not fire");
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

    // ---- feat_search_thread: key routing ----------------------------------

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
        // feat_alt_arrows_nav: the Alt synonym is swallowed by the popup too.
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE); // navigation, not caret
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        // feat_alt_arrows_nav: the Alt synonym is swallowed by the popup too.
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
        assert!(!app.clear_screen_requested, "Ctrl+L must not fire");
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

    // ---- feat_search_all_threads: key routing ----------------------------

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
        // feat_alt_arrows_nav: the Alt synonym is swallowed by the popup too.
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
        assert!(!app.clear_screen_requested, "Ctrl+L must not fire");
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

    // ---- feat_stderr_log_modal: ^G log viewer ------------------------------

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
        assert_eq!(state.scroll, 0, "opens live-tailing");
        assert!(
            state.lines.contains(&marker),
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
    fn log_viewer_pgup_pgdn_and_arrows_scroll() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);

        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(app.log_visible_rows, 20, "page = one modal page");
        let scroll = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.scroll,
            other => panic!("expected LogViewer, got {other:?}"),
        };
        assert_eq!(scroll(&app), 20, "PgUp detaches by one page");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(scroll(&app), 21, "↑ scrolls one row");
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(scroll(&app), 1);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(scroll(&app), 0, "back at the live tail");
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
        type_in(&mut app, "draft");
        app.scroll_up(30);
        let scroll_before = app.scroll;
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        let state_scroll = |app: &chibi_tui::app::App| match &app.mode {
            chibi_tui::app::Mode::LogViewer { state } => state.scroll,
            other => panic!("expected LogViewer, got {other:?}"),
        };

        // Typing must not reach the textarea and must not scroll (letters
        // avoid the popup's own keys — though none exist beyond nav/Esc).
        type_in(&mut app, "xyz");
        assert_eq!(state_scroll(&app), 0);
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
        assert!(!app.clear_screen_requested, "Ctrl+L must not fire");
        assert!(!app.should_quit);
        assert_eq!(
            app.input.lines().join(""),
            "draft",
            "typing must not leak into (nor disturb) the prompt textarea"
        );
        assert_eq!(
            state_scroll(&app),
            0,
            "swallowed keys must not move the read position"
        );
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

    // ---- feat_status_line: ^O toggle ---------------------------------------

    /// ^O toggles the status strip, default hidden, round-trip.
    #[test]
    fn ctrl_o_toggles_status_strip_round_trip() {
        let mut app = app_with_chats(1);
        assert!(
            !app.status_strip_visible,
            "strip must start hidden (task contract)"
        );
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(app.status_strip_visible);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(!app.status_strip_visible);
    }

    /// View state like Focus: the strip survives modal open/close, and the
    /// modal branch swallows ^O while a popup is open (no toggle leaks).
    #[test]
    fn ctrl_o_survives_modals_and_is_swallowed_while_one_is_open() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
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
        assert!(app.status_strip_visible);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(!app.status_strip_visible);
    }

    // ---- feat_model_picker_lite: ^M chord + modal isolation -----------------

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
            app.input.input(tui_textarea::Input {
                key: tui_textarea::Key::Char(ch),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
        inject_ready_picker(&mut app, 3);

        // Typing leaks nowhere; global chords and thread switching are
        // swallowed; the mode never moves.
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
        assert!(!app.status_strip_visible);
        assert_eq!(
            app.input.lines().join(""),
            "precious draft",
            "no keystroke leaked into the textarea"
        );
        assert_eq!(app.scroll, 0, "PgUp did not scroll the chat pane");

        // The ONLY working keys: ↑/↓ navigate, Enter confirms, Esc cancels,
        // Ctrl+C quits.
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
            app.input.input(tui_textarea::Input {
                key: tui_textarea::Key::Char(ch),
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
}
