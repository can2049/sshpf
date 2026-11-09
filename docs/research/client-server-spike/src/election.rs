//! Assertion 1 + 2: singleton election and stale-socket recovery.
//!
//! What belongs here: two claims that the whole process model rests on.
//!
//! 1. **Exactly one server wins.** N processes race for the same lock; exactly
//!    one acquires it, the losers exit cleanly. If this were not true, two
//!    servers would fight over the same forwarding ports.
//! 2. **A killed server leaves recoverable state.** After `SIGKILL` the socket
//!    file survives but nobody listens; the next process must be able to tell
//!    that apart from "a live server is starting up" and reclaim it.
//!
//! What does not: how a client picks a socket path, or what a snapshot holds.
//!
//! Run with `cargo run --bin election`.

#[path = "common/mod.rs"]
mod common;

use std::fs::{File, OpenOptions};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::assert::{check, finish};

/// Exit code a losing process uses: "another server already owns this identity".
const EXIT_LOST: i32 = 3;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        // Child modes re-execute this same binary.
        Some("child") => child_hold_lock(args.get(2)),
        Some("holder") => child_hold_socket(args.get(2)),
        _ => parent(),
    };
    std::process::exit(code);
}

/// Parent: run both assertions and report.
fn parent() -> i32 {
    let dir = work_dir();
    println!("work dir: {}\n", dir.display());

    election_round(&dir);
    stale_socket_round(&dir);

    finish("election")
}

/// A fresh, emptied scratch directory under the OS temp dir (idempotent runs).
fn work_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push("sshpf-cs-spike-election");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// Assertion 1: exactly one winner among N racing processes
// ---------------------------------------------------------------------------

fn election_round(dir: &Path) {
    const N: usize = 8;
    let lock = dir.join("election.lock");
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            check("election: locate own executable", false, &e.to_string());
            return;
        }
    };

    // Start every racer before any of them can win, so they genuinely overlap.
    let mut children = Vec::new();
    for _ in 0..N {
        match Command::new(&exe).arg("child").arg(&lock).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
            Ok(c) => children.push(c),
            Err(e) => {
                check("election: spawn racers", false, &e.to_string());
                return;
            }
        }
    }

    let mut winners = 0usize;
    let mut clean_losers = 0usize;
    for child in children {
        match child.wait_with_output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout).to_string();
                if text.contains("WON") {
                    winners += 1;
                } else if out.status.code() == Some(EXIT_LOST) {
                    clean_losers += 1;
                }
            }
            Err(e) => check("election: collect racer", false, &e.to_string()),
        }
    }

    println!("racers={N} winners={winners} clean_losers={clean_losers}");
    check("election: exactly one process acquires the lock", winners == 1, &format!("winners={winners}"));
    check(
        "election: every loser exits with the agreed code",
        clean_losers == N - 1,
        &format!("losers={clean_losers}/{}", N - 1),
    );
}

/// Child: try to acquire the lock, hold it briefly, report the verdict.
fn child_hold_lock(lock: Option<&String>) -> i32 {
    let Some(lock) = lock else {
        return EXIT_LOST;
    };
    let file = match OpenOptions::new().create(true).read(true).write(true).open(lock) {
        Ok(f) => f,
        Err(_) => return EXIT_LOST,
    };
    match file.try_lock() {
        Ok(()) => {
            println!("WON");
            // Hold long enough that the other racers really do collide with us.
            std::thread::sleep(Duration::from_millis(250));
            0
        }
        Err(_) => EXIT_LOST,
    }
}

// ---------------------------------------------------------------------------
// Assertion 2: a SIGKILLed server leaves a socket that the next process reclaims
// ---------------------------------------------------------------------------

fn stale_socket_round(dir: &Path) {
    let socket = dir.join("stale.sock");
    let lock = dir.join("stale.lock");
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            check("stale: locate own executable", false, &e.to_string());
            return;
        }
    };

    // A "server" that binds the socket and holds the lock until killed.
    let mut holder = match Command::new(&exe).arg("holder").arg(dir).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    {
        Ok(c) => c,
        Err(e) => {
            check("stale: spawn holder", false, &e.to_string());
            return;
        }
    };

    if !wait_for(|| socket.exists(), Duration::from_secs(5)) {
        let _ = holder.kill();
        check("stale: holder created the socket", false, "timeout");
        return;
    }

    // Killing the holder is the scenario: no cleanup code runs, so the socket
    // file remains and the lock is released by the kernel.
    let _ = holder.kill();
    let _ = holder.wait();

    check("stale: socket file survives SIGKILL", socket.exists(), "");

    // A live listener is gone, so a connect must be refused (not hang).
    let connect_failed =
        matches!(UnixStream::connect(&socket), Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused);
    check("stale: connect to the leftover socket is refused", connect_failed, "");

    // Reclaim: taking the lock is what proves no live server exists. Only then
    // is unlinking safe — the other order could delete a path a starting server
    // is about to bind.
    let lock_file = match OpenOptions::new().create(true).read(true).write(true).open(&lock) {
        Ok(f) => f,
        Err(e) => {
            check("stale: open the election lock", false, &e.to_string());
            return;
        }
    };
    let acquired = lock_file.try_lock().is_ok();
    check("stale: the lock is free after SIGKILL", acquired, "");

    let removed = std::fs::remove_file(&socket).is_ok();
    let rebind = UnixListener::bind(&socket).is_ok();
    check("stale: unlink then bind succeeds", removed && rebind, &format!("removed={removed} rebind={rebind}"));
}

/// Child: bind the socket, take the lock, and stay alive until killed.
fn child_hold_socket(dir: Option<&String>) -> i32 {
    let Some(dir) = dir else {
        return EXIT_LOST;
    };
    let dir = Path::new(dir);
    let socket = dir.join("stale.sock");
    let lock = dir.join("stale.lock");
    let Ok(_listener) = UnixListener::bind(&socket) else {
        return EXIT_LOST;
    };
    let Ok(lock_file) = OpenOptions::new().create(true).read(true).write(true).open(&lock) else {
        return EXIT_LOST;
    };
    if lock_file.try_lock().is_err() {
        return EXIT_LOST;
    }
    println!("HOLDING");
    // Keep both handles alive until the parent kills us.
    std::thread::sleep(Duration::from_secs(60));
    0
}

/// Poll `cond` until it holds or the deadline passes.
fn wait_for(cond: impl Fn() -> bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

// Keep the `File` import meaningful on all cfg paths of this bin.
#[allow(unused_imports)]
use File as _FileAlias;
