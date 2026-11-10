//! Multi-*process* tests for the client/server split: the real binary is started
//! as a server, and raw `TcpStream` clients speak the wire protocol to it.
//!
//! # Why processes and not threads
//!
//! Every rule this feature adds is a rule about *separate processes*: the server
//! publishes an endpoint other processes discover, it counts connections nobody
//! else can count, it survives its clients dying, and two servers must never both
//! win the same config. A test that called `server::run()` on a thread would share
//! the process with the client and could not observe the single most important
//! property — that killing or dropping a socket is visible to a *different* OS
//! process. So these drive `CARGO_BIN_EXE_sshpf`, exactly like `tests/logging.rs`.
//!
//! # Isolation
//!
//! Each test owns a `TempEnv`: a temp dir holding `config.toml`, an empty IPC dir
//! and an empty log dir, all three handed to the child through `SSHPF_CONFIG`,
//! `SSHPF_IPC_DIR` and `SSHPF_LOG_DIR`. Nothing is shared between tests, so the
//! suite is safe to run in parallel; CI runs it with `--test-threads=1` anyway
//! because the timings here are wall-clock based.
//!
//! # What is deliberately asserted
//!
//! | test | the rule it proves |
//! |---|---|
//! | [`the_server_publishes_a_port_and_token_and_a_client_can_attach`] | discovery works and the handshake delivers authoritative state |
//! | [`a_client_with_the_wrong_token_is_refused`] | the token, not the port, is the permission boundary |
//! | [`two_clients_see_the_same_snapshot_revision`] | clients converge on one server revision |
//! | [`one_client_leaving_does_not_stop_the_server`] | one window closing does not stop everyone's forwards |
//! | [`the_last_client_leaving_stops_the_server`] | the last goodbye *does* stop them, and cleans up |
//! | [`an_abrupt_disconnect_keeps_the_server_up_for_the_grace_window_then_exits`] | a crashed window gets a grace window |
//! | [`a_new_client_during_the_grace_window_cancels_the_shutdown`] | reopening inside that window keeps the tunnels up |
//! | [`only_one_server_can_hold_the_config`] | the singleton lock, across processes |
//! | [`keep_server_survives_its_last_client`] | `--keep-server` opts out of "last one out turns off the lights" |
//! | [`check_mode_never_starts_a_server`] | `--check` stays a pure read |
//!
//! # Timeouts, not sleeps
//!
//! Every wait is a poll with a deadline (`wait_until`, `exited_within`), so a slow
//! CI machine makes a test slower rather than flaky. The only unconditional sleeps
//! are the ones whose *purpose* is to let a window pass (the grace-window tests).
//!
//! # The server must not own a terminal
//!
//! `ServerGuard` starts the server with stdin on `NUL` and stdout/stderr on files.
//! A server that tried to take the terminal would panic here instead of silently
//! corrupting the user's shell, which is the bug this arrangement makes visible.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sshpf::ipc::{self, ClientMsg, Endpoint, PROTO_VERSION, ServerMsg, Snapshot};

/// The binary under test.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sshpf")
}

/// How long one read waits before the caller gets [`Next::Idle`] back.
///
/// A whole frame is a few hundred bytes on loopback, so a partial frame that takes
/// this long means the peer is gone rather than slow.
const READ_SLICE: Duration = Duration::from_secs(1);

/// Turn a wire path into the form both sides agree on.
///
/// The server answers with the path it resolved; the client asked with the path it
/// was given. Comparing the normalised forms asserts "these name the same file"
/// without pinning the exact spelling, which is what the handshake's config check
/// itself does.
fn same_path(got: &str, expected: &str) -> bool {
    ipc::endpoint::normalize(Path::new(got)) == ipc::endpoint::normalize(Path::new(expected))
}

// ---------------------------------------------------------------------------
// isolation
// ---------------------------------------------------------------------------

