//! `sshpf` — SSH port forward manager with a TUI dashboard.
//!
//! Entry point and process life cycle. This file is deliberately thin: it wires
//! the pieces together and owns the terminal + signal handling, nothing else.
//!
//! # The map
//!
//! ```text
//! main()        logging → args (src/cli.rs) → paths → one of the three modes
//! run_tui()     signals → terminal → App → event_loop → shutdown → restore
//! event_loop()  drain events, draw one frame, read one key — repeat until quit
//! ```
//!
//! What lives where:
//!
//! * **`src/cli.rs`** — argument parsing, `USAGE`, `--list-hosts` / `--check`.
//! * **`crate::app`** — all state and every state transition. Nothing in this file
//!   touches a tunnel directly.
//! * **`crate::ui`** — all drawing. `event_loop` only calls `ui::draw`.
//! * **`crate::keys`** — all key routing. `event_loop` only hands the event over.
//! * **`crate::logging`** — every record; `src/eventlog.rs` names the events.
//!
//! # Signals
//!
//! A signal only flips an atomic flag; the loop then exits *normally* so `Drop`
//! handlers stop the child processes. `process::exit()` would skip them. Which
//! signals exist differs per platform, so [`sshpf::sys::quit_signals`] decides.
//!
//! Registration uses `signal-hook`'s safe API (`flag::register` /
//! `flag::register_usize`), which internally does a single atomic store — exactly
//! what a signal handler is allowed to do, and no `unsafe` here.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use ratatui::DefaultTerminal;

use sshpf::app::App;
use sshpf::config::{self, Paths};
use sshpf::eventlog;
use sshpf::logging;
use sshpf::model::StatusMsg;
use sshpf::{cli, keys, sys, ui};

/// Poll interval: how often keys/events are checked and the UI redraws.
const TICK: Duration = Duration::from_millis(200);

