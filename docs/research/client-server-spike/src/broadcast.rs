//! Assertion 3 + 4: snapshot broadcast consistency and slow-consumer isolation.
//!
//! What belongs here: the two properties that make "every client sees the same
//! thing" true, and the one that keeps one bad client from taking the server
//! down with it.
//!
//! 3. **Every client agrees.** Across many revisions each client's `revision`
//!    increases strictly, and the final snapshot is identical everywhere.
//! 4. **A client that stops reading cannot stall the others.** The server fans
//!    out through a bounded per-session queue and a full queue means
//!    "disconnect", not "block". The probe models that queue directly rather
//!    than relying on kernel socket buffers (which differ per OS and would make
//!    the claim unreproducible).
//!
//! What does not: how the server is elected, or when it shuts down.
//!
//! Run with `cargo run --bin broadcast`.

#[path = "common/mod.rs"]
mod common;

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::assert::{check, finish};
use common::frame::{read_frame, write_frame};
use common::wire::Snapshot;

/// How many state changes the server publishes during the consistency test.
const REVISIONS: u64 = 100;

fn main() {
    let dir = work_dir();
    println!("work dir: {}\n", dir.display());
    consistency_round(&dir);
    println!();
    slow_consumer_round(&dir);
    std::process::exit(finish("broadcast"));
}

fn work_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push("sshpf-cs-spike-broadcast");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A fresh bound listener at `socket`, replacing any leftover file.
fn bind(socket: &Path) -> Result<UnixListener, String> {
    let _ = std::fs::remove_file(socket);
    UnixListener::bind(socket).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Assertion 3: all clients converge on identical state
// ---------------------------------------------------------------------------

fn consistency_round(dir: &Path) {
    let socket = dir.join("broadcast.sock");
    let listener = match bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            check("broadcast: bind listener", false, &e);
            return;
        }
    };

    const CLIENTS: usize = 3;
    let accepted = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicU64::new(0));

    // One reader thread per accepted client: it records revisions as they arrive.
    let acceptor = {
        let accepted = accepted.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            let mut handles = Vec::new();
            for _ in 0..CLIENTS {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted.fetch_add(1, Ordering::SeqCst);
                        let done = done.clone();
                        handles.push(std::thread::spawn(move || collect_revisions(stream, done)));
                    }
                    Err(_) => break,
                }
            }
            handles
        })
    };

    let mut clients = Vec::new();
    for _ in 0..CLIENTS {
        match UnixStream::connect(&socket) {
            Ok(s) => clients.push(s),
            Err(e) => {
                check("broadcast: connect client", false, &e.to_string());
                return;
            }
        }
    }
    // Publish only once everyone is registered, so a missed early revision
    // cannot be blamed on scheduling.
    wait_for(|| accepted.load(Ordering::SeqCst) >= CLIENTS, Duration::from_secs(5));

    let started = Instant::now();
    let mut last = Snapshot::default();
    for rev in 1..=REVISIONS {
        let snap = Snapshot { client_count: CLIENTS as u32, ..Snapshot::sample(rev, 10) };
        let payload = match toml::to_string(&snap) {
            Ok(p) => p,
            Err(e) => {
                check("broadcast: encode snapshot", false, &e.to_string());
                return;
            }
        };
        for c in clients.iter_mut() {
            if write_frame(c, &payload).is_err() {
                check("broadcast: write snapshot", false, &format!("rev={rev}"));
                return;
            }
        }
        last = snap;
    }
    let publish_ms = started.elapsed().as_millis();
    std::thread::sleep(Duration::from_millis(200));

    // Tell the readers the stream is over, then collect their verdicts.
    for c in clients.iter_mut() {
        let _ = c.shutdown(std::net::Shutdown::Write);
    }
    done.store(1, Ordering::SeqCst);
    let results = match acceptor.join() {
        Ok(hs) => hs.into_iter().filter_map(|h| h.join().ok()).collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };

    println!("published={REVISIONS} clients={} in {publish_ms}ms", results.len());
    check(
        "broadcast: every client sees strictly increasing revisions",
        results.iter().all(|r| r.strictly_increasing),
        "",
    );
    check(
        "broadcast: every client receives the final revision",
        results.iter().all(|r| r.last_rev == REVISIONS),
        &format!("last_revs={:?}", results.iter().map(|r| r.last_rev).collect::<Vec<_>>()),
    );
    check(
        "broadcast: every client's final snapshot is identical",
        results.iter().all(|r| r.final_digest == last.digest()),
        &last.digest(),
    );
    check(
        "broadcast: 100 revisions publish inside one 200ms frame budget",
        publish_ms < 200,
        &format!("{publish_ms}ms"),
    );

    let _ = std::fs::remove_file(&socket);
}

/// What one client observed.
struct Observed {
    last_rev: u64,
    strictly_increasing: bool,
    final_digest: String,
}