/// Serialises the one process-global mutation this file performs.
///
/// `ipc::endpoint_for` reads `SSHPF_IPC_DIR` from the environment, and an endpoint
/// is per-test, so the variable is set, read and restored under this lock. Children
/// always get their own copy through `Command::env`, so only the derivation itself
/// needs serialising.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Derive an endpoint against `dir`, whatever the test harness's own environment is.
fn endpoint_in(dir: &Path, config: &Path) -> Endpoint {
    let _guard = env_lock();
    let previous = std::env::var_os("SSHPF_IPC_DIR");
    // SAFETY: every mutation of `SSHPF_IPC_DIR` in this process goes through this
    // lock, and the previous value is restored before the guard is dropped.
    unsafe { std::env::set_var("SSHPF_IPC_DIR", dir) };
    let endpoint = ipc::endpoint_for(config);
    match previous {
        Some(value) => {
            // SAFETY: as above, still under the lock.
            unsafe { std::env::set_var("SSHPF_IPC_DIR", value) };
        }
        None => {
            // SAFETY: as above, still under the lock.
            unsafe { std::env::remove_var("SSHPF_IPC_DIR") };
        }
    }
    endpoint
}

/// A per-test sandbox: config file, IPC directory, log directory.
struct TempEnv {
    /// Held for its `Drop`: deleting the directory is the cleanup.
    dir: tempfile::TempDir,
    config: PathBuf,
    ipc_dir: PathBuf,
    log_dir: PathBuf,
    endpoint: Endpoint,
    /// How many tunnels the config declares, so a snapshot can be checked against it.
    tunnels: usize,
}

impl TempEnv {
    /// A sandbox whose config declares `tunnels` distinct, **disabled** forwards.
    ///
    /// Disabled on purpose: a server that restored them would try to reach real SSH
    /// hosts, which would make these tests depend on the network. The number of
    /// tunnels is what the snapshot is checked against, and that is independent of
    /// whether they run.
    fn new(tunnels: usize) -> TempEnv {
        let dir = tempfile::tempdir().expect("a temp dir");
        let config = dir.path().join("config.toml");
        write_config(&config, tunnels);
        let ipc_dir = dir.path().join("ipc");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).expect("the log dir must be creatable");
        let endpoint = endpoint_in(&ipc_dir, &config);
        TempEnv { dir, config, ipc_dir, log_dir, endpoint, tunnels }
    }

    /// A file inside (or, for the IPC dir, beside) the sandbox.
    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The config path exactly as the child receives it, which is also the identity
    /// the handshake compares.
    fn config_str(&self) -> &str {
        self.config.to_str().expect("a temp path is valid UTF-8")
    }
}

/// Write a valid config with `count` distinct, disabled local forwards.
fn write_config(path: &Path, count: usize) {
    let mut text = String::from("version = 1\n");
    for i in 1..=count {
        text.push_str(&format!(
            "\n[[tunnels]]\n\
             id = {i}\n\
             name = \"tunnel-{i}\"\n\
             kind = \"local\"\n\
             ssh_host = \"host-{i}\"\n\
             local_addr = \"127.0.0.1\"\n\
             local_port = {}\n\
             remote_host = \"127.0.0.1\"\n\
             remote_port = {}\n\
             enabled = false\n\
             auto_restart = false\n",
            17_000 + i,
            8_000 + i
        ));
    }
    std::fs::write(path, text).expect("writing the fixture config");
}

// ---------------------------------------------------------------------------
// the server process
// ---------------------------------------------------------------------------

/// A `--server` child that is killed when the test ends.
///
/// Without this, a failed assertion would leave a server holding the singleton lock
/// and listening on a port for the rest of the session. On Windows that would also
/// keep `target\debug\sshpf.exe` locked and block the *next* `cargo build` for
/// everyone sharing the worktree, so the guard is a hard requirement, not tidiness.
struct ServerGuard {
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    log_path: PathBuf,
}

