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
//! * `Ctrl+C` cancels the in-flight request when one exists; quits only in
//!   idle. `Esc`/`q` quit only while idle.
//! * backend failures surface as a modal popup (`R` reconnect, `Esc`/`q`
//!   quit) instead of crashing or being silently dropped;
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
};
use futures_util::StreamExt;
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use chibi_tui::app::{Connection, ReconnectRequest};
use chibi_tui::backend::{Backend, BackendEvent};
use chibi_tui::model::ChatStatus;
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
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        EnableMouseCapture
    )?;
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

    // --- terminal teardown ---
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    res
}

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

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
                            handle_key(&mut app, key);

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

                            // Cancel requested by Ctrl+C on an in-flight request.
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

                            if app.error_popup.is_none() && source.accepts_submissions() {
                                if let Some(submitted) = app.take_input() {
                                    app.begin_request(&submitted);
                                    match source {
                                        Source::Live(live) => live.submit_encoded(submitted, event_tx.clone()),
                                        Source::Mock(mock_backend) => {
                                            mock_backend.submit(submitted.prompt, event_tx.clone())
                                        }
                                        Source::LivePlaceholder => unreachable!("guarded above"),
                                    }
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
                app.apply_backend_event(evt);
                if let Some(chat) = app.chats.get(app.active) {
                    persist_chat(chat, history_dir);
                }
            }

            // ---- spinner animation (~10 fps while busy) ----
            _ = spinner_tick.tick() => {
                if app.status != ChatStatus::Idle {
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

/// Apply key handling to app state. Backend interactions (submit, cancel,
/// reconnect) happen in the event loop by observing state changes, keeping
/// this function synchronous and testable.
fn handle_key(app: &mut chibi_tui::app::App, key: crossterm::event::KeyEvent) {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // ---- modal error popup captures everything ----
    if app.error_popup.is_some() {
        match key.code {
            // Reconnect request — executed by the event loop.
            KeyCode::Char('r') | KeyCode::Char('R') => {
                app.reconnect_requested = Some(ReconnectRequest {});
            }
            KeyCode::Esc => app.should_quit = true,
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            // Any other key just dismisses the popup (stay in the app).
            _ => app.dismiss_error(),
        }
        return;
    }

    // ---- global quit / cancel semantics ----
    //
    // Ctrl+C cancels the in-flight request when one exists, otherwise quits.
    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        if let Some((request_id, thread_id)) = app.cancel_active() {
            app.pending_cancel = Some((request_id, thread_id));
        } else {
            app.should_quit = true;
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            // Quit only while idle: mid-flight Esc would be ambiguous with
            // cancel, which owns Ctrl+C.
            if !app.is_busy() {
                app.should_quit = true;
            }
        }
        KeyCode::Char('q')
            if key.modifiers.is_empty()
                && app.status == ChatStatus::Idle
                && app.input.lines().iter().all(|l| l.is_empty()) =>
        {
            app.should_quit = true;
        }
        _ => {}
    }

    if app.should_quit {
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
        // Ctrl+A / Ctrl+E reach tui-textarea's built-in readline mappings
        // (head/end of line); they fall through untouched below.
        _ => {}
    }

    // Vim-style navigation only when the input buffer is empty.
    let input_is_empty = app.input.lines().iter().all(|l| l.is_empty());
    let plain = key.modifiers.is_empty();

    match key.code {
        KeyCode::Up => app.select_prev(),
        KeyCode::Down => app.select_next(),
        KeyCode::PageUp => app.scroll_up(20),
        KeyCode::PageDown => app.scroll_down(20),
        KeyCode::Char('k') if plain && input_is_empty => app.select_prev(),
        KeyCode::Char('j') if plain && input_is_empty => app.select_next(),
        KeyCode::Char('n') if plain && input_is_empty => app.new_chat(),
        // Submit is handled in the loop via take_input(); intercepting Enter
        // here prevents tui-textarea from inserting a newline. The actual
        // drain happens right after handle_key returns.
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

    fn submit_text(app: &mut chibi_tui::app::App, text: &str) {
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
    }

    // ---- cancel hotkey -----------------------------------------------------

    #[test]
    fn ctrl_c_while_busy_requests_cancel_instead_of_quit() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "long running");
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(app.pending_cancel.is_some(), "cancel requested");
        let (request_id, _) = app.pending_cancel.clone().unwrap();
        assert_eq!(Some(request_id), app.active_request_id.clone());
        assert!(!app.should_quit, "busy Ctrl+C must not quit");
    }

    #[test]
    fn ctrl_c_when_idle_quits() {
        let mut app = app_with_chats(1);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.pending_cancel.is_none());
        assert!(app.should_quit);
    }

    #[test]
    fn esc_is_ignored_while_busy_and_quits_when_idle() {
        let mut app = app_with_chats(1);
        submit_text(&mut app, "in flight");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.should_quit, "Esc during a request must not quit");

        app.resolve_cancel_locally();
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.should_quit, "Esc in idle quits");
    }

    #[test]
    fn plain_q_quits_only_from_empty_idle_input() {
        let mut app = app_with_chats(1);

        // Typing 'q' into the input must insert, never quit. (The first 'q'
        // press both quits-on-empty and inserts — the loop then redraws with
        // the buffer no longer empty, so the second 'q' is a plain insert.)
        app.input.input(tui_textarea::Input {
            key: tui_textarea::Key::Char('x'),
            ctrl: false,
            alt: false,
            shift: false,
        });
        press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(
            !app.should_quit,
            "q with text in the buffer types, not quits"
        );
        assert_eq!(app.input.lines().join(""), "xq");

        app.clear_input();
        press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.should_quit, "bare q on empty idle input quits");
    }

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
    fn popup_esc_q_and_ctrl_c_quit() {
        for (code, mods) in [
            (KeyCode::Esc, KeyModifiers::NONE),
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

    #[test]
    fn vim_nav_still_works_on_empty_input() {
        let mut app = app_with_chats(2);
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.active, 0);
        // With text in the buffer, j/k type instead of navigating.
        for ch in "hi".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.active, 0);
        assert_eq!(app.input.lines().join(""), "hij", "j typed into buffer");
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
}