fn main() -> ExitCode {
    // Logging first: everything below, including a failed `--config` parse, is
    // more useful with a record of it. A failure here is not fatal — sshpf works
    // without a log — but it is reported once the TUI is up (see `run_tui`).
    let log_path = logging::init_from_env().ok();
    logging::install_crash_hooks();
    let started = Instant::now();

    let args = match cli::parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eventlog::cli_error("cli.bad_args", &e);
            let _ = writeln!(std::io::stderr(), "{e}");
            return ExitCode::from(2);
        }
    };

    let paths = cli::paths_for(&args);
    eventlog::app_start(
        env!("CARGO_PKG_VERSION"),
        &config::display_path(&paths.config),
        &log_path.as_ref().map(|p| config::display_path(p)).unwrap_or_default(),
    );

    if args.list_hosts {
        eventlog::cli_mode("list_hosts");
        return cli::list_hosts();
    }
    if args.check {
        eventlog::cli_mode("check");
        return cli::check_config(&paths);
    }

    match run_tui(paths, args.no_restore, started) {
        Ok(code) => code,
        Err(e) => {
            // The TUI could not even start (terminal init, event loop I/O) — the
            // one failure with no UI left to explain it.
            logging::error("app.fatal", &[("stage", &"tui"), ("msg", &e.to_string())]);
            let _ = writeln!(std::io::stderr(), "错误: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Register the quit signals without any `unsafe` code.
///
/// Two things must happen per signal, and both are async-signal-safe (one atomic
/// store each — nothing else may run in a handler here, because logging takes a
/// mutex):
///
/// 1. flip the quit flag, so the event loop exits normally and every `Drop` runs
///    (`process::exit()` would skip them, leaving tunnels up and the terminal in
///    raw mode);
/// 2. remember *which* signal arrived, because the TUI is gone by the time the
///    exit status is produced and an exit code of 0 cannot tell Ctrl-C from `kill`.
///
/// `signal_hook::flag` is the safe, signal-safe wrapper for exactly this shape, so
/// no `unsafe` block is needed. Which signals exist differs per OS (no `SIGHUP` on
/// Windows); [`sshpf::sys::quit_signals`] knows.
fn install_signal_flags(quit: &Arc<AtomicBool>, received: &Arc<AtomicUsize>) {
    for sig in sshpf::sys::quit_signals() {
        let sig = *sig;
        let registered = signal_hook::flag::register_usize(sig, received.clone(), sig as usize)
            .and_then(|_| signal_hook::flag::register(sig, quit.clone()));
        if let Err(e) = registered {
            // Swallowing this used to be a silent trap: Ctrl-C would kill the
            // process without running any `Drop`, leaving the tunnels unstopped and
            // the terminal in raw mode. Not fatal — the TUI still runs and `q`
            // quits — but the user deserves to know which shortcut is gone.
            logging::warn("app.signal_unavailable", &[("signal", &sys::signal_name(sig)), ("msg", &e.to_string())]);
        }
    }
}

/// Take the terminal, or report why it could not be taken without panicking.
///
/// `ratatui::init()` is `try_init().expect("failed to initialize terminal")`, so
/// it **panics** when there is no tty (`sshpf < /dev/null`, a CI runner,
/// `ssh host sshpf` without a pty, a Windows process with no console) — bypassing
/// the `app.fatal` handling that exists for exactly that case. `try_init` returns
/// the error instead, so it reaches `main` as an `io::Error`, is recorded, and
/// exits 1 instead of 101.
///
/// `try_init` can fail *after* it has already put the terminal in raw mode (its
/// steps are: install hook → enable raw mode → enter the alternate screen → build
/// the terminal), leaving a partial state the next shell would inherit. So undo
/// it explicitly; `restore` is best effort and never panics.
///
/// `try_init` installs ratatui's own panic hook (which restores the screen)
/// *after* ours, so the two chain: ratatui restores the screen and then ours
/// writes `crash.panic`. That ordering is what the crash tests rely on.
fn take_terminal() -> std::io::Result<ratatui::DefaultTerminal> {
    match ratatui::try_init() {
        Ok(terminal) => Ok(terminal),
        Err(e) => {
            // Only undo state we actually created: `is_raw_mode_enabled` is the
            // observable proof that `try_init` got past its first step. With no
            // tty at all it fails on that step, and emitting escape sequences at
            // a redirected stdout would corrupt whatever is reading it.
            if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
                ratatui::restore();
            }
            Err(e)
        }
    }
}

/// The per-session setup that must happen before the first frame.
fn prepare_app(app: &mut App, no_restore: bool) {
    if let Some(reason) = logging::init_error() {
        // Silent logging loss is the worst failure mode for a diagnostic feature;
        // the user finds out through the status line instead of an empty file.
        app.set_status(StatusMsg::warn(format!("日志未启用: {reason}")));
        logging::error("log.unavailable", &[("msg", &reason)]);
    }
    app.load_hosts();
    if no_restore {
        logging::info("app.restore_skipped", &[("reason", &"no-restore")]);
        app.set_status(StatusMsg::info("已跳过自动恢复（--no-restore）"));
    } else {
        app.restore_enabled();
    }
}

/// Shut the session down in the order that cannot leave a broken terminal behind.
///
/// Tunnels are stopped **before** the terminal is restored, so a stray panic
/// during shutdown cannot strand the user in raw mode with live forwards.
fn shut_down(app: &mut App, mouse: bool, started: Instant, result: std::io::Result<()>) -> ExitCode {
    logging::set_tui_active(false);
    let was_quit = app.should_quit;
    app.shutdown();
    eventlog::app_exit(
        if was_quit { "user" } else { "signal" },
        started.elapsed().as_millis(),
        app.tunnels.iter().filter(|t| matches!(t.status, sshpf::model::TunnelStatus::Up { .. })).count(),
    );
    if mouse {
        // Releasing capture matters more than the return value: leaving it on
        // makes the shell that regains control print garbage on every move.
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    // `restore` prints to stderr (never panics) if the terminal cannot be put
    // back; the event loop's own `io::Result` is what decides the exit code.
    ratatui::restore();
    if result.is_ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Run the TUI until the user quits, a quit signal arrives, or the terminal fails.
fn run_tui(paths: Paths, no_restore: bool, started: Instant) -> std::io::Result<ExitCode> {
    // A signal only flips these flags: the loop then exits normally so `Drop`
    // handlers stop the tunnels. `process::exit()` would skip them. `received`
    // records *which* signal arrived — the TUI is gone by the time the exit code
    // is produced, so otherwise Ctrl-C is indistinguishable from a `kill`.
    let quit = Arc::new(AtomicBool::new(false));
    let received = Arc::new(AtomicUsize::new(0));
    install_signal_flags(&quit, &received);

    // Terminal first, tunnels second: if there is no tty there is nothing to
    // clean up yet (see `take_terminal`).
    let mut terminal = take_terminal()?;
    // From here on the crash handler must release the alternate screen before the
    // message lands in the user's shell.
    logging::set_tui_active(true);

    if config::perms_too_open(&paths.config) {
        // Group/world readable config: it names hosts and key paths, so this is a
        // real (if small) exposure and worth a record either way.
        let ok = config::tighten_perms(&paths.config).is_ok();
        eventlog::config_perms_tightened(&config::display_path(&paths.config), ok);
    }

    let mut app = App::new(paths);
    prepare_app(&mut app, no_restore);

    // Mouse capture is on for the whole session: the import table's filtering and
    // the table's row selection are click-driven, and terminals cannot report a
    // click without it. It is best-effort — a terminal that refuses capture (some
    // multiplexers, some CI ptys) must still give a working keyboard-only UI, so
    // a failure is ignored rather than fatal.
    let mouse = execute!(std::io::stdout(), EnableMouseCapture).is_ok();
    if !mouse {
        eventlog::mouse_capture_failed();
    }

    let result = event_loop(&mut terminal, &mut app, &quit);
    let signal = received.load(Ordering::Relaxed);
    if signal != 0 {
        logging::info("app.signal", &[("signal", &sys::signal_name(signal as i32))]);
    }
    Ok(shut_down(&mut app, mouse, started, result))
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, quit: &Arc<AtomicBool>) -> std::io::Result<()> {
    while !quit.load(Ordering::Relaxed) {
        let frame_started = Instant::now();
        app.drain_events();
        terminal.draw(|frame| ui::draw(frame, app))?;
        // A frame that overruns the tick is the first symptom of a wedged UI
        // (a blocking call sneaking into the event pump). Logging it turns "the
        // TUI felt stuck" into a timestamp.
        let frame_ms = frame_started.elapsed().as_millis();
        if frame_ms >= logging::SLOW_FRAME_MS {
            eventlog::slow_frame(frame_ms);
        }
        if app.should_quit {
            logging::info("app.quit", &[("reason", &"user")]);
            break;
        }
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => keys::handle_key(app, key),
                // Mouse events are dispatched through the hit map the renderer
                // recorded on the frame just drawn, so `App` resolves the target
                // without `main` knowing anything about overlays or layout.
                Event::Mouse(m) => keys::handle_mouse(app, m),
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `signal_name` must never panic for an out-of-range number: it is used in
    /// the log line for a signal we do not know.
    #[test]
    fn signal_name_is_total() {
        for sig in [i32::MIN, -1, 0, 9, 15, i32::MAX] {
            let name = sys::signal_name(sig);
            assert!(!name.is_empty(), "sig {sig} produced an empty name");
        }
    }

    /// The registration itself must not panic on the platform's own quit signals,
    /// and a registered signal must flip both flags. Uses `SIGUSR1` (never in
    /// `quit_signals`) so the assertion cannot interfere with the test harness.
    #[test]
    fn a_registered_flag_is_written_and_remembers_the_signal() {
        use std::sync::atomic::AtomicUsize;
        let quit = Arc::new(AtomicBool::new(false));
        let received = Arc::new(AtomicUsize::new(0));
        let sig = signal_hook::consts::SIGUSR1;
        assert!(
            signal_hook::flag::register_usize(sig, received.clone(), sig as usize).is_ok(),
            "flag::register_usize must accept a real signal"
        );
        assert!(signal_hook::flag::register(sig, quit.clone()).is_ok());
        // Raise it at ourselves and give the dispatcher a moment.
        let _ = signal_hook::low_level::raise(sig);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && received.load(Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(received.load(Ordering::Relaxed), sig as usize, "the signal number must be remembered");
        assert!(quit.load(Ordering::Relaxed), "the quit flag must be set");
    }
}