impl ServerGuard {
    /// Start `sshpf --server [extra]` in the sandbox, with no terminal attached.
    fn start(env: &TempEnv, tag: &str, extra: &[&str]) -> ServerGuard {
        let stdout_path = env.file(&format!("server-{tag}.out"));
        let stderr_path = env.file(&format!("server-{tag}.err"));
        let stdout = std::fs::File::create(&stdout_path).expect("creating the server stdout file");
        let stderr = std::fs::File::create(&stderr_path).expect("creating the server stderr file");
        let child = Command::new(bin())
            .arg("--server")
            .args(extra)
            .env("SSHPF_CONFIG", &env.config)
            .env("SSHPF_IPC_DIR", &env.ipc_dir)
            .env("SSHPF_LOG_DIR", &env.log_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("the sshpf binary must be runnable");
        ServerGuard { child, stdout_path, stderr_path, log_path: env.log_dir.join("sshpf.log") }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait up to `secs` for the server to publish a port, and fail with its output
    /// if it never does — a server that died on start-up is the usual cause.
    fn published_port(&self, endpoint: &Endpoint, secs: u64) -> u16 {
        match wait_for_port(endpoint, secs) {
            Some(port) => port,
            None => panic!("the server published no port within {secs}s; {}", self.diagnostics()),
        }
    }

    /// Whether the process is still running right now (reaping it if it exited).
    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Poll until the process exits, returning its status if it did within `secs`.
    fn exited_within(&mut self, secs: u64) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Everything the server left behind, for a failure message.
    ///
    /// The log is the only place the server can explain a wrong decision: it is
    /// headless, so a `server.grace_started` line or its absence is the difference
    /// between "the test waited wrong" and "the server never armed the shutdown".
    fn diagnostics(&self) -> String {
        format!(
            "stdout:\n{}\nstderr:\n{}\nlog:\n{}",
            slurp(&self.stdout_path),
            slurp(&self.stderr_path),
            slurp(&self.log_path)
        )
    }
}

impl Drop for ServerGuard {
    /// Kill the child unconditionally, then reap it.
    ///
    /// Never waits for a natural exit: a `--keep-server` server is *supposed* to run
    /// forever, so waiting would hang the suite. Killing and reaping also releases
    /// the executable file lock, which is what lets the next `cargo build` run.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn slurp(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

// ---------------------------------------------------------------------------
// protocol plumbing
// ---------------------------------------------------------------------------

/// Poll `ipc::read_port` until the server publishes a port, or give up.
fn wait_for_port(endpoint: &Endpoint, secs: u64) -> Option<u16> {
    let mut found = None;
    let published = wait_until(secs, || match ipc::read_port(endpoint) {
        Ok(port) => {
            found = Some(port);
            true
        }
        Err(_) => false,
    });
    if published { found } else { None }
}

/// Poll `pred` every 50 ms, returning whether it became true within `secs`.
fn wait_until(secs: u64, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Connect to the published port, with timeouts so a wedged server cannot hang the
/// suite.
fn dial(endpoint: &Endpoint) -> TcpStream {
    let port = ipc::read_port(endpoint).expect("the server must have published a port");
    assert_ne!(port, 0, "an ephemeral bind must produce a real port, not 0");
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("the published port must accept a connection");
    stream.set_read_timeout(Some(READ_SLICE)).expect("setting the read timeout");
    stream.set_write_timeout(Some(READ_SLICE)).expect("setting the write timeout");
    stream
}

/// Send the first frame a server expects.
fn hello(stream: &mut TcpStream, token: &str, config_path: &str) {
    let msg = ClientMsg::Hello {
        proto_version: PROTO_VERSION,
        config_path: config_path.to_string(),
        token: token.to_string(),
    };
    ipc::write_message(stream, &msg).expect("the hello frame must be writable");
}

/// What one read attempt produced.
enum Next {
    Msg(ServerMsg),
    /// The connection ended, or became unusable. Both mean "the server is gone".
    Closed,
    /// Nothing arrived within the slice; the connection is still open.
    Idle,
}

/// One read attempt, bounded by `millis`.
fn read_next(stream: &mut TcpStream, millis: u64) -> Next {
    stream.set_read_timeout(Some(Duration::from_millis(millis.max(1)))).expect("setting the read timeout");
    match ipc::read_server_msg(stream) {
        Ok(Some(msg)) => Next::Msg(msg),
        // A clean EOF at a frame boundary: the server closed this session.
        Ok(None) => Next::Closed,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Next::Idle,
        // A reset or a decoding failure: equally unusable.
        Err(_) => Next::Closed,
    }
}

/// Read until a message satisfies `pred`, returning it or the reason it never came.
fn drain_until(
    stream: &mut TcpStream,
    secs: u64,
    mut pred: impl FnMut(&ServerMsg) -> bool,
) -> Result<ServerMsg, String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("no matching message within {secs}s"));
        }
        let slice = u64::try_from(left.as_millis()).unwrap_or(u64::MAX).min(1_000);
        match read_next(stream, slice) {
            Next::Msg(msg) => {
                if pred(&msg) {
                    return Ok(msg);
                }
            }
            Next::Closed => return Err("the server closed the connection".to_string()),
            Next::Idle => {}
        }
    }
}

