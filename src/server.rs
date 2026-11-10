//! The server: the one process that owns the tunnels, serves every client, and
//! decides when the whole thing stops.
//!
//! # Why a server at all
//!
//! Tunnels are processes' worth of state: a `Supervisor` full of threads, a retry
//! schedule, a lock on local ports. Two terminals cannot both own them. Making one
//! process the owner and the rest viewers is what lets several windows show the
//! same forwards and edit them without fighting — and it is what makes "the last
//! window closes, everything stops" a rule instead of a race.
//!
//! # Shape
//!
//! ```text
//! main --server ──► run()
//!                     ├── election     抢锁：只有一个 server
//!                     ├── listener     回环 TCP + 端口/令牌文件
//!                     ├── App          **服务器也运行一个 App**
//!                     │                 （权威态、Supervisor、配置文件都在这里）
//!                     └── session*     每个 client 一个线程：握手 → 读命令 → 发快照
//! ```
//!
//! # The decision that makes this small
//!
//! The server runs the *same* [`App`] as the single-process build. It is not a
//! rewrite of the state machine, it is the state machine plus a listener. That is
//! why `App` grew a `mirror` seam instead of being split in two: with the seam,
//! "who owns the truth" is one `Option`, and every key handler, form and render
//! path stays exactly as it was.
//!
//! The consequence to keep in mind: **the server never renders**. It runs the
//! event pump and the retry timer on a plain thread, because those are the parts
//! of `App` that must keep happening with no window open.
//!
//! # Who decides what
//!
//! A client's command is a *request*. The server re-validates it (the id exists,
//! the definition is valid, the port is free) before touching anything, because a
//! client's view can be seconds stale and a stale view must never own a destroy
//! action. That is why `commands` re-checks rather than trusting `expect_rev`
//! alone.
//!
//! # Layout
//!
//! | file | what lives there |
//! |---|---|
//! | [`election`] | the singleton lock, stale discovery files, `--server` start-up |
//! | [`commands`] | turning one `Op` into `App` calls, with server-side validation |
//! | [`state`] | the shared handle sessions use to reach the `App` |
//! | [`broadcast`] | revision bumping and fan-out of one `Snapshot` |
//! | [`session`] | one client: handshake, read loop, bounded send |
//! | [`lifecycle`] | client counting, the grace window, and the shutdown sequence |
//!
//! Belongs here: server-side orchestration.
//! Does not belong: the wire format ([`crate::ipc`]), what a tunnel does
//! ([`crate::tunnel`]), or rendering ([`crate::ui`] — never used here).

pub mod broadcast;
pub mod commands;
pub mod election;
pub mod framing;
pub mod lifecycle;
pub mod session;
pub mod signature;
pub mod start;
pub mod state;

pub use election::{Role, ServerIdentity, become_server};
pub use start::start;
pub use state::request_stop_via_hook;
pub use state::{ServerState, Shared};

use std::net::TcpListener;
use std::time::Duration;

use crate::app::App;
use crate::config::Paths;
use crate::eventlog;
use crate::logging;

/// How long the server waits before retrying after an unexpected listener error.
///
/// Accept errors are almost always transient (a file-descriptor limit, a brief
/// resource exhaustion), and the alternative to waiting is a hot spin.
#[allow(dead_code)]
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// How long the accept loop sleeps between polls when nothing is pending.
///
/// The listener is non-blocking, so the loop must wake on its own to notice a
/// requested shutdown: a blocking `accept()` cannot be interrupted from another
/// thread, which would make "the last client left" a decision the server never
/// gets to act on. 50 ms is invisible to a user and costs almost nothing idle.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Run the foreground server until it is told to stop.
///
/// `identity` is what [`election::become_server`] returned: holding it is what
/// makes this process the only server for this config, and dropping it releases
/// the lock.
pub fn run(paths: Paths, args: ServerArgs, identity: election::ServerIdentity) -> anyhow::Result<()> {
    election::release_stale_endpoint();
    broadcast::reset();

    let mut app = App::with_host_key_policy(paths, args.host_key_policy);
    // The server never takes the terminal: it may be started from a client's
    // terminal, so it must not steal the cursor or leave raw mode behind.
    app.keep_server = args.keep_server;
    app.client_count = 0;

    // Restore the tunnels the user left enabled, exactly once, here.
    //
    // This is the server's job and nobody else's: a client that restored too would
    // race it, and the visible result is every forward bouncing ("connected,
    // dropped, reconnected") the moment a second window opens. `--no-restore` is
    // therefore a server-side flag once a server is in play — see
    // `main::prepare_app`, which skips restore for mirrors for this same reason.
    //
    // Before this ran, the server started with every tunnel `Stopped` until someone
    // pressed `a`: the config says `restore_on_start = true`, the single-process
    // build honoured it, and only the client-server path quietly did not.
    if args.no_restore {
        logging::info("app.restore_skipped", &[("reason", &"no-restore")]);
    } else {
        app.restore_enabled();
    }

    let shared = state::Shared::new(app, args.keep_server, identity.token.clone());
    eventlog::server_started(
        &crate::config::display_path(&identity.endpoint.config_path),
        std::process::id(),
        identity.port,
    );

    spawn_event_pump(shared.clone());
    install_stop_hook(shared.clone());
    // Non-blocking is load-bearing, not an optimisation: the shutdown decision is
    // made on another thread (the grace timer or the signal watcher), and a blocked
    // `accept()` would never let this loop observe it — the server would stay up
    // with zero clients forever, breaking the one rule this feature exists for.
    identity.listener.set_nonblocking(true)?;

    accept_loop(&identity.listener, &shared);

    lifecycle::shutdown(shared, args.trigger);
    election::release_endpoint(&identity);
    Ok(())
}

