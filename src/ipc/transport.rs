//! The transport half of the endpoint: the loopback listener, the discovery
//! files, the singleton lock, and the shared secret a client must present.
//!
//! # Why loopback TCP, on every platform
//!
//! `std` exposes Unix domain sockets on Unix but not on Windows, so a UDS
//! transport would need a second implementation, a second discovery scheme and a
//! second set of failure modes. `TcpListener`/`TcpStream` on `127.0.0.1` work
//! identically on both, so there is one code path to reason about and test.
//!
//! # Why a secret when the socket is loopback-only
//!
//! `127.0.0.1` is **not** a permission boundary: on a shared machine any local
//! user can connect to any loopback port they can guess. The protection is a
//! 128-bit token that the server generates once per run, writes to a private file
//! in a per-user directory, and requires in the handshake ([`Hello`]). Another
//! user can reach the port but cannot read the token, so they cannot be served.
//!
//! [`Hello`]: super::msg::ClientMsg::Hello
//!
//! # Why the lock file makes the server a singleton
//!
//! The server holds an exclusive [`File::try_lock`] for its whole life. Losing a
//! race is not an error: the loser learns "a server is already serving this
//! config" and becomes a client. Process death releases the lock automatically —
//! including `SIGKILL` / `TerminateProcess`, because the kernel closes the handle.
//! That is what lets a stale port file be told apart from a live server.
//!
//! Belongs here: binding, discovery file I/O, locking, token generation, and
//! connecting with a bounded timeout.
//! Does not belong: the paths themselves ([`super::endpoint`]), or what the bytes
//! mean once connected ([`super::codec`]).

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use super::endpoint::Endpoint;

/// How long a client waits for the TCP handshake before giving up.
///
/// Loopback connects either succeed immediately or fail with a refusal, so this
/// only bounds the pathological case (a firewall filter, a wedged backlog). It is
/// short because it runs inside the UI's retry loop, not on a user's patience.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the server waits for a client's `Hello` before dropping it.
///
/// A connection that opens and then says nothing must not hold a slot: without
/// this, `client_count` could never return to zero and the server would never
/// exit.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A fresh 32-hex-digit secret from OS entropy.
///
/// `RandomState` is seeded from the operating system's entropy source each time
/// it is constructed, so two instances hash any input to unrelated values; the
/// process id and the clock are folded in as well so that even a pathological
/// repeat of the seeder cannot produce the same token twice within one run.
pub fn generate_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut out = String::with_capacity(32);
    let mut mixer = |salt: u64| {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", hasher.finish()));
    };
    mixer(0);
    mixer(1);
    out
}

/// The server's listening socket on loopback, plus the port it got.
///
/// The port is ephemeral (`0`) so two servers for *different* configs never
/// collide, and no fixed port is reserved for sshpf.
pub fn bind_loopback() -> io::Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Connect to a server described by `endpoint`.
///
/// The port file is the discovery mechanism; a missing or unparsable file means
/// "no server", which the caller turns into "spawn one".
pub fn connect(endpoint: &Endpoint) -> io::Result<TcpStream> {
    let port = read_port(endpoint)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
}

/// Publish the port a server just bound.
///
/// Written atomically (temp file then rename) so a client never reads a
/// half-written number and dials port `0` or a truncated one.
pub fn write_port(endpoint: &Endpoint, port: u16) -> io::Result<()> {
    write_atomic(&endpoint.port_path, port.to_string().as_bytes())
}

/// Read the published port.
pub fn read_port(endpoint: &Endpoint) -> io::Result<u16> {
    let text = read_trimmed(&endpoint.port_path)?;
    text.parse::<u16>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("端口文件内容无法解析: {text:?}")))
}

/// Write the token a client must present.
///
/// Uses the private-file constructor so the file is `0600` on Unix; on Windows it
/// inherits the per-user directory's ACL, which is the equivalent boundary there.
pub fn write_token(endpoint: &Endpoint, token: &str) -> io::Result<()> {
    let mut file = crate::sys::create_private_file(&endpoint.token_path)?;
    file.write_all(token.as_bytes())?;
    file.flush()
}

/// Read the token a server published.
pub fn read_token(endpoint: &Endpoint) -> io::Result<String> {
    read_trimmed(&endpoint.token_path)
}

/// Remove the endpoint's discovery files (a server does this on clean shutdown).
///
/// The lock file is left alone on purpose: it holds no state, and deleting a file
/// another process may be about to lock is a worse bug than a leftover empty file.
pub fn remove_files(endpoint: &Endpoint) {
    let _ = std::fs::remove_file(&endpoint.port_path);
    let _ = std::fs::remove_file(&endpoint.token_path);
}

