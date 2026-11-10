//! Starting the server from the command line: elect, install the signal bridge,
//! serve, and turn the outcome into an exit code.
//!
//! # Why this is not in `main`
//!
//! `main` owns *dispatch* — which mode was asked for — and nothing else. The
//! server's own start-up is a different job with its own rules (a lost election is
//! success, a signal must be turned into an ordered shutdown, the terminal is never
//! touched), so it lives here and `main` calls one function.
//!
//! # The three outcomes, deliberately different
//!
//! | outcome | exit code | why |
//! |---|---|---|
//! | served and stopped cleanly | 0 | the normal path |
//! | lost the election to another server | **0** | opening a second window is designed, not an error |
//! | could not create the endpoint / bind | non-zero | nothing can ever work in this configuration |
//!
//! Belongs here: `--server` start-up and the signal bridge.
//! Does not belong: the accept loop ([`super::run`]) or the CLI parse
//! ([`crate::cli`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::cli::Args;
use crate::config::Paths;
use crate::eventlog;
use crate::ipc;
use crate::logging;
use crate::sys;

use super::{Role, ServerArgs, ShutdownTrigger, become_server, run};

/// Run in `--server` mode and report the outcome as an exit code.
pub fn start(config_path: Paths, args: &Args) -> std::process::ExitCode {
    let endpoint = ipc::endpoint_for(&config_path.config);
    let identity = match become_server(endpoint) {
        Ok(Role::Client) => return already_served(),
        Ok(Role::Server(identity)) => identity,
        Err(e) => return failed(&format!("{e:#}")),
    };

    // The server must react to a quit signal by *returning*, not by dying: a default
    // disposition skips `Drop`, so the tunnels would be left running, the discovery
    // files on disk, and no `server.shutdown_begin` record would explain it. A
    // foreground `--server` is the documented way to debug this feature, so Ctrl-C
    // working is part of the contract.
    let flags = crate::sys::signals::QuitFlags::install();
    spawn_signal_watcher(flags.quit.clone(), flags.received.clone());

    let server_args = ServerArgs {
        keep_server: args.keep_server,
        host_key_policy: args.host_key_policy(),
        trigger: ShutdownTrigger::Process,
    };
    match run(config_path, server_args, *identity) {
        Ok(()) => {
            let signal = flags.signal();
            if signal != 0 {
                logging::info("app.signal", &[("signal", &sys::signal_name(signal))]);
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => failed(&format!("{e:#}")),
    }
}

/// Report that another server already owns this config, and succeed.
///
/// A foreground user asked for `--server` explicitly and deserves an answer on
/// stderr, but the situation is the designed one: opening a second window must not
/// report a failure to the shell.
fn already_served() -> std::process::ExitCode {
    use std::io::Write as _;
    eventlog::server_election_lost("another server owns this config");
    let _ = writeln!(
        std::io::stderr(),
        "已有 server 在运行该配置，本进程退出 / A server already serves this config; exiting"
    );
    std::process::ExitCode::SUCCESS
}

/// Report a start-up failure on stderr and in the log.
fn failed(detail: &str) -> std::process::ExitCode {
    use std::io::Write as _;
    logging::error("server.fatal", &[("msg", &detail)]);
    let _ = writeln!(std::io::stderr(), "server 启动失败 / cannot start the server: {detail}");
    std::process::ExitCode::FAILURE
}

/// Turn a quit signal into a server stop request.
///
/// The signal handler itself may only store to an atomic (logging takes a mutex), so
/// this cheap poll is the bridge: it watches the flag `install_signal_flags` set and
/// calls the server's stop hook once.
///
/// A thread rather than an event loop because the accept loop owns the main thread:
/// the server is headless, and the only thing it waits for besides connections is
/// "someone asked me to stop".
fn spawn_signal_watcher(quit: Arc<AtomicBool>, received: Arc<AtomicUsize>) {
    let spawned = std::thread::Builder::new().name("sshpf-server-signal".to_string()).spawn(move || {
        loop {
            if quit.load(Ordering::Relaxed) {
                // `request_stop_via_hook` records `server.shutdown_begin` with
                // `trigger=signal` and flips the flag the accept loop polls, which
                // is what makes the teardown (and the endpoint cleanup) run.
                let _ = super::request_stop_via_hook();
                let signal = received.load(Ordering::Relaxed);
                if signal != 0 {
                    logging::info("app.signal", &[("signal", &sys::signal_name(signal as i32))]);
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    if let Err(e) = spawned {
        // Without the watcher `Ctrl-C` falls back to the default disposition and
        // kills the process without teardown, leaving the discovery files and
        // possibly the tunnels behind. Worth an ERROR: it changes what `kill` does.
        logging::error("server.signal_watcher_failed", &[("msg", &e.to_string())]);
    }
}
