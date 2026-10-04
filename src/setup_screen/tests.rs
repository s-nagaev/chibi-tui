//! Tests for the onboarding setup screen: static content contracts, gate
//! rules, ladder-branch content, and the preserved quit-confirm grammar.

use super::*;
use crate::theme::Theme;
use ratatui::{backend::TestBackend, Terminal};
use std::path::PathBuf;

/// Flatten styled lines into one plain string for substring checks.
fn flat(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn unix_content_lists_pipx_and_venv_commands() {
    let text = flat(&content_lines(TargetOs::Unix, &Theme::tokyo_night()));
    assert!(text.contains("not found on PATH"), "got: {text}");
    assert!(text.contains("pipx install chibi-bot"));
    assert!(text.contains("python3 -m venv ~/.local/share/chibi-tui/venv"));
    assert!(text.contains("~/.local/share/chibi-tui/venv/bin/pip install chibi-bot"));
    assert!(text.contains("export CHIBI_BACKEND_BIN=~/.local/share/chibi-tui/venv/bin/chibi"));
    assert!(text.contains("export CHIBI_BACKEND_BIN=/path/to/chibi"));
}

#[test]
fn windows_content_lists_py_and_venv_commands() {
    let text = flat(&content_lines(TargetOs::Windows, &Theme::tokyo_night()));
    assert!(text.contains(r"py -3 -m venv %USERPROFILE%"), "got: {text}");
    assert!(text.contains(r"\Scripts\pip install chibi-bot"));
    assert!(text.contains(r"set CHIBI_BACKEND_BIN=%USERPROFILE%"));
    assert!(text.contains(r"set CHIBI_BACKEND_BIN=C:\path\to\chibi.exe"));
    // pipx works on Windows too, so it stays on that page as well.
    assert!(text.contains("pipx install chibi-bot"));
}

#[test]
fn both_variants_carry_the_backend_bin_escape_hatch() {
    for os in [TargetOs::Unix, TargetOs::Windows] {
        let text = flat(&content_lines(os, &Theme::tokyo_night()));
        assert!(text.contains("Already have a backend?"), "os: {os:?}");
        assert!(text.contains("CHIBI_BACKEND_BIN"), "os: {os:?}");
    }
}

#[test]
fn hint_names_retry_and_quit_keys() {
    let text = flat(&content_lines(TargetOs::Unix, &Theme::tokyo_night()));
    assert!(text.contains("r retry"));
    assert!(text.contains("q/Esc quit"));
}

#[test]
fn applies_only_to_spawn_errors_without_an_override() {
    let spawn = BackendError::Spawn {
        program: "chibi".to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
    };
    let exec_failed = BackendError::UnexpectedExit {
        status: crate::backend_client::ExitStatusDisplay(1),
    };
    assert!(applies(&spawn, None));
    assert!(!applies(&spawn, Some(OsStr::new("/bin/false"))));
    // A binary that exists but fails right after exec is a different
    // story: the popup must keep the real error text.
    assert!(!applies(&exec_failed, None));
}

#[test]
fn backend_override_prefers_env_then_existing_persisted_path() {
    // Explicit env wins, always.
    assert_eq!(
        backend_override_with(Some(OsStr::new("/explicit/chibi")), None),
        Some(OsString::from("/explicit/chibi"))
    );
    // Persisted path counts as an override only while the binary exists.
    let live = std::env::temp_dir().join(format!("chibi-tui-gate-{}", uuid::Uuid::new_v4()));
    std::fs::write(&live, b"").unwrap();
    assert_eq!(
        backend_override_with(None, Some(&live)),
        Some(live.clone().into_os_string())
    );
    // A persisted path whose binary is gone must NOT suppress the screen:
    // the reinstall flow owns that case.
    assert_eq!(
        backend_override_with(None, Some(Path::new("/no/such/chibi"))),
        None
    );
    assert_eq!(backend_override_with(None, None), None);
    // Empty env value counts as unset.
    assert_eq!(
        backend_override_with(Some(OsStr::new("")), Some(&live)),
        Some(live.into_os_string())
    );
}

/// The quit-confirm overlay renders over the setup screen without
/// panicking and carries the shared popup text (same renderer and
/// grammar as the in-app confirm).
#[test]
fn quit_confirm_overlay_renders_over_the_setup_screen() {
    let theme = Theme::tokyo_night();
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
    terminal.draw(|f| draw(f, &theme, true)).unwrap();
    let buf = terminal.backend().buffer();
    let flat: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(
        flat.contains("Quit chibi-tui?"),
        "the confirm popup must overlay the setup screen"
    );
    assert!(
        flat.contains("not found on PATH"),
        "setup content stays underneath"
    );
}

#[test]
fn offer_content_names_the_install_key_and_rung() {
    let text = flat(&offer_lines(
        TargetOs::Unix,
        &Theme::tokyo_night(),
        Rung::Pipx,
        None,
    ));
    assert!(text.contains("i  install chibi-bot now"), "got: {text}");
    assert!(text.contains("pipx"));
    assert!(text.contains("i install"));
    // The static manual hints stay available on the same page.
    assert!(text.contains("pipx install chibi-bot"));
}

#[test]
fn offer_note_surfaces_cancelled_installs() {
    let text = flat(&offer_lines(
        TargetOs::Unix,
        &Theme::tokyo_night(),
        Rung::Venv,
        Some("Install cancelled."),
    ));
    assert!(text.contains("Install cancelled."), "got: {text}");
}

#[test]
fn no_python_content_names_platform_hints_and_reprobe() {
    let unix = flat(&no_python_lines(TargetOs::Unix, &Theme::tokyo_night()));
    assert!(unix.contains("No usable Python 3.11+"), "got: {unix}");
    assert!(unix.contains("brew install python@3.12"));
    assert!(unix.contains("p re-probe"));
    let win = flat(&no_python_lines(TargetOs::Windows, &Theme::tokyo_night()));
    assert!(
        win.contains("winget install Python.Python.3.12"),
        "got: {win}"
    );
}

#[test]
fn failed_content_warns_loudly_before_break_system_packages() {
    let theme = Theme::tokyo_night();
    let with_bsp = flat(&failed_lines(
        &theme,
        Rung::Venv,
        "pip exited 1",
        &["ERROR: Could not find a version".to_owned()],
        Some(Rung::BreakSystemPackages),
    ));
    assert!(
        with_bsp.contains("--break-system-packages"),
        "got: {with_bsp}"
    );
    assert!(with_bsp.contains("WARNING"));
    assert!(with_bsp.contains("not recommended"));
    assert!(with_bsp.contains("n next option"));

    let without_next = flat(&failed_lines(
        &theme,
        Rung::BreakSystemPackages,
        "pip exited 1",
        &[],
        None,
    ));
    assert!(
        without_next.contains("No further automatic options"),
        "got: {without_next}"
    );
}

#[test]
fn installing_content_shows_spinner_tail_and_cancel_keys() {
    let theme = Theme::tokyo_night();
    let mut log = LogTail::new(50);
    log.push("Collecting chibi-bot");
    log.push("  Downloading chibi_bot-1.18.0-py3-none-any.whl (2.1 MB)");
    let text = flat(&installing_lines(&theme, Rung::Pipx, &log, 3));
    assert!(
        text.contains("Installing chibi-bot via pipx"),
        "got: {text}"
    );
    assert!(text.contains("Collecting chibi-bot"));
    assert!(text.contains("Downloading chibi_bot-1.18.0"));
    assert!(text.contains("Esc cancel install"));
    assert!(text.contains("Ctrl+C quit"));

    let empty = flat(&installing_lines(&theme, Rung::Venv, &LogTail::new(4), 0));
    assert!(empty.contains("Starting installer"), "got: {empty}");
}

#[test]
fn installed_content_names_the_verified_binary() {
    let text = flat(&installed_lines(
        &Theme::tokyo_night(),
        Path::new("/home/u/.local/pipx/venvs/chibi-bot/bin/chibi"),
    ));
    assert!(text.contains("installed and verified"), "got: {text}");
    assert!(text.contains("/home/u/.local/pipx/venvs/chibi-bot/bin/chibi"));
}

/// Every state renders without panicking on a small TestBackend and keeps
/// the quit overlay composable.
#[test]
fn all_screen_states_render_without_panic() {
    let theme = Theme::tokyo_night();
    let mut terminal = Terminal::new(TestBackend::new(90, 34)).unwrap();
    let log = Arc::new(Mutex::new(LogTail::new(40)));
    let states: Vec<Screen> = vec![
        Screen::Intro,
        Screen::Probing,
        Screen::Offer {
            rung: Rung::Pipx,
            note: None,
        },
        Screen::NoPython,
        Screen::Installing { rung: Rung::Venv },
        Screen::Failed {
            rung: Rung::Venv,
            reason: "pip exited 1".to_owned(),
            next: Some(Rung::PipUser),
        },
        Screen::Installed {
            binary: PathBuf::from("/usr/local/bin/chibi"),
        },
    ];
    for s in &states {
        terminal
            .draw(|f| draw_state(f, s, &theme, false, Some(&log), 1))
            .unwrap();
    }
    // And with the quit overlay + an empty log.
    terminal
        .draw(|f| draw_state(f, &states[0], &theme, true, None, 0))
        .unwrap();
}
