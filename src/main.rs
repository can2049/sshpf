//! `sshpf` — SSH port forward manager with a TUI dashboard.
//!
//! Entry point and process life cycle. This file is deliberately thin: it wires
//! the pieces together and owns the terminal + signal handling, nothing else.
//!
//! # The map
//!
//! ```text
//! main()        logging → args (src/cli.rs) → paths → one of the modes
//! run_tui()     signals → terminal → App → event_loop → shutdown → restore
//! event_loop()  drain events, draw one frame, read one key — repeat until quit
//! ```
//!
//! # The process modes (Unix)
//!
//! One config has exactly one **server**, which owns every tunnel, and any number
//! of **clients**, which render that server's state and send it commands. This is
//! what makes a second terminal show the first one's tunnels instead of fighting
//! it for the same local ports.
//!
//! | invocation | role |
//! |---|---|
//! | `sshpf` | client; starts the server first if none is running |
//! | `sshpf --server` | the server itself (debugging, or a supervisor) |
//! | `sshpf --keep-server` | client; the server survives the last client |
//! | `sshpf --check` / `--list-hosts` | neither — they never touch the socket |
//!
//! The last row matters: turning a print-and-exit mode into a client would start
//! a background server as a side effect of `sshpf --check`.
//!
//! On Windows this collapses to the single process it always was: there is no
//! Unix socket transport there yet (see `docs/research/client-server.md` §10).
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

    #[cfg(unix)]
    if args.server {
        eventlog::cli_mode("server");
        return sshpf::server::run(paths, args.keep_server, args.no_restore, args.host_key_policy());
    }

    match run_tui(paths, &args, started) {
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
///
/// `authoritative` is whether this process owns the tunnels (`--server`, or the
/// single-process Windows path). A **client** must not restore: the `enabled`
/// flag is already restored by the server, and doing it again from every client
/// would restart every tunnel once per attached window.
fn prepare_app(app: &mut App, no_restore: bool, authoritative: bool) {
    if let Some(reason) = logging::init_error() {
        // Silent logging loss is the worst failure mode for a diagnostic feature;
        // the user finds out through the status line instead of an empty file.
        app.set_status(StatusMsg::warn(format!("日志未启用: {reason}")));
        logging::error("log.unavailable", &[("msg", &reason)]);
    }
    app.load_hosts();
    // The load repaired the user's table: entries that meant the same forward were
    // folded into one (see `model::dedup`). A shorter table on start-up with no
    // explanation reads as data loss, so it is said out loud — after `load_hosts`
    // so `--no-restore`'s own notice does not overwrite it.
    //
    // A client skips both: its table is replaced by the server's first snapshot,
    // so announcing a repair of a file it will not display would be a lie.
    if !authoritative {
        return;
    }
    announce_folded(app);
    if no_restore {
        logging::info("app.restore_skipped", &[("reason", &"no-restore")]);
        app.set_status(StatusMsg::info("已跳过自动恢复（--no-restore）"));
    } else {
        app.restore_enabled();
    }
}

/// Say that duplicate saved entries were merged on load, if any were.
///
/// Both halves of the sentence matter: *how many* rows disappeared (the table is
/// visibly shorter) and *which intent they collapsed into* (so the survivor is
/// identifiable in the list). Silent repair would be read as "sshpf lost my
/// forwards" — the opposite of the truth.
fn announce_folded(app: &mut App) {
    if app.folded_on_load.is_empty() {
        return;
    }
    let dropped: usize = app.folded_on_load.iter().map(|g| g.dropped.len()).sum();
    let kept = app.folded_on_load.first().map(|g| g.kept.clone()).unwrap_or_default();
    app.set_status(StatusMsg::warn(sshpf::help::bi(
        &format!("已合并 {dropped} 条重复转发（保留「{kept}」等 {} 条）", app.folded_on_load.len()),
        &format!("Merged {dropped} duplicate forward(s) into {} existing one(s)", app.folded_on_load.len()),
    )));
}

/// Shut the session down in the order that cannot leave a broken terminal behind.
///
/// Tunnels are stopped **before** the terminal is restored, so a stray panic
/// during shutdown cannot strand the user in raw mode with live forwards.
fn shut_down(app: &mut App, mouse: bool, started: Instant, result: std::io::Result<()>) -> ExitCode {
    logging::set_tui_active(false);
    let was_quit = app.should_quit;
    // A client tells the server it is leaving and touches nothing else: the
    // server decides whether this was the last one, and owns every file. `App::shutdown`
    // on a mirror would try to stop tunnels it does not own.
    if app.is_mirror() {
        app.send_bye(was_quit);
        eventlog::app_exit(if was_quit { "user" } else { "signal" }, started.elapsed().as_millis(), 0);
        if mouse {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
        }
        ratatui::restore();
        return if result.is_ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
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
fn run_tui(paths: Paths, args: &cli::Args, started: Instant) -> std::io::Result<ExitCode> {
    // A signal only flips these flags: the loop then exits normally so `Drop`
    // handlers stop the tunnels. `process::exit()` would skip them. `received`
    // records *which* signal arrived — the TUI is gone by the time the exit code
    // is produced, so otherwise Ctrl-C is indistinguishable from a `kill`.
    let quit = Arc::new(AtomicBool::new(false));
    let received = Arc::new(AtomicUsize::new(0));
    install_signal_flags(&quit, &received);

    if config::perms_too_open(&paths.config) {
        // Group/world readable config: it names hosts and key paths, so this is a
        // real (if small) exposure and worth a record either way.
        let ok = config::tighten_perms(&paths.config).is_ok();
        eventlog::config_perms_tightened(&config::display_path(&paths.config), ok);
    }

    let mut app = App::with_host_key_policy(paths.clone(), args.host_key_policy());
    // Attach **before** the terminal is taken, and the order is load-bearing in
    // both directions:
    //
    // * attaching needs no terminal (it is one socket handshake), so a failure
    //   here is a plain stderr message and a normal non-zero exit instead of a
    //   TUI the user has to open to read a status line;
    // * taking the terminal first would make every headless invocation
    //   (`sshpf < /dev/null`, a CI step, `ssh host sshpf` without `-t`) die with
    //   a tty error before it ever tried to attach — the client would never start
    //   the server, and `--check`-style automation would look broken.
    let attached = attach_client(&mut app, &paths, args)?;
    // Terminal now: with no client link to establish, a missing tty is the only
    // thing that can still fail, and it is reported as the environment problem it
    // is (see `take_terminal`).
    let mut terminal = take_terminal()?;
    // From here on the crash handler must release the alternate screen before the
    // message lands in the user's shell.
    logging::set_tui_active(true);
    prepare_app(&mut app, args.no_restore, !attached);

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

/// Turn this process into a client of the server for this config.
///
/// Returns whether it did. `Ok(false)` means "stay authoritative": on Windows,
/// where there is no transport yet, and that is the only remaining case — so the
/// behaviour there is exactly what it was before client-server existed.
///
/// A failure is **not** a fallback to single-process mode. Silently becoming the
/// owner of the tunnels after failing to reach the server is how two processes
/// end up fighting over the same local ports — the very defect this mode removes.
/// So the reason is reported (log + stderr) and the process exits non-zero.
#[cfg(unix)]
fn attach_client(app: &mut App, paths: &Paths, args: &cli::Args) -> std::io::Result<bool> {
    match sshpf::client::attach(paths, args.host_key_policy(), args.keep_server) {
        Ok((out, incoming)) => {
            app.attach_remote(out, incoming);
            logging::info("client.attach", &[("config", &config::display_path(&paths.config))]);
            Ok(true)
        }
        Err(e) => {
            // No terminal has been taken yet, so there is nothing to restore:
            // this message goes straight to the user's shell.
            eventlog::client_attach_failed(&e.zh);
            let _ = writeln!(std::io::stderr(), "错误 / Error: {} / {}", e.zh, e.en);
            Err(std::io::Error::other(format!("{} / {}", e.zh, e.en)))
        }
    }
}

/// Windows: no client-server transport yet, so this process owns the tunnels.
#[cfg(not(unix))]
fn attach_client(_app: &mut App, _paths: &Paths, _args: &cli::Args) -> std::io::Result<bool> {
    Ok(false)
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

    /// A signal this platform lets `signal_hook` register and `raise`, but which
    /// is not one of [`sys::quit_signals`], so the flags flipped here cannot
    /// interfere with the test harness.
    #[cfg(unix)]
    const SCRATCH_SIGNAL: i32 = signal_hook::consts::SIGUSR1;
    /// Windows has no `SIGUSR1`. `SIGABRT` is a real CRT signal that is not in
    /// `quit_signals`; the test registers a handler **before** raising it, so the
    /// default abort action never runs.
    #[cfg(windows)]
    const SCRATCH_SIGNAL: i32 = signal_hook::consts::SIGABRT;

    /// The registration itself must not panic on the platform's own quit signals,
    /// and a registered signal must flip both flags.
    #[test]
    fn a_registered_flag_is_written_and_remembers_the_signal() {
        use std::sync::atomic::AtomicUsize;
        let quit = Arc::new(AtomicBool::new(false));
        let received = Arc::new(AtomicUsize::new(0));
        let sig = SCRATCH_SIGNAL;
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
