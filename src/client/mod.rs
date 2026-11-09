//! Becoming a client: attach to the one server that serves this config,
//! starting that server when nobody is serving yet.
//!
//! # What belongs here
//!
//! The **order** of the attach algorithm, and nothing else. That order is the
//! feature's only safety property, so it lives in one readable function
//! ([`attach`]) instead of being spread over the modules below:
//!
//! ```text
//! ① connect ── ok ─────────────────────────────► handshake ► pump
//!    │ failed
//! ② take the election lock (File::try_lock)
//!    │ busy ──► another process is electing or serving: retry connect, never spawn
//!    │ got it ──► ③ connect once more (a rival may have just become reachable)
//!    │            ④ spawn a detached server, hand it the lock, retry connect
//!    ▼
//! ⑤ budget spent ──► bilingual, actionable error (never "just forward locally")
//! ```
//!
//! A client **never** binds a forwarding port, never owns a `Supervisor` and
//! never restores tunnels: the server is the authority for all three. All this
//! module produces is a socket that has said `Welcome`, plus two threads.
//!
//! # Why the lock is released *before* the retries, not after
//!
//! Holding the election lock means "no server is alive for this config" — a live
//! server holds it for its whole lifetime. The spawned server must therefore
//! *acquire* that same lock before it may unlink a stale socket and bind, so a
//! client that keeps holding it blocks the very server it just started: the child
//! waits for the lock, the parent waits for a socket nobody will ever bind, and
//! both time out. Releasing it right after `spawn` hands the election over.
//!
//! Nothing is lost by releasing: a rival that wins the released lock only spawns
//! a rival server, which loses the election and exits (exit code 3) — one
//! short-lived process, never two serving ones. The stale-socket `unlink` that
//! holding the lock used to guard also moved into the server, where it happens
//! under the lock.
//!
//! # Layout
//!
//! | file | what lives there |
//! |---|---|
//! | [`mod`] (this file) | [`AttachError`], the election order, waiting for a socket |
//! | [`handshake`] | connect, `Hello`/`Welcome`, and every way the exchange can fail |
//! | [`spawn`] | the detached `--server` child and its command line |
//! | [`pump`] | the reader/writer threads that own the socket afterwards |
//!
//! Does not belong here: what a snapshot means ([`crate::app::remote`]), the
//! socket/message encoding ([`crate::ipc`]), or the terminal.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::config::Paths;
use crate::eventlog;
use crate::ipc::{self, ClientMsg, Endpoint, ServerMsg};
use crate::ssh::known_hosts::HostKeyPolicy;
use crate::sys;

pub mod handshake;
pub mod pump;
pub mod spawn;

/// First delay between two `connect` attempts. Doubles per attempt.
const FIRST_RETRY: Duration = Duration::from_millis(20);

/// Ceiling for one delay, so the later attempts are not seconds apart.
const MAX_RETRY: Duration = Duration::from_millis(250);

/// How long a client waits for a server to become reachable before it reports a
/// failure. A cold start (spawn, exec, config load, restore) fits well inside it;
/// a wedged machine does not, and telling the user beats hanging the terminal.
const CONNECT_BUDGET: Duration = Duration::from_secs(5);

/// Bilingual, actionable failure to become a client.
///
/// Both halves carry the same information: what was tried, and what the user can
/// do about it. A caller that only has room for one line uses [`AttachError::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachError {
    pub zh: String,
    pub en: String,
}

impl AttachError {
    /// Build a bilingual error.
    pub fn new(zh: impl Into<String>, en: impl Into<String>) -> AttachError {
        AttachError { zh: zh.into(), en: en.into() }
    }