/// Try to become the one server for this config.
///
/// `Ok(Some(_))` means the caller is the server and must keep the value alive for
/// its whole life. `Ok(None)` means someone else holds it — a live server, or one
/// that is starting up — so the caller should behave as a client.
pub fn try_become_singleton(lock_path: &std::path::Path) -> io::Result<Option<File>> {
    let file = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(e) if is_lock_held(&e) => Ok(None),
        // `TryLockError` is `WouldBlock`-shaped for real contention and `Io` for a
        // genuine failure; anything but contention is reported.
        Err(std::fs::TryLockError::Error(e)) => Err(e),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
    }
}

/// Whether *no* server is serving this endpoint's config, decided by the lock.
///
/// This is the authority for "should a client start a server?", which is a
/// different question from "did the dial succeed". A dial can fail for reasons that
/// say nothing about whether a server exists — most importantly, on some Windows
/// hosts a **refused** loopback connect takes longer than [`CONNECT_TIMEOUT`] to
/// come back, so it surfaces as `TimedOut` instead of `ConnectionRefused`. Reading
/// that as "a server is running" strands the user: the client refuses to spawn,
/// prints a timeout, and exits while nothing at all is listening.
///
/// The lock answers the question directly. A live server holds it for its whole
/// life and the kernel releases it the moment the process dies, so *being able to
/// take it* proves nobody is serving. That is the same fact the election relies on,
/// asked without the guesswork.
///
/// The lock is taken and dropped immediately: the caller only wants the answer, and
/// the server it goes on to spawn runs its own election.
///
/// * `Ok(true)` — nothing is serving; starting a server is the right move.
/// * `Ok(false)` — a server holds the lock: serving already, or still starting up.
/// * `Err(_)` — the lock cannot be used at all, so the question is unanswerable.
///   Callers must not read this as "go ahead and spawn": a directory broken enough
///   to fail the lock would fail the child's election in exactly the same way.
pub fn no_live_server(endpoint: &Endpoint) -> io::Result<bool> {
    match try_become_singleton(&endpoint.lock_path) {
        Ok(Some(lock)) => {
            // Dropped at once: holding it would make the server we are about to
            // spawn lose its own election.
            drop(lock);
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether a failed [`File::try_lock`] means "already locked" rather than a real
/// I/O problem.
///
/// Both supported platforms report contention as `WouldBlock`, which is the
/// portable part (`RawOsError` values differ — Windows reports
/// `ERROR_LOCK_VIOLATION`). Any other kind is a real failure and is propagated, so
/// a broken filesystem is never silently read as "another server is running".
fn is_lock_held(e: &std::fs::TryLockError) -> bool {
    matches!(e, std::fs::TryLockError::WouldBlock)
}

/// Create the private directory the endpoint's files live in.
pub fn prepare_dir(endpoint: &Endpoint) -> io::Result<()> {
    std::fs::create_dir_all(&endpoint.dir)?;
    // Best effort: a filesystem that cannot express the mode still works, and the
    // token is the real boundary. Not failing here keeps sshpf usable on those.
    let _ = crate::sys::restrict_dir(&endpoint.dir);
    Ok(())
}

/// Read a small file and trim trailing whitespace/newlines.
fn read_trimmed(path: &std::path::Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// Write a small file by writing a sibling then renaming over the target.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = crate::sys::create_private_file(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Leave nothing behind that a later reader could mistake for state.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// An endpoint pointing into a fresh temp dir, without touching the real
    /// per-user runtime directory.
    fn temp_endpoint(tag: &str) -> (tempfile::TempDir, Endpoint) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut endpoint = super::super::endpoint::endpoint_for(Path::new("/tmp/cfg.toml"));
        endpoint.dir = dir.path().to_path_buf();
        endpoint.port_path = dir.path().join(format!("{tag}.port"));
        endpoint.token_path = dir.path().join(format!("{tag}.token"));
        endpoint.lock_path = dir.path().join(format!("{tag}.lock"));
        (dir, endpoint)
    }

    #[test]
    fn a_bound_port_survives_the_round_trip() {
        let (_d, endpoint) = temp_endpoint("p");
        let (_listener, port) = bind_loopback().expect("loopback is always bindable");
        assert_ne!(port, 0, "an ephemeral bind must produce a real port");
        write_port(&endpoint, port).expect("writing the port file");
        assert_eq!(read_port(&endpoint).expect("reading it back"), port);
    }

    #[test]
    fn a_missing_port_file_is_an_io_error_not_a_default() {
        let (_d, endpoint) = temp_endpoint("missing");
        let err = read_port(&endpoint).expect_err("no file means no server");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_garbage_port_file_is_rejected() {
        let (_d, endpoint) = temp_endpoint("garbage");
        std::fs::write(&endpoint.port_path, "not a number\n").expect("fixture");
        let err = read_port(&endpoint).expect_err("garbage must not dial a default port");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn the_token_survives_the_round_trip_and_has_128_bits() {
        let (_d, endpoint) = temp_endpoint("tok");
        let token = generate_token();
        assert_eq!(token.len(), 32, "32 hex digits is 128 bits: {token}");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");
        write_token(&endpoint, &token).expect("writing the token");
        assert_eq!(read_token(&endpoint).expect("reading it back"), token);
    }

    #[test]
    fn two_tokens_differ() {
        // A repeated token would make the secret useless across restarts.
        assert_ne!(generate_token(), generate_token());
    }

    #[test]
    fn the_singleton_lock_admits_exactly_one_holder() {
        let (_d, endpoint) = temp_endpoint("lock");
        let first = try_become_singleton(&endpoint.lock_path).expect("the first attempt must not error");
        assert!(first.is_some(), "an unlocked file must be acquirable");
        // A second handle in the same process is still a second holder: both
        // platforms lock per open file, which is what the integration test relies
        // on when it runs two releases of the binary.
        let second = try_become_singleton(&endpoint.lock_path).expect("must report, not error");
        assert!(second.is_none(), "a held lock must not be acquirable twice");
    }

    #[test]
    fn dropping_the_lock_releases_it() {
        let (_d, endpoint) = temp_endpoint("released");
        let held = try_become_singleton(&endpoint.lock_path).expect("acquire");
        assert!(held.is_some());
        drop(held);
        let again = try_become_singleton(&endpoint.lock_path).expect("re-acquire");
        assert!(again.is_some(), "process death / drop must free the lock");
    }

    #[test]
    fn an_unlocked_endpoint_reports_no_live_server() {
        // The first-run / post-crash case: nothing serves this config, so a client
        // must be told to start one.
        let (_d, endpoint) = temp_endpoint("nobody");
        assert!(no_live_server(&endpoint).expect("a usable lock must answer"), "nothing is serving");
    }

    #[test]
    fn a_held_lock_reports_a_live_server() {
        // A server holds this for its whole life, so the answer must be "someone is
        // already serving" and the caller must not spawn a second one.
        let (_d, endpoint) = temp_endpoint("someone");
        let held = try_become_singleton(&endpoint.lock_path).expect("acquire");
        assert!(held.is_some(), "the test must actually hold the lock");
        assert!(!no_live_server(&endpoint).expect("a usable lock must answer"), "a held lock means someone serves");
    }

    /// The probe must not keep the lock it took, or the server the caller then spawns
    /// would lose its own election and exit, leaving the client with nothing.
    #[test]
    fn the_no_live_server_probe_releases_the_lock_it_borrowed() {
        let (_d, endpoint) = temp_endpoint("borrow");
        assert!(no_live_server(&endpoint).expect("first probe"));
        let after = try_become_singleton(&endpoint.lock_path).expect("the probe must have released it");
        assert!(after.is_some(), "a borrowed lock must be dropped before returning");
    }

    #[test]
    fn a_server_is_reachable_through_the_port_file() {
        use std::io::BufRead;
        let (_d, endpoint) = temp_endpoint("live");
        let (listener, port) = bind_loopback().expect("bind");
        write_port(&endpoint, port).expect("publish");
        // Accept on a thread so the connect cannot deadlock the test.
        let handle = std::thread::spawn(move || {
            let (mut accepted, _) = listener.accept().expect("the client must arrive");
            let mut line = String::new();
            let _ = std::io::BufReader::new(&mut accepted).read_line(&mut line);
            line
        });
        let mut client = connect(&endpoint).expect("connect through discovery");
        client.write_all(b"ping\n").expect("write");
        let got = handle.join().expect("the accept thread must finish");
        assert_eq!(got, "ping\n");
    }

    #[test]
    fn preparing_the_directory_is_idempotent() {
        let (_d, mut endpoint) = temp_endpoint("dir");
        endpoint.dir = endpoint.dir.join("nested");
        prepare_dir(&endpoint).expect("first creation");
        prepare_dir(&endpoint).expect("second creation must not fail");
        assert!(endpoint.dir.is_dir());
    }

    #[test]
    fn removing_the_files_leaves_the_lock_alone() {
        let (_d, endpoint) = temp_endpoint("cleanup");
        std::fs::write(&endpoint.port_path, "1").expect("fixture");
        std::fs::write(&endpoint.token_path, "t").expect("fixture");
        std::fs::write(&endpoint.lock_path, "").expect("fixture");
        remove_files(&endpoint);
        assert!(!endpoint.port_path.exists());
        assert!(!endpoint.token_path.exists());
        assert!(endpoint.lock_path.exists(), "the lock file is harmless and is kept");
    }
}