/// A welcomed client: its socket, the state the server handed it, and the newest
/// state it has read since.
struct Attached {
    stream: TcpStream,
    config_path: String,
    server_pid: u32,
    /// The snapshot carried by the `Welcome`, i.e. the state at attach time.
    snapshot: Snapshot,
    /// The newest snapshot read since then (never behind [`Self::snapshot`]).
    latest: Snapshot,
}

impl Attached {
    /// Read one pending frame, applying it if it is a snapshot.
    ///
    /// Returns whether a snapshot was applied. Used from a `wait_until` closure, so
    /// it must return promptly: nothing pending means one bounded read attempt, not
    /// a wait.
    fn read_newer(&mut self) -> bool {
        match read_next(&mut self.stream, 50) {
            Next::Msg(ServerMsg::Snapshot(snapshot)) if snapshot.revision > self.latest.revision => {
                self.latest = snapshot;
                true
            }
            _ => false,
        }
    }
}

/// Attach a raw client that speaks the protocol correctly, and return its welcome.
fn attach(env: &TempEnv) -> Attached {
    let token = ipc::read_token(&env.endpoint).expect("the server must publish a token");
    let mut stream = dial(&env.endpoint);
    hello(&mut stream, &token, env.config_str());
    match drain_until(&mut stream, 10, |msg| matches!(msg, ServerMsg::Welcome { .. })) {
        Ok(ServerMsg::Welcome { config_path, server_pid, snapshot, .. }) => {
            Attached { stream, config_path, server_pid, snapshot: snapshot.clone(), latest: snapshot }
        }
        Ok(other) => panic!("expected a welcome, got {other:?}"),
        Err(e) => panic!("the server never welcomed a correct client: {e}"),
    }
}

// ---------------------------------------------------------------------------
// discovery and the handshake
// ---------------------------------------------------------------------------

