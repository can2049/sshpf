//! Assertion 9: a real server process and real client processes agree, and the
//! last client leaving really does stop the server.
//!
//! What belongs here: only the orchestration of the end-to-end probe — spawning
//! OS processes, driving them in a chosen order, and comparing what they wrote
//! down. What does not: the server's bookkeeping (`e2e/server`) or a client's
//! behaviour (`e2e/client`).
//!
//! Why this exists: `broadcast` and `lifecycle` prove the *logic* with threads
//! and an in-memory model, which cannot show that the rules survive a real
//! process boundary — separate address spaces, real `flock`, real socket EOF.
//! This binary closes that gap, including the stale-`last_exit` race driven
//! across three distinct processes.
//!
//! Run with `cargo run --bin e2e`.

#[path = "common/mod.rs"]
mod common;

#[path = "e2e/client.rs"]
mod client;
#[path = "e2e/server.rs"]
mod server;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::assert::{check, finish};

/// How long a graceful server shutdown may take once the last client leaves.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        // Child modes re-execute this same binary.
        Some("server") => args.get(2).map_or(3, |d| server::run(Path::new(d))),
        Some("client") => match (args.get(2), args.get(3)) {
            (Some(d), Some(name)) => client::run(Path::new(d), name),
            _ => 3,
        },
        _ => orchestrate(),
    };
    std::process::exit(code);
}

fn orchestrate() -> i32 {
    let dir = work_dir();
    println!("work dir: {}\n", dir.display());
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            check("e2e: locate own executable", false, &e.to_string());
            return finish("e2e");
        }
    };

    consistency_and_last_exit(&dir, &exe);
    println!();
    stale_last_exit_across_processes(&dir, &exe);
    finish("e2e")
}

fn work_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push("sshpf-cs-spike-e2e");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// Assertion 9a: three real clients converge; leaving one at a time ends with the
// last one stopping the server.
// ---------------------------------------------------------------------------

fn consistency_and_last_exit(dir: &Path, exe: &Path) {
    let round = dir.join("a");
    let _ = std::fs::create_dir_all(&round);
    let mut srv = match spawn_server(exe, &round) {
        Some(c) => c,
        None => {
            check("e2e: spawn the server process", false, "");
            return;
        }
    };
    if !wait_for_file(&round.join("ready"), Duration::from_secs(10)) {
        let _ = srv.kill();
        check("e2e: the server reports ready", false, "timeout");
        return;
    }
    check("e2e: a real server process binds the socket and holds the lock", true, "");

    // Three genuine client processes.
    let mut clients = Vec::new();
    for i in 0..3 {
        match spawn_client(exe, &round, &format!("c{i}")) {
            Some((child, stdin)) => clients.push((format!("c{i}"), child, stdin)),
            None => {
                check("e2e: spawn a client process", false, &format!("c{i}"));
                let _ = srv.kill();
                return;
            }
        }
    }
    // Every client waits until it has seen all three peers attached.
    for (_, _, stdin) in clients.iter_mut() {
        send(stdin, "wait");
    }
    let all_saw_three =
        clients.iter().all(|(name, _, _)| wait_for_file(&round.join(format!("{name}.saw3")), Duration::from_secs(15)));
    check("e2e: every real client observes all three clients attached", all_saw_three, "");

    // The clients must agree on the final snapshot they each received.
    for (_, _, stdin) in clients.iter_mut() {
        send(stdin, "hold");
    }
    let snapshots: Vec<String> =
        (0..3).map(|i| read_trim(&round.join(format!("c{i}.snap"))).unwrap_or_default()).collect();
    let all_agree = !snapshots[0].is_empty() && snapshots.iter().all(|s| s == &snapshots[0]);
    check("e2e: every real client's snapshot is identical", all_agree, &snapshots[0]);

    let strictly_increasing = (0..3).all(|i| read_trim(&round.join(format!("c{i}.incr"))).as_deref() == Some("true"));
    check("e2e: every real client saw strictly increasing revisions", strictly_increasing, "");

    // Two clients leave politely; the server must not exit yet.
    for (name, _, stdin) in clients.iter_mut().take(2) {
        send(stdin, "bye");
        let _ = wait_for_file(&round.join(format!("{name}.done")), Duration::from_secs(5));
    }
    std::thread::sleep(Duration::from_millis(700));
    let still_alive = srv.try_wait().map(|s| s.is_none()).unwrap_or(false);
    check("e2e: the server keeps running while one real client remains", still_alive, "");

    // The last client leaves; the server must shut down on its own.
    if let Some((name, _, stdin)) = clients.last_mut() {
        send(stdin, "bye_last");
        let _ = wait_for_file(&round.join(format!("{name}.done")), Duration::from_secs(5));
    }
    let exited = wait_child_exit(&mut srv, SHUTDOWN_BUDGET);
    check("e2e: the server process exits after the last client leaves", exited, "");
    let reason = read_trim(&round.join("server-reason.txt")).unwrap_or_default();
    check("e2e: the server records why it exited", reason == "last_client", &reason);
    let _ = srv.kill();
}