/// Publish the hook a signal watcher calls to stop this server.
///
/// The watcher runs on its own thread and only knows "stop", so the hook is how it
/// reaches the state without holding a `Shared` of its own.
fn install_stop_hook(shared: state::Shared) {
    state::register_stop_hook(Box::new(move || shared.request_stop("signal")));
}

/// Accept connections until the server is asked to stop.
///
/// Polls instead of blocking so a shutdown decided on another thread is observed:
/// see the note at [`run`].
fn accept_loop(listener: &TcpListener, shared: &state::Shared) {
    while !shared.is_stopping() {
        match listener.accept() {
            Ok((stream, peer)) => accept_one(stream, peer, shared),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                // A failed accept (a connection that reset between poll and accept)
                // is routine; the listener is still usable, so back off and retry.
                logging::debug("server.accept_failed", &[("msg", &e.to_string())]);
                std::thread::sleep(ACCEPT_BACKOFF);
            }
        }
    }
}

/// Hand one accepted socket to a session thread.
///
/// Whether the client may be *counted* is decided inside the handshake, after the
/// token is verified: a connection refused for a bad secret must never have
/// incremented `client_count`, or anyone probing the port could keep the server
/// alive.
fn accept_one(stream: std::net::TcpStream, peer: std::net::SocketAddr, shared: &state::Shared) {
    // Each accepted socket must block again for its own session: the non-blocking
    // flag is inherited from the listener on some platforms.
    if stream.set_nonblocking(false).is_err() {
        eventlog::server_connection_refused(&format!("{peer}: cannot make the socket blocking"));
        return;
    }
    session::spawn(stream, peer, shared.clone());
}

/// Run the `App` event pump and retry timer without a UI.
///
/// This is the half of `App` that must keep running when no window is open: a
/// tunnel that drops must still reconnect, and a `Ready` event must still move the
/// row to `Up` so every client sees it. It is the same `drain_events` the TUI
/// calls each frame; only the interval differs.
fn spawn_event_pump(shared: state::Shared) {
    let handle = std::thread::Builder::new().name("sshpf-server-pump".to_string()).spawn(move || {
        while !shared.is_stopping() {
            shared.with_app(|app| app.drain_events());
            shared.publish_if_changed();
            std::thread::sleep(state::PUMP_INTERVAL);
        }
    });
    if let Err(e) = handle {
        // Without the pump, tunnels never reconnect and clients never see changes,
        // so the server must not pretend to keep working.
        logging::error("server.pump_failed", &[("msg", &e.to_string())]);
    }
}

/// What `main` passes into the server beyond the shared paths.
#[derive(Debug, Clone, Copy)]
pub struct ServerArgs {
    /// Stay up after the last client leaves, instead of shutting down.
    pub keep_server: bool,
    /// The host-key policy from the command line.
    pub host_key_policy: crate::ssh::known_hosts::HostKeyPolicy,
    /// Why this server is stopping (recorded in `server.shutdown_begin`).
    pub trigger: ShutdownTrigger,
    /// Do not re-open the tunnels the config says were enabled.
    ///
    /// Honoured here rather than in the client, because the server is the only
    /// process that may start a tunnel. A client that asked for `--no-restore` gets
    /// it through the spawn arguments — see [`crate::client::spawn::SpawnRequest`].
    pub no_restore: bool,
}

/// Why the server is being asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownTrigger {
    /// The process reached its natural end.
    Process,
    /// A quit signal arrived (Ctrl-C in a foreground `--server`).
    Signal,
}

impl ShutdownTrigger {
    pub fn name(self) -> &'static str {
        match self {
            ShutdownTrigger::Process => "process",
            ShutdownTrigger::Signal => "signal",
        }
    }
}

/// The listener type the election hands out once it is bound and published.
pub type BoundListener = TcpListener;

/// Ask the running server (if any) to stop, from a signal watcher thread.
///
/// Public so `main` can wire a signal flag to it without knowing how the server
/// stores its stop hook.
pub fn signal_stop_handle() -> fn() -> bool {
    state::request_stop_via_hook
}
