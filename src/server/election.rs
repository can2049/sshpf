//! Becoming *the* server for a config: take the lock, clear a stale socket, bind.
//!
//! # The one invariant this file exists to keep
//!
//! One config has exactly one live server. Two servers would each bind the same
//! local ports, so the second one's `bind` fails and the user sees a random
//! half of their forwards work — the worst kind of failure, because it looks like
//! a network problem.
//!
//! The design carries this with no pid file, no heartbeat and no timeout probing:
//!
//! * a live server **holds the election lock for its whole lifetime**;
//! * the kernel releases that lock when the process dies, `SIGKILL` included.
//!
//! So "can I take the lock?" is exactly "is there a server for this config?".
//!
//! # Why the lock comes before the unlink
//!
//! A leftover socket file is only *provably* stale if no server owns the config,
//! and the lock is that proof. Unlinking first would let a loser delete the socket
//! of a server that was mid-startup — turning a harmless race into a broken
//! endpoint. The order (lock → unlink → bind) is therefore a safety property, not
//! a style choice.
//!
//! Belongs here: the lock, stale-socket recovery, and binding the listener with
//! private permissions.
//! Does not belong: who connects ([`super::session`]) or when to stop
//! ([`super::lifecycle`]).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::eventlog;
use crate::ipc::Endpoint;
use crate::logging;

/// How long to keep retrying the lock before concluding a server is alive.
///
/// A few retries absorb the moment when a previous server is still unlinking, but
/// a live server holds the lock for hours — so the budget must be short. The
/// answer after it runs out is not "wait longer", it is "someone else is serving".
const LOCK_BUDGET: Duration = Duration::from_secs(2);

/// Delay between lock attempts.
const LOCK_RETRY: Duration = Duration::from_millis(25);

/// What the caller should do after trying to become the server.
pub enum Election {
    /// This process owns the config. The lock must be kept alive for the whole
    /// process: dropping it is what would let a rival start.
    Won { lock: File, listener: UnixListener },
    /// Another process serves this config. Exit quietly: this is the normal
    /// outcome of an optimistic spawn, not a fault.
    Lost,
}

/// Win the election for `endpoint`, bind its socket, and hand back both.
///
/// Returns `Err` only for a real I/O problem (a directory that cannot be created,
/// a socket path that is not a socket); "someone else is serving" is
/// [`Election::Lost`].
pub fn elect(endpoint: &Endpoint) -> std::io::Result<Election> {
    create_private_dir(&endpoint.dir)?;
    let Some(lock) = acquire_lock(&endpoint.lock_path) else {
        eventlog::server_election_lost(&endpoint.lock_path.display().to_string());
        return Ok(Election::Lost);
    };
    // Holding the lock means no server is alive, so any socket file here is the
    // remains of a `SIGKILL`ed one. Remove it before binding, never before.
    if endpoint.socket_path.exists() && std::fs::remove_file(&endpoint.socket_path).is_ok() {
        eventlog::server_stale_socket_removed(&endpoint.socket_path.display().to_string());
    }
    let listener = UnixListener::bind(&endpoint.socket_path)?;
    // The umask decides what `bind` created (775 was measured), and the socket is
    // the only access boundary there is, so set it explicitly.
    if let Err(e) = std::fs::set_permissions(&endpoint.socket_path, std::fs::Permissions::from_mode(0o600)) {
        logging::warn("server.socket_chmod_failed", &[("msg", &e.to_string())]);
    }
    listener.set_nonblocking(true)?;
    Ok(Election::Won { lock, listener })
}

/// Create the endpoint directory with owner-only permissions.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Take the lock, retrying briefly; `None` when someone else holds it.
fn acquire_lock(path: &Path) -> Option<File> {
    let deadline = Instant::now() + LOCK_BUDGET;
    loop {
        // `0600`: the lock names nothing, but keeping it private matches the
        // directory and avoids surprising a security review.
        // `truncate(false)` is deliberate: the lock file carries no contents, and
        // truncating it would be a pointless write racing with another lock
        // attempt. Only the `flock` on it matters.
        match OpenOptions::new().create(true).write(true).truncate(false).mode(0o600).open(path) {
            Ok(file) => match file.try_lock() {
                Ok(()) => return Some(file),
                // Another process is serving (or just started doing so).
                Err(std::fs::TryLockError::WouldBlock) => {}
                // A real filesystem problem: reported by the caller as an error.
                Err(std::fs::TryLockError::Error(e)) => {
                    logging::error("server.lock_failed", &[("msg", &e.to_string())]);
                    return None;
                }
            },
            Err(e) => {
                logging::error("server.lock_open_failed", &[("msg", &e.to_string())]);
                return None;
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(LOCK_RETRY);
    }
}

/// Remove the socket file on the way out, so the next start is the happy path.
pub fn unlink_socket(endpoint: &Endpoint) {
    match std::fs::remove_file(&endpoint.socket_path) {
        Ok(()) => {}
        // Already gone is exactly what we wanted; anything else is worth a line.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => logging::warn("server.socket_unlink_failed", &[("msg", &e.to_string())]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::endpoint_for;

    fn endpoint(dir: &Path) -> Endpoint {
        endpoint_for(&dir.join("config.toml"))
    }

    #[test]
    fn the_first_election_wins_and_binds_a_private_socket() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path());
        let Election::Won { lock, listener } = elect(&ep).unwrap() else {
            panic!("the first election must win");
        };
        let mode = std::fs::metadata(&ep.socket_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the socket is the only access boundary, so it must be private");
        // Keep both alive to the end of the test.
        drop(listener);
        drop(lock);
    }

    #[test]
    fn a_second_election_loses_while_the_first_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path());
        let Election::Won { lock, listener } = elect(&ep).unwrap() else {
            panic!("the first election must win");
        };
        // Same process, second handle: `flock` is per open file, so this really
        // does contend the way a second process would.
        assert!(matches!(elect(&ep).unwrap(), Election::Lost), "a rival must not become a second server");
        drop(listener);
        drop(lock);
    }

    #[test]
    fn the_lock_is_released_when_its_handle_drops() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path());
        let Election::Won { lock, listener } = elect(&ep).unwrap() else {
            panic!("the first election must win");
        };
        drop(listener);
        drop(lock);
        // The lock is free again, so the next start wins instead of giving up.
        assert!(matches!(elect(&ep).unwrap(), Election::Won { .. }), "a free lock must be winnable");
    }

    #[test]
    fn a_stale_socket_file_is_replaced_by_the_winner() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path());
        std::fs::create_dir_all(&ep.dir).unwrap();
        // Simulate a `SIGKILL`ed server: a socket file nothing is listening on.
        let dead = UnixListener::bind(&ep.socket_path).unwrap();
        drop(dead);

        let Election::Won { lock, listener } = elect(&ep).unwrap() else {
            panic!("a stale socket must not block the election");
        };
        assert!(std::os::unix::net::UnixStream::connect(&ep.socket_path).is_ok(), "the new socket must work");
        drop(listener);
        drop(lock);
        assert_eq!(std::fs::metadata(&ep.lock_path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn the_endpoint_directory_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let ep = endpoint(dir.path());
        let _ = elect(&ep).unwrap();
        let mode = std::fs::metadata(&ep.dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the directory must not be reachable by other users");
    }
}
