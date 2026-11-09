//! The spike's server process: a real `UnixListener`, a real election lock, a
//! bounded per-session queue, and the grace-window shutdown.
//!
//! What belongs here: accepting sessions, recounting clients, broadcasting the
//! snapshot, and deciding to exit. What does not: the parent's orchestration
//! (`e2e`) or what a client does with a snapshot (`e2e/client`).
//!
//! Two rules are the whole point and each lives in exactly one place:
//! [`State::snapshot`] defines the authoritative view, and [`shutdown_loop`] is
//! the only thing that decides to stop. `Bye { last_exit }` deliberately does
//! **not** appear in that decision.

use std::fs::{File, OpenOptions};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::common::frame::{read_frame, write_frame};
use crate::common::msg::ClientMsg;
use crate::common::wire::Snapshot;

/// Per-session outbound queue depth: full means "disconnect", never "block".
const QUEUE_CAPACITY: usize = 8;
/// How long the server waits after the client count reaches zero.
const GRACE: Duration = Duration::from_millis(400);
/// Number of tunnels the spike's snapshot pretends to hold.
const FAKE_TUNNELS: usize = 3;

/// The authoritative state: revision, client count, and uptime.
pub struct State {
    pub revision: AtomicU64,
    pub clients: AtomicUsize,
    started: Instant,
}

impl State {
    fn new() -> Self {
        State { revision: AtomicU64::new(0), clients: AtomicUsize::new(0), started: Instant::now() }
    }

    /// The snapshot every session receives: one builder, one definition of truth.
    pub fn snapshot(&self) -> Snapshot {
        let mut snap = Snapshot::sample(self.revision.load(Ordering::SeqCst), FAKE_TUNNELS);
        snap.client_count = self.clients.load(Ordering::SeqCst) as u32;
        snap.server_now_ms = self.started.elapsed().as_millis() as u64;
        snap
    }
}

/// One registered session's identity plus its outbound queue.
struct Peer {
    id: u64,
    tx: SyncSender<String>,
}

/// The session registry. `depart` must remove *this* session's queue, so peers
/// carry an id rather than relying on stack order.
type Registry = Arc<Mutex<Vec<Peer>>>;

