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

use chibi_tui::app::{Connection, Mode, ReconnectRequest};
use chibi_tui::backend::{Backend, BackendEvent};
use chibi_tui::{history, mock, splash, theme, ui};

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
    // popup and can be recovered with `R`.
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
        match chibi_tui::LiveBackend::connect(&cli.workspace).await {
            Ok(live) => {
                app.connection = Connection::Connected;
                Source::Live(live)
            }
            Err(e) => {
                app.connection = Connection::Disconnected;
                app.show_error(format!("{e}\n(hint: use --mock for the offline demo mode)"));
                Source::LivePlaceholder
            }
        }
    };

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
        // frame size — no cached geometry anywhere in the render path.
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
                            // message submission.
                            let name_before = app.chat_title();
                            let was_renaming = !app.mode.is_normal();

                            handle_key(&mut app, key);

                            // A rename commit happened iff Enter closed an
                            // open session and the active chat's name changed.
                            // Rejected drafts (empty / whitespace-only) leave
                            // the name untouched and skip persistence.
                            let renamed = was_renaming
                                && key.code == KeyCode::Enter
                                && app.mode.is_normal()
                                && app.chat_title() != name_before;
                            // ANY Enter pressed inside rename mode belongs to
                            // the rename editor — never to message submission
                            // (a rejected save must not leak into the prompt).
                            let enter_consumed_by_rename =
                                was_renaming && key.code == KeyCode::Enter;

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
                            // over submission — its Enter was consumed by the
                            // editor, and the renamed chat is persisted here so
                            // the new title survives restarts.
                            if renamed {
                                persist_chat(&app.chats[app.active], history_dir);
                            } else if enter_consumed_by_rename {
                                // Rejected rename save (empty draft): nothing
                                // to do — old name kept, nothing persisted.
                            } else if app.error_popup.is_none()
                                && source.accepts_submissions()
                                && should_submit(&key)
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
                    Some(Ok(_)) => {}                     // mouse etc.
                    Some(Err(e)) => return Err(io::Error::other(e)),
                    None => {} // stream ended; keep looping until quit
                }
            }

            // ---- backend events ----
            Some(evt) = event_rx.recv() => {
                // Per-thread async: after a chat's terminal event, start its
                // next queued prompt (FIFO) — including for background chats.
                let drain_thread_id = match &evt {
                    BackendEvent::QueueDrain { thread_id } => Some(thread_id.clone()),
                    _ => None,
                };
                if let Some(thread_id) = drain_thread_id {
                    if let Some(next) = app.dequeue_next_for(&thread_id) {
                        send_submitted(source, &next, event_tx.clone());
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
            // disconnect must not panic — surface it as a chat error instead.
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

    // ---- modal error popup captures everything ----
    // (Ctrl+R rename is intentionally unreachable while the popup is open:
    // the popup branch returns before any mode handling.)
    if app.error_popup.is_some() {
        match key.code {
            // Reconnect request — executed by the event loop.
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

    // ---- global quit / cancel semantics ----
    //
    // Ctrl+C cancels the in-flight request when one exists, otherwise quits —
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
                // Chat navigation stays blocked mid-rename: switching the
                // active chat under an open editor would be confusing.
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
            app.clear_input();
            return;
        }
        (KeyCode::Char('u'), true) => {
            // Delete from cursor to start of line. tui-textarea maps Ctrl+U
            // to undo, which surprises readline users — override it here.
            app.input.delete_line_by_head();
            return;
        }
        // Ctrl+N: new chat.
        (KeyCode::Char('n'), true) => {
            app.new_chat();
            return;
        }
        // Ctrl+A / Ctrl+E reach tui-textarea's built-in readline mappings
        // (head/end of line); they fall through untouched below.
        _ => {}
    }

    // Vim-style navigation only when the input buffer is empty.
    let input_is_empty = app.input.lines().iter().all(|l| l.is_empty());

    match key.code {
        KeyCode::Up => app.select_prev(),
        KeyCode::Down => app.select_next(),
        KeyCode::PageUp => app.scroll_up(app.chat_visible_rows),
        KeyCode::PageDown => app.scroll_down(app.chat_visible_rows),
        KeyCode::Esc if !input_is_empty => {
            // Non-empty input: clear it.
            app.clear_input();
        }
        // Esc is ignored when input is empty — it never quits.
        // Only Ctrl+C quits (idle) or cancels (busy).
        // BARE Enter is swallowed here and submitted by the loop's
        // should_submit() gate; Shift+Enter / Alt+Enter fall through to the
        // textarea as newline inserts. On terminals WITHOUT the kitty
        // keyboard protocol, Shift+Enter arrives as bare Enter bytes and
        // degrades to submit — documented in the README.
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            app.input.insert_newline();
        }
        KeyCode::Enter => {}
        // Everything else (including Ctrl+A/E, Alt+B/F, Alt+D word ops)
        // goes into the textarea's readline-compatible handler.
        _ => {
            let converted: tui_textarea::Input = key.into();
            app.input.input(converted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chibi_tui::app::Chat;

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

    /// Chat navigation (↑/↓) is blocked mid-rename so the active chat cannot
    /// silently change under the open editor.
    #[test]
    fn arrows_do_not_switch_chats_while_renaming() {
        let mut app = app_with_chats(3);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.active, 0, "navigation suppressed during rename");
        assert!(!matches!(app.mode, chibi_tui::app::Mode::Normal));
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
}