#[test]
fn the_server_publishes_a_port_and_token_and_a_client_can_attach() {
    let env = TempEnv::new(2);
    let server = ServerGuard::start(&env, "primary", &[]);

    // The endpoint is the discovery contract: a port file and a token file, both
    // named after the config, in the directory the environment selected.
    let port = server.published_port(&env.endpoint, 15);
    assert_ne!(port, 0, "a bound port must never be 0");
    assert_eq!(env.endpoint.port_path.parent(), Some(env.ipc_dir.as_path()), "the port file lives in SSHPF_IPC_DIR");
    let token = ipc::read_token(&env.endpoint).expect("the token must be published beside the port");
    assert!(!token.is_empty(), "an empty token would authorise nobody (see check_token)");

    let client = attach(&env);
    assert!(
        same_path(&client.config_path, env.config_str()),
        "the welcome must name the config being served: got {:?}, expected {:?}",
        client.config_path,
        env.config_str()
    );
    assert_eq!(
        client.snapshot.tunnels.len(),
        env.tunnels,
        "the first snapshot must carry every tunnel in the config: {:?}",
        client.snapshot.tunnels.iter().map(|t| &t.cfg.name).collect::<Vec<_>>()
    );
    assert!(client.snapshot.revision >= 1, "revisions are strictly increasing and start at 1");
    assert!(client.snapshot.client_count >= 1, "one client is attached, so the count cannot be 0");
    assert_eq!(client.server_pid, server.pid(), "the welcome must name the process that answered it");
    assert!(!client.snapshot.keep_server, "a plain --server does not outlive its clients");
}

#[test]
fn a_client_with_the_wrong_token_is_refused() {
    let env = TempEnv::new(1);
    let server = ServerGuard::start(&env, "primary", &[]);
    server.published_port(&env.endpoint, 15);

    // The port is reachable by any local process — that is exactly why the token
    // exists. A stranger must be turned away *before* any state is sent.
    let mut stranger = dial(&env.endpoint);
    hello(&mut stranger, "00000000000000000000000000000000", env.config_str());

    let mut saw_welcome = false;
    let mut refusal: Option<String> = None;
    let mut closed = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match read_next(&mut stranger, 1_000) {
            Next::Msg(ServerMsg::Welcome { .. }) => {
                saw_welcome = true;
            }
            Next::Msg(ServerMsg::Error { code, .. }) => {
                refusal = Some(code);
                // The refusal is followed by a close; no need to wait for it.
                break;
            }
            Next::Msg(_) => {}
            Next::Closed => {
                closed = true;
                break;
            }
            Next::Idle => {}
        }
    }

    assert!(!saw_welcome, "a wrong token must never receive the state: the token is the permission boundary");
    if let Some(code) = &refusal {
        assert_eq!(code, "bad_token", "a token mismatch must be reported as bad_token, not {code}");
    }
    assert!(
        refusal.is_some() || closed,
        "a wrong token must be refused with an error or a close, not ignored (refusal={refusal:?}, closed={closed})"
    );
}

// ---------------------------------------------------------------------------
// several clients, one authoritative state
// ---------------------------------------------------------------------------

#[test]
fn two_clients_see_the_same_snapshot_revision() {
    let env = TempEnv::new(2);
    let server = ServerGuard::start(&env, "primary", &[]);
    server.published_port(&env.endpoint, 15);

    let mut first = attach(&env);
    let mut second = attach(&env);
    assert_eq!(first.snapshot.tunnels.len(), env.tunnels);
    assert_eq!(second.snapshot.tunnels.len(), env.tunnels);

    // The two welcomes were produced at different moments — attaching is itself a
    // change — so the snapshots must be read forward until both clients hold the
    // *same* revision. That convergence is the property under test: one
    // authoritative state, broadcast to everyone.
    let converged = wait_until(10, || {
        first.read_newer();
        second.read_newer();
        first.latest.revision == second.latest.revision && first.latest.client_count == 2
    });
    assert!(
        converged,
        "both clients must converge on one revision with client_count=2 \
         (first rev={} count={}, second rev={} count={})",
        first.latest.revision, first.latest.client_count, second.latest.revision, second.latest.client_count
    );
    assert_eq!(
        first.latest.tunnels, second.latest.tunnels,
        "the same revision must describe the same tunnels to every client"
    );
    assert_eq!(first.latest.tunnels.len(), env.tunnels, "neither client may lose a row");
}

// ---------------------------------------------------------------------------
// one client leaving vs. the last client leaving
// ---------------------------------------------------------------------------