// ---------------------------------------------------------------------------
// Assertion 9b: the stale-last-exit race, across three real processes.
// ---------------------------------------------------------------------------

/// The interleaving, with every participant a separate OS process:
///
/// ```text
/// t0  server up, client A attached (client_count == 1)
/// t1  A records that it saw count == 1        <- it would show the warning now
/// t2  client B connects (count becomes 2)
/// t3  A sends Bye{last_exit: true}            <- carrying the stale observation
/// t4  server recounts: 2 - 1 = 1 > 0  ->  must NOT exit
/// t5  B leaves -> count == 0 -> server exits
/// ```
fn stale_last_exit_across_processes(dir: &Path, exe: &Path) {
    let round = dir.join("b");
    let _ = std::fs::create_dir_all(&round);
    let mut srv = match spawn_server(exe, &round) {
        Some(c) => c,
        None => {
            check("e2e-race: spawn the server process", false, "");
            return;
        }
    };
    if !wait_for_file(&round.join("ready"), Duration::from_secs(10)) {
        let _ = srv.kill();
        check("e2e-race: the server reports ready", false, "timeout");
        return;
    }

    let (mut a, mut a_in) = match spawn_client(exe, &round, "a") {
        Some(p) => p,
        None => {
            let _ = srv.kill();
            check("e2e-race: spawn client A", false, "");
            return;
        }
    };
    // t1: A sees that it is the only client — the condition that shows the warning.
    send(&mut a_in, "wait");
    let a_saw_one = wait_for_file(&round.join("a.solo"), Duration::from_secs(10));
    check("e2e-race: client A observes that it is the only client", a_saw_one, "");

    // t2: B connects before A's Bye is sent.
    let (mut b, mut b_in) = match spawn_client(exe, &round, "b") {
        Some(p) => p,
        None => {
            let _ = a.kill();
            let _ = srv.kill();
            check("e2e-race: spawn client B", false, "");
            return;
        }
    };
    send(&mut b_in, "wait");
    let b_seen = wait_for_file(&round.join("b.saw2"), Duration::from_secs(10));
    check("e2e-race: client B attaches before A says goodbye", b_seen, "");

    // t3: A leaves with the now-stale claim.
    send(&mut a_in, "bye_last");
    let _ = wait_for_file(&round.join("a.done"), Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(900));

    // t4: the server must still be running, because B is still attached.
    let survived_stale_claim = srv.try_wait().map(|s| s.is_none()).unwrap_or(false);
    check("e2e-race: a stale last_exit claim does NOT stop the server (B is still attached)", survived_stale_claim, "");

    // t5: B leaves for real; now the server exits.
    send(&mut b_in, "bye_last");
    let _ = wait_for_file(&round.join("b.done"), Duration::from_secs(5));
    let exited = wait_child_exit(&mut srv, SHUTDOWN_BUDGET);
    check("e2e-race: the server exits once the genuine last client leaves", exited, "");
    let reason = read_trim(&round.join("server-reason.txt")).unwrap_or_default();
    check("e2e-race: the server's exit reason is the real last client", reason == "last_client", &reason);

    let _ = a.kill();
    let _ = b.kill();
    let _ = srv.kill();
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

fn spawn_server(exe: &Path, round: &Path) -> Option<Child> {
    Command::new(exe).arg("server").arg(round).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()
}

fn spawn_client(exe: &Path, round: &Path, name: &str) -> Option<(Child, std::process::ChildStdin)> {
    let mut child = Command::new(exe)
        .arg("client")
        .arg(round)
        .arg(name)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdin = child.stdin.take()?;
    Some((child, stdin))
}

/// Send one command line to a client's stdin.
fn send(stdin: &mut std::process::ChildStdin, line: &str) {
    use std::io::Write;
    let _ = writeln!(stdin, "{line}");
    let _ = stdin.flush();
}

/// Poll until `child` exits, returning whether it did inside `budget`.
fn wait_child_exit(child: &mut Child, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
    false
}

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn wait_for_file(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}