    /// One line, Chinese first — the shape `main` writes to stderr.
    pub fn text(&self) -> String {
        crate::help::bi(&self.zh, &self.en)
    }
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

impl std::error::Error for AttachError {}

/// Connect to the server serving `paths.config`, spawning it if none is running.
///
/// Performs the `Hello`/`Welcome` handshake synchronously, so the caller either
/// gets a working session or a precise reason — never a half-open connection that
/// fails on the first keypress. After returning, the socket belongs to two
/// threads: a reader forwarding [`ServerMsg`]s into `inbound`, and a writer
/// draining `outbound` into frames.
///
/// `keep_server` is only a property of a server *this* call starts; when it is
/// true the spawned server is told to stay up after its last client leaves.
///
/// A failure is returned, never logged: the caller (`main`) is the only place
/// that knows whether it is about to print it, show it in the status bar, or
/// exit, and one failure must not produce two records.
pub fn attach(
    paths: &Paths,
    host_key_policy: HostKeyPolicy,
    keep_server: bool,
) -> Result<(Sender<ClientMsg>, Receiver<ServerMsg>), AttachError> {
    let endpoint = ipc::endpoint_for(&paths.config);
    let config_path = endpoint.config_path.display().to_string();
    // ① The common case: a server is already serving (this is the second window).
    let socket = match handshake::connect(&endpoint) {
        Ok(socket) => socket,
        // ② Nobody answered: either become the elector, or wait for whoever is.
        Err(_) => join_or_spawn(&endpoint, keep_server, host_key_policy)?,
    };
    // ③ A failure here is final on purpose: a server *did* answer, so spawning a
    // second one would paper over a version/config mismatch instead of reporting
    // it. The user is told which of the two it is.
    let session = handshake::handshake(socket, &config_path, host_key_policy)?;
    eventlog::client_attached(session.server_pid, &session.config_path);
    pump::start(session.socket, Some(session.welcome))
}

/// The election: take the lock or wait, and spawn only when nobody is serving.
fn join_or_spawn(
    endpoint: &Endpoint,
    keep_server: bool,
    host_key_policy: HostKeyPolicy,
) -> Result<UnixStream, AttachError> {
    let held = take_election_lock(endpoint)?;
    // Busy means "another process is electing or serving". Spawning a second
    // server here is the one mistake this algorithm exists to prevent, so the
    // only move left is to wait for the winner's socket.
    let Some(lock) = held else {
        return wait_for_server(endpoint);
    };
    // We hold the lock, so no live server exists (it would still hold it). A
    // connect that works now can only be a server that bound microseconds before
    // we took the lock — the tolerated race — so use it instead of a rival.
    if let Ok(socket) = handshake::connect(endpoint) {
        return Ok(socket);
    }
    // Still nothing: we are the spawner. The child needs this lock to bind, so
    // the election is handed over here (see "why the lock is released" above).
    let pid = spawn::spawn_server(&endpoint.config_path, keep_server, host_key_policy)?;
    eventlog::client_server_spawned(pid);
    drop(lock);
    wait_for_server(endpoint)
}

/// Take the election lock, or report that somebody else owns it.
///
/// `Ok(None)` is "busy", not an error: it is the normal outcome for every client
/// that loses the race, and the caller must wait rather than spawn.
fn take_election_lock(endpoint: &Endpoint) -> Result<Option<File>, AttachError> {
    prepare_dir(&endpoint.dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(sys::PRIVATE_FILE_MODE)
        .open(&endpoint.lock_path)
        .map_err(|e| lock_error(endpoint, &e))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        // A real I/O failure (read-only directory, no descriptors) is a reportable
        // problem, not a busy lock: retrying forever would hide it.
        Err(std::fs::TryLockError::Error(e)) => Err(lock_error(endpoint, &e)),
    }
}

/// Create the socket/lock directory private to this user.
fn prepare_dir(dir: &Path) -> Result<(), AttachError> {
    std::fs::create_dir_all(dir).map_err(|e| dir_error(dir, &e))?;
    // 0700: the socket is a door into this process, so nobody else may open it.
    // The parent directory ($XDG_RUNTIME_DIR) is already per-user; this is the
    // sshpf-owned inner one, and tightening it is cheap.
    sys::restrict_dir(dir).map_err(|e| dir_error(dir, &e))
}

/// Retry `connect` with exponential backoff until the budget is spent.
///
/// The first attempt is made before the deadline is read, so a server that is
/// already up costs one `connect` and no sleeping at all — the common case for a
/// second window is not delayed by the election's machinery.
fn wait_for_server(endpoint: &Endpoint) -> Result<UnixStream, AttachError> {
    let deadline = Instant::now() + CONNECT_BUDGET;
    let mut delay = FIRST_RETRY;
    let mut last = match handshake::connect(endpoint) {
        Ok(socket) => return Ok(socket),
        Err(e) => e,
    };
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(no_server_error(endpoint, Some(&last)));
        }
        // Never sleep past the deadline: the error must arrive on time.
        std::thread::sleep(delay.min(left));
        delay = next_delay(delay);
        match handshake::connect(endpoint) {
            Ok(socket) => return Ok(socket),
            Err(e) => last = e,
        }
    }
}