#[test]
fn one_client_leaving_does_not_stop_the_server() {
    let env = TempEnv::new(1);
    let mut server = ServerGuard::start(&env, "primary", &[]);
    let port = server.published_port(&env.endpoint, 15);

    let staying = attach(&env);
    let mut leaving = attach(&env);

    // A polite goodbye from one of two clients. `last_exit` is a proposal, and the
    // server recounts: one client is still attached, so nothing may stop.
    let bye = ClientMsg::Bye { last_exit: false };
    ipc::write_message(&mut leaving.stream, &bye).expect("the bye frame must be writable");
    drop(leaving);

    // Give the shutdown path every chance to be wrong before asserting it is not.
    std::thread::sleep(Duration::from_secs(2));
    assert!(server.is_running(), "a client leaving must not stop the server; {}", server.diagnostics());
    assert_eq!(
        ipc::read_port(&env.endpoint).ok(),
        Some(port),
        "the port file must still name the port the server bound: {}",
        env.endpoint.port_path.display()
    );

    // And it must still be *serving*, not merely alive: a fresh connection is the
    // observable proof.
    let newcomer = attach(&env);
    assert_eq!(newcomer.snapshot.tunnels.len(), env.tunnels, "a newcomer must still be served the tunnels");
    assert_eq!(newcomer.server_pid, server.pid(), "the same process must still be answering");
    drop(staying);
}

#[test]
fn the_last_client_leaving_stops_the_server() {
    let env = TempEnv::new(1);
    let mut server = ServerGuard::start(&env, "primary", &[]);
    server.published_port(&env.endpoint, 15);

    let mut last = attach(&env);
    let bye = ClientMsg::Bye { last_exit: true };
    ipc::write_message(&mut last.stream, &bye).expect("the bye frame must be writable");

    // The count reaches zero, so the server enters its (short) grace window and
    // then exits. A few seconds is generous for a 0.5 s window.
    let status = server
        .exited_within(10)
        .unwrap_or_else(|| panic!("the last client left but the server is still running; {}", server.diagnostics()));
    assert_eq!(status.code(), Some(0), "a normal shutdown is a successful exit: {status:?}");

    // A clean shutdown removes the discovery files so the next run does not read a
    // port nobody listens on.
    let removed = wait_until(5, || !env.endpoint.port_path.exists());
    assert!(removed, "a clean shutdown must remove the port file {}", env.endpoint.port_path.display());
}

#[test]
fn an_abrupt_disconnect_keeps_the_server_up_for_the_grace_window_then_exits() {
    let env = TempEnv::new(1);
    let mut server = ServerGuard::start(&env, "primary", &[]);
    server.published_port(&env.endpoint, 15);

    // No `Bye`: the socket just disappears, which is what a killed terminal or a
    // `TerminateProcess` looks like from the other side. Dropping the client closes
    // the socket without a frame ever being written.
    let client = attach(&env);
    drop(client);

    std::thread::sleep(Duration::from_secs(1));
    assert!(
        server.is_running(),
        "a crashed client must not tear the tunnels down immediately: the grace window exists so a \
         reopened terminal keeps them; {}",
        server.diagnostics()
    );
    assert!(env.endpoint.port_path.exists(), "the endpoint must still be published during the grace window");

    let exited = server.exited_within(10);
    assert!(exited.is_some(), "the server must exit once the grace window has passed; {}", server.diagnostics());
}

#[test]
fn a_new_client_during_the_grace_window_cancels_the_shutdown() {
    let env = TempEnv::new(1);
    let mut server = ServerGuard::start(&env, "primary", &[]);
    server.published_port(&env.endpoint, 15);

    let client = attach(&env);
    drop(client);

    // Inside the ~5 s grace window: this is the "user closed the window and typed
    // the command again" case, and it must cost nothing.
    std::thread::sleep(Duration::from_secs(1));
    let reopened = attach(&env);

    // Well past the window: if the new client had not cancelled it, the server
    // would be gone by now.
    std::thread::sleep(Duration::from_secs(7));
    assert!(
        server.is_running(),
        "a client arriving during the grace window must cancel the shutdown; {}",
        server.diagnostics()
    );
    assert!(env.endpoint.port_path.exists(), "the endpoint stays published because the shutdown was cancelled");
    assert_eq!(reopened.snapshot.tunnels.len(), env.tunnels, "the reopened client sees the same tunnels");
}