/// Read snapshots until the peer closes, checking revision monotonicity.
fn collect_revisions(mut stream: UnixStream, done: Arc<AtomicU64>) -> Observed {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut last_rev = 0u64;
    let mut strictly_increasing = true;
    let mut final_digest = String::new();

    loop {
        match read_frame(&mut stream) {
            Ok(Some(payload)) => match toml::from_str::<Snapshot>(&payload) {
                Ok(snap) => {
                    if snap.revision <= last_rev {
                        strictly_increasing = false;
                    }
                    last_rev = snap.revision;
                    final_digest = snap.digest();
                }
                Err(_) => break,
            },
            Ok(None) => break,
            Err(_) => {
                if done.load(Ordering::SeqCst) == 1 {
                    break;
                }
            }
        }
    }
    Observed { last_rev, strictly_increasing, final_digest }
}

// ---------------------------------------------------------------------------
// Assertion 4: a stalled client is disconnected, not allowed to block
// ---------------------------------------------------------------------------

/// Depth of each session's outbound queue, mirroring the protocol's bound.
const QUEUE_CAPACITY: usize = 8;

fn slow_consumer_round(dir: &Path) {
    let socket = dir.join("slow.sock");
    let listener = match bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            check("slow: bind listener", false, &e);
            return;
        }
    };

    // Two sessions, each with the bounded queue the protocol specifies.
    // A reader thread drains the good one; nothing ever drains the stalled one —
    // that is exactly "this client stopped reading".
    let (good_tx, good_rx) = sync_channel::<String>(QUEUE_CAPACITY);
    let (stalled_tx, _stalled_rx_kept_alive) = sync_channel::<String>(QUEUE_CAPACITY);

    let received = Arc::new(AtomicUsize::new(0));
    let reader = {
        let received = received.clone();
        std::thread::spawn(move || {
            for _frame in good_rx.iter() {
                received.fetch_add(1, Ordering::SeqCst);
            }
        })
    };

    let payload = match toml::to_string(&Snapshot::sample(1, 50)) {
        Ok(p) => p,
        Err(e) => {
            check("slow: encode a large snapshot", false, &e.to_string());
            return;
        }
    };

    let started = Instant::now();
    let mut good_queued = 0usize;
    let mut stalled_disconnected_at: Option<u128> = None;

    for rev in 1..=400u64 {
        let body = format!("{payload}\nrevision_extra = {rev}\n");
        // Publishing must never block: both sends are `try_send`.
        if good_tx.try_send(body.clone()).is_ok() {
            good_queued += 1;
        }
        if stalled_disconnected_at.is_none() {
            match stalled_tx.try_send(body) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    stalled_disconnected_at = Some(started.elapsed().as_millis());
                }
                Err(TrySendError::Disconnected(_)) => {
                    stalled_disconnected_at = Some(started.elapsed().as_millis());
                }
            }
        }
    }
    let publish_ms = started.elapsed().as_millis();
    drop(good_tx);
    let _ = reader.join();
    let good_received = received.load(Ordering::SeqCst);

    println!(
        "slow-consumer: queue_capacity={QUEUE_CAPACITY} good_queued={good_queued} \
         good_received={good_received} stalled_full_at={:?}ms publish_total={publish_ms}ms",
        stalled_disconnected_at
    );
    check(
        "slow: the good session keeps draining while another is stalled",
        good_received > 0,
        &format!("{good_received} frames"),
    );
    check(
        "slow: publishing 400 frames never blocks on the stalled session",
        publish_ms < 2000,
        &format!("{publish_ms}ms"),
    );
    check(
        "slow: the stalled session hits the queue bound and is dropped",
        stalled_disconnected_at.is_some(),
        &format!("at {:?}", stalled_disconnected_at),
    );

    // A real socket round-trip too, so the claim is not purely about channels:
    // the good client over a UDS keeps receiving while another is not reading.
    let live_good = live_good_client_keeps_reading(&listener, &socket);
    check("slow: a live good client keeps reading over a real socket", live_good, "");

    let _ = std::fs::remove_file(&socket);
}

/// Connect two real clients; drain one and not the other; prove the drained one
/// still makes progress after the stalled one's buffer has been filled.
fn live_good_client_keeps_reading(listener: &UnixListener, socket: &Path) -> bool {
    let good = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let stalled = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let (good_server, _) = match listener.accept() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let (stalled_server, _) = match listener.accept() {
        Ok(p) => p,
        Err(_) => return false,
    };

    // Reader for the good client.
    let got = Arc::new(AtomicUsize::new(0));
    let reader = {
        let got = got.clone();
        std::thread::spawn(move || {
            let mut s = good;
            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match read_frame(&mut s) {
                    Ok(Some(_)) => {
                        got.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
                if got.load(Ordering::SeqCst) >= 50 {
                    break;
                }
            }
        })
    };

    // `stalled` is deliberately never read from.
    let _keep_stalled_open = stalled;
    let payload = match toml::to_string(&Snapshot::sample(1, 50)) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let mut writer = good_server;
    let mut stalled_writer = stalled_server;
    // Keep writing to the stalled peer without blocking forever: once its
    // buffer fills, this probe simply reports the truth and we stop probing.
    let _ = stalled_writer.set_write_timeout(Some(Duration::from_millis(5)));
    for rev in 0..200u64 {
        let body = format!("{payload}\nrevision_extra = {rev}\n");
        if write_frame(&mut writer, &body).is_err() {
            break;
        }
        let _ = write_frame(&mut stalled_writer, &body);
    }
    let _ = writer.shutdown(std::net::Shutdown::Write);
    let _ = reader.join();
    got.load(Ordering::SeqCst) >= 50
}

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