/// The delay after `current`: double it, but never beyond [`MAX_RETRY`].
fn next_delay(current: Duration) -> Duration {
    (current * 2).min(MAX_RETRY)
}

/// The lock could not be opened at all.
fn lock_error(endpoint: &Endpoint, detail: &io::Error) -> AttachError {
    let path = endpoint.lock_path.display();
    AttachError::new(
        format!("无法打开单例锁 {path}：{detail}。请确认该目录可由当前用户写入，然后重试。"),
        format!(
            "Could not open the singleton lock {path}: {detail}. Make sure the directory is writable by this user, then retry."
        ),
    )
}

/// The socket directory could not be created or tightened.
fn dir_error(dir: &Path, detail: &io::Error) -> AttachError {
    let path = dir.display();
    AttachError::new(
        format!("无法创建运行目录 {path}：{detail}。请确认该目录可由当前用户写入，然后重试。"),
        format!(
            "Could not create the runtime directory {path}: {detail}. Make sure it is writable by this user, then retry."
        ),
    )
}

/// No server became reachable within [`CONNECT_BUDGET`].
fn no_server_error(endpoint: &Endpoint, last: Option<&io::Error>) -> AttachError {
    let socket = endpoint.socket_path.display();
    let seconds = CONNECT_BUDGET.as_secs();
    let detail = last.map(|e| e.to_string()).unwrap_or_else(|| "unknown".to_string());
    AttachError::new(
        format!(
            "无法连接到 sshpf 服务端（{socket}），已重试 {seconds} 秒，最后错误：{detail}。\
             请重试；若持续失败，先在一个终端运行 `sshpf --server` 查看原因，并检查日志。"
        ),
        format!(
            "Could not reach the sshpf server ({socket}) after {seconds}s of retries; last error: {detail}. \
             Retry, or run `sshpf --server` in a terminal to see why, and check the log."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    /// A throwaway endpoint: the real paths, derived into a tempdir.
    fn endpoint_in(dir: &Path) -> Endpoint {
        Endpoint {
            config_path: dir.join("config.toml"),
            socket_path: dir.join("s.sock"),
            lock_path: dir.join("s.lock"),
            dir: dir.to_path_buf(),
        }
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn the_backoff_doubles_and_then_stops_growing() {
        assert_eq!(next_delay(FIRST_RETRY), Duration::from_millis(40));
        assert_eq!(next_delay(Duration::from_millis(40)), Duration::from_millis(80));
        // The cap is the point: later attempts must stay frequent enough to feel
        // instant once the server comes up.
        assert_eq!(next_delay(MAX_RETRY), MAX_RETRY);
        assert_eq!(next_delay(MAX_RETRY * 2), MAX_RETRY);
    }

    #[test]
    fn the_retry_schedule_is_bounded_by_the_budget() {
        // What the user actually waits for: sleep, not attempts. The schedule
        // must fit the budget and must not degenerate into a busy loop.
        let mut delay = FIRST_RETRY;
        let mut slept = Duration::ZERO;
        let mut attempts = 0usize;
        while slept < CONNECT_BUDGET {
            slept += delay;
            delay = next_delay(delay);
            attempts += 1;
            assert!(attempts < 128, "the schedule never terminates");
        }
        assert!(attempts >= 4, "too few attempts to ride out a cold start: {attempts}");
        assert!(attempts <= 64, "the poll loop would be too chatty: {attempts}");
        assert!(slept < CONNECT_BUDGET + MAX_RETRY, "overshoot must stay under one delay: {slept:?}");
    }

    #[test]
    fn the_lock_is_exclusive_and_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(dir.path());
        let first = take_election_lock(&endpoint).expect("lock").expect("no rival");
        // The directory is created private, and so is the lock file: a world
        // readable lock would let another user take the election.
        assert_eq!(mode_of(&endpoint.dir), 0o700, "socket dir must be 0700");
        assert_eq!(mode_of(&endpoint.lock_path), 0o600, "lock file must be 0600");

        // A second handle in this process takes the same kernel lock, which is
        // exactly what a rival client would hit.
        assert!(take_election_lock(&endpoint).expect("lock").is_none(), "a held lock must read as busy");

        drop(first);
        assert!(take_election_lock(&endpoint).expect("lock").is_some(), "a released lock must be free");
    }

    #[test]
    fn a_dead_socket_file_is_not_mistaken_for_a_server() {
        // A SIGKILLed server leaves the file behind. `connect` must fail, so the
        // caller falls through to the election instead of trusting the path.
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(dir.path());
        std::fs::write(&endpoint.socket_path, b"stale").expect("write");
        let err = handshake::connect(&endpoint).expect_err("a plain file is not a socket");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::Other
            ),
            "unexpected kind {:?}",
            err.kind()
        );
    }

    #[test]
    fn a_listening_socket_is_reached_without_waiting_for_the_budget() {
        // The happy half of `wait_for_server`: the first attempt succeeds, so the
        // 5 s budget is never spent.
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(dir.path());
        let _listener = std::os::unix::net::UnixListener::bind(&endpoint.socket_path).expect("bind");
        let started = Instant::now();
        let socket = wait_for_server(&endpoint).expect("should connect on the first try");
        assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
        drop(socket);
    }

    #[test]
    fn waiting_for_a_server_that_never_appears_reports_what_was_tried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(dir.path());
        let err = no_server_error(&endpoint, None);
        // Actionable: names the path, the budget and a next step, in both languages.
        assert!(err.zh.contains("s.sock"), "{}", err.zh);
        assert!(err.zh.contains("sshpf --server"), "{}", err.zh);
        assert!(err.en.contains("sshpf --server"), "{}", err.en);
        assert!(err.en.contains('5'), "the budget must be stated: {}", err.en);
        assert!(!err.text().is_empty());
        // Display is the Chinese-first single line `main` prints.
        assert_eq!(err.to_string(), err.text());
    }

    #[test]
    fn the_lock_can_be_taken_by_a_child_after_exec() {
        // The hand-over in `join_or_spawn` only works if the lock descriptor does
        // not survive `exec` into the spawned server. If it did, the child would
        // inherit an open file description on the same flock, the parent's `drop`
        // would not release it, and the server it just started could never take
        // the lock to bind — the deadlock this algorithm exists to avoid.
        //
        // O_CLOEXEC is the mechanism, and `std` sets it on every file it opens;
        // this asserts that rather than trusting it.
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(dir.path());
        let lock = take_election_lock(&endpoint).expect("lock").expect("no rival");
        let flags = descriptor_flags(&lock).expect("this platform must expose its descriptor flags");
        // 0o2000000 is O_CLOEXEC.
        assert_ne!(flags & 0o2000000, 0, "the election lock must not leak into the spawned server (flags {flags:o})");
        drop(lock);
    }

    /// The `flags:` field of `/proc/self/fdinfo/<fd>`, octal.
    #[cfg(target_os = "linux")]
    fn descriptor_flags(file: &File) -> Option<u32> {
        use std::os::unix::io::AsRawFd;
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd())).ok()?;
        let line = info.lines().find(|l| l.starts_with("flags:"))?;
        u32::from_str_radix(line.split_whitespace().nth(1)?, 8).ok()
    }

    /// Only Linux exposes descriptor flags; elsewhere the assertion is skipped
    /// rather than faked (the property still holds — `std` sets the flag on every
    /// platform — this just cannot be observed without `unsafe` `fcntl`).
    #[cfg(not(target_os = "linux"))]
    fn descriptor_flags(_file: &File) -> Option<u32> {
        None
    }

    #[test]
    fn the_endpoint_paths_are_the_ones_the_lock_and_connect_use() {
        // The algorithm relies on both halves agreeing on one directory: the lock
        // is taken beside the socket it protects.
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint_in(dir.path());
        assert_eq!(endpoint.lock_path.parent(), Some(endpoint.dir.as_path()));
        assert_eq!(endpoint.socket_path.parent(), Some(endpoint.dir.as_path()));
        assert!(endpoint.config_path.as_os_str().as_bytes().ends_with(b"config.toml"));
    }
}