// ---------------------------------------------------------------------------
// election
// ---------------------------------------------------------------------------

#[test]
fn only_one_server_can_hold_the_config() {
    let env = TempEnv::new(1);
    let mut winner = ServerGuard::start(&env, "winner", &[]);
    winner.published_port(&env.endpoint, 15);

    // The second server wants the same config and the same IPC directory. Two
    // servers would mean two `Supervisor`s binding the same local ports, so the
    // loser must stand down quietly rather than fail loudly.
    let mut loser = ServerGuard::start(&env, "loser", &[]);
    let status = loser.exited_within(10).unwrap_or_else(|| {
        panic!("two servers for one config must not both run; the loser kept going");
    });
    assert_eq!(
        status.code(),
        Some(0),
        "losing the election is a normal race, not a failure: {status:?}; {}",
        loser.diagnostics()
    );

    // The winner is untouched and still serving.
    assert!(winner.is_running(), "the winner must keep running; {}", winner.diagnostics());
    let client = attach(&env);
    assert_eq!(client.snapshot.tunnels.len(), env.tunnels, "the winner still serves the config");
    assert_eq!(client.server_pid, winner.pid(), "the process answering must be the winner");
}

// ---------------------------------------------------------------------------
// --keep-server
// ---------------------------------------------------------------------------

#[test]
fn keep_server_survives_its_last_client() {
    let env = TempEnv::new(1);
    let mut server = ServerGuard::start(&env, "keeper", &["--keep-server"]);
    server.published_port(&env.endpoint, 15);

    let mut last = attach(&env);
    assert!(last.snapshot.keep_server, "the snapshot must tell clients the server will not exit with them");
    let bye = ClientMsg::Bye { last_exit: true };
    ipc::write_message(&mut last.stream, &bye).expect("the bye frame must be writable");
    drop(last);

    // Several seconds — longer than either grace window — then the same check.
    std::thread::sleep(Duration::from_secs(6));
    assert!(server.is_running(), "--keep-server must outlive its last client; {}", server.diagnostics());

    // Still discoverable and still serving: a server that stayed alive but stopped
    // accepting would be useless.
    let again = attach(&env);
    assert_eq!(again.snapshot.tunnels.len(), env.tunnels, "the kept server must still serve the tunnels");
    assert!(again.snapshot.keep_server, "the flag must still be reported after the restart of a session");
}

// ---------------------------------------------------------------------------
// modes that must never touch the server
// ---------------------------------------------------------------------------

#[test]
fn check_mode_never_starts_a_server() {
    let env = TempEnv::new(2);

    let out = Command::new(bin())
        .arg("--check")
        .env("SSHPF_CONFIG", &env.config)
        .env("SSHPF_IPC_DIR", &env.ipc_dir)
        .env("SSHPF_LOG_DIR", &env.log_dir)
        .stdin(Stdio::null())
        .output()
        .expect("running --check");
    assert!(out.status.success(), "--check must succeed on a valid config: {out:?}");

    // The observable difference between "validate a file" and "start the world":
    // no endpoint was published, so no server came up behind the user's back.
    let published = published_endpoints(&env.ipc_dir);
    assert!(
        published.is_empty(),
        "--check must never start a server, but it published {:?}",
        published.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
    );
}

/// Every `*.port` / `*.token` file in a directory, sorted, for "nothing was
/// published" assertions.
fn published_endpoints(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if matches!(path.extension().and_then(|e| e.to_str()), Some("port") | Some("token")) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