/// Run the server until the last client has been gone for [`GRACE`].
///
/// Returns 0 when it stopped because the last client left, 3 when another server
/// already owned this identity (the losing side of an election).
pub fn run(dir: &Path) -> i32 {
    let socket = dir.join("server.sock");
    let lock = dir.join("server.lock");

    // Election first. A live server holds this for its whole lifetime, so losing
    // means "someone else is already serving" — exit quietly rather than fight
    // over the socket path.
    let Ok(lock_file) = open_lock(&lock) else {
        return 3;
    };
    if lock_file.try_lock().is_err() {
        return 3;
    }

    // Safe to unlink now: we hold the lock, so any existing socket is stale.
    let _ = std::fs::remove_file(&socket);
    let Ok(listener) = UnixListener::bind(&socket) else {
        return 3;
    };
    // Non-blocking accept, so the shutdown decision is not stuck waiting for a
    // connection that will never come.
    if listener.set_nonblocking(true).is_err() {
        return 3;
    }
    let _ = std::fs::write(dir.join("ready"), "1");

    let state = Arc::new(State::new());
    let registry: Registry = Arc::new(Mutex::new(Vec::new()));
    let stopping = Arc::new(AtomicUsize::new(0));
    let next_id = Arc::new(AtomicU64::new(0));

    {
        let (state, stopping, dir) = (state.clone(), stopping.clone(), dir.to_path_buf());
        std::thread::spawn(move || shutdown_loop(&state, &stopping, &dir));
    }

    while stopping.load(Ordering::SeqCst) == 0 {
        match listener.accept() {
            Ok((stream, _)) => {
                let (tx, rx) = sync_channel::<String>(QUEUE_CAPACITY);
                let id = next_id.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut list) = registry.lock() {
                    list.push(Peer { id, tx });
                }
                // Attaching changes `client_count`, so every client must be told;
                // this is also why revisions strictly increase as peers join.
                state.clients.fetch_add(1, Ordering::SeqCst);
                state.revision.fetch_add(1, Ordering::SeqCst);
                broadcast(&registry, &state);

                let (state_t, reg_t) = (state.clone(), registry.clone());
                std::thread::spawn(move || session(id, stream, state_t, reg_t, rx));
            }
            // No pending connection: this is where the loop makes progress.
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    // Shutdown order: drop the socket first (no new clients), then the lock.
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::write(dir.join("stopped"), "1");
    let _ = lock_file.unlock();
    0
}

/// The one place that decides to stop: count reaches zero, stays zero for
/// [`GRACE`], then records the reason and signals the accept loop.
fn shutdown_loop(state: &Arc<State>, stopping: &Arc<AtomicUsize>, dir: &Path) {
    let mut zero_since: Option<Instant> = None;
    loop {
        if stopping.load(Ordering::SeqCst) == 1 {
            return;
        }
        if state.clients.load(Ordering::SeqCst) == 0 {
            let since = *zero_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= GRACE {
                let _ = std::fs::write(dir.join("server-reason.txt"), "last_client");
                stopping.store(1, Ordering::SeqCst);
                return;
            }
        } else {
            // A client came back inside the window: cancel the shutdown.
            zero_since = None;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// One session: greet it with the current snapshot, then apply what it sends.
fn session(id: u64, mut stream: UnixStream, state: Arc<State>, registry: Registry, rx: Receiver<String>) {
    let Ok(mut out) = stream.try_clone() else {
        depart(id, &state, &registry);
        return;
    };
    // The writer thread is the only code that writes to this socket, so a peer
    // that stops reading blocks only its own writer.
    let writer = std::thread::spawn(move || {
        while let Ok(payload) = rx.recv() {
            if write_frame(&mut out, &payload).is_err() {
                return;
            }
        }
    });

    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    loop {
        match read_frame(&mut stream) {
            Ok(Some(payload)) => match toml::from_str::<ClientMsg>(&payload) {
                Ok(ClientMsg::Hello { .. }) => {
                    // The welcome is the current state, so a client never
                    // renders an empty table on connect.
                    let Ok(text) = toml::to_string(&state.snapshot()) else {
                        break;
                    };
                    if write_frame(&mut stream, &text).is_err() {
                        break;
                    }
                }
                Ok(ClientMsg::Command { .. }) => {
                    state.revision.fetch_add(1, Ordering::SeqCst);
                    broadcast(&registry, &state);
                }
                // `last_exit` says the user was warned; it is not acted on here.
                // `depart` recounts, and only the authoritative count can stop
                // the server — a stale claim must never destroy peers' forwards.
                Ok(ClientMsg::Bye { .. }) | Err(_) => break,
            },
            // Clean EOF: the client went away, politely or otherwise.
            Ok(None) => break,
            // A read timeout is not a disconnect; keep waiting.
            Err(e) if is_timeout(&e) => {}
            Err(_) => break,
        }
        if writer.is_finished() {
            break;
        }
    }

    depart(id, &state, &registry);
    drop(writer);
}

/// Remove exactly this session and tell the survivors.
fn depart(id: u64, state: &Arc<State>, registry: &Registry) {
    let removed = match registry.lock() {
        Ok(mut list) => {
            let before = list.len();
            list.retain(|p| p.id != id);
            list.len() != before
        }
        Err(_) => false,
    };
    // Only the session that actually owned a registration decrements, so a
    // double depart cannot drive the count negative.
    if removed {
        state.clients.fetch_sub(1, Ordering::SeqCst);
        state.revision.fetch_add(1, Ordering::SeqCst);
        broadcast(registry, state);
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

/// Fan out one snapshot to every session; drop any peer that cannot keep up.
fn broadcast(registry: &Registry, state: &Arc<State>) {
    let Ok(payload) = toml::to_string(&state.snapshot()) else {
        return;
    };
    let Ok(mut list) = registry.lock() else {
        return;
    };
    // `retain` plus a fallible send is the entire back-pressure policy: a full
    // queue or a dead writer means the peer is gone, and keeping it would let
    // one stalled client dictate the pace of every other client.
    list.retain(|peer| peer.tx.try_send(payload.clone()).is_ok());
}

fn open_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).read(true).write(true).open(path)
}
