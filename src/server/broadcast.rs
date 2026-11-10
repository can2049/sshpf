//! Fan-out: sending one snapshot to every attached client.
//!
//! # Why a process-global registry
//!
//! The publisher is reached from two directions — a client's command and the
//! headless event pump — and both hold only a [`Shared`](super::state::Shared)
//! handle, which is a per-serving concern rather than a per-session one. Passing a
//! registry through both call paths would thread a `Mutex<HashMap>` through the
//! `App` seam. A module-level registry keeps the session list owned by exactly the
//! code that adds and removes sessions.
//!
//! It is a single server per process, so "global" here means "one server's
//! sessions", not a cross-server table: [`reset`] empties it at start-up.
//!
//! # Frames, not payloads
//!
//! The queue carries **complete frames** — the `u32` big-endian length prefix
//! followed by the TOML body — not bare payloads. That is load-bearing: a client
//! reads with `ipc::read_server_msg`, which insists on the prefix, so a queue of
//! raw payloads would desynchronise every client's stream on the first broadcast
//! (the client would read the first four bytes of `revision = …` as its length).
//! Encoding here, once per snapshot, also means one serialisation serves every
//! client.
//!
//! # Why a bounded queue per client
//!
//! A client that stops reading must not be allowed to stall the server, and a
//! snapshot is a full restatement of the state, so a backed-up client only needs
//! the *newest* one. Each session therefore gets a small queue and a send that
//! never blocks; when it is full the session is marked lagging and dropped by its
//! own thread. That converts "one wedged client freezes everything" into "one
//! wedged client is disconnected", which is the only acceptable failure here.
//!
//! Belongs here: the session registry and one publish/send.
//! Does not belong: building the snapshot ([`super::state`]), or the handshake
//! ([`super::session`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Mutex, MutexGuard};

use crate::ipc::Snapshot;

/// How many undelivered snapshots one client may have queued.
///
/// Two: the one being written and one replacement. More would only queue stale
/// states that the newer one supersedes.
pub const CLIENT_QUEUE: usize = 2;

/// One connected client, as the broadcaster sees it.
pub(crate) struct Outbox {
    /// One encoded frame (length prefix + body), waiting to be written.
    /// `try_send` never blocks.
    pub tx: SyncSender<Vec<u8>>,
    /// Set when the client could not keep up, so its thread tears it down.
    pub lagging: std::sync::Arc<AtomicBool>,
}

/// Every connected client, keyed by an id that is never reused.
static SESSIONS: Mutex<Option<HashMap<u64, Outbox>>> = Mutex::new(None);

/// The next session id. Monotonic so a stale registry entry can never be matched
/// to a new client after a removal and re-add.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Lock the registry, recovering from poisoning (see `state`'s note on why).
fn registry() -> MutexGuard<'static, Option<HashMap<u64, Outbox>>> {
    match SESSIONS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Empty the registry. Called once at server start-up so a previous run's state in
/// the same process (tests) cannot leak into this one.
pub fn reset() {
    let mut guard = registry();
    *guard = Some(HashMap::new());
}

/// Register a client and return the id its thread uses to unregister later.
pub(crate) fn register(tx: SyncSender<Vec<u8>>, lagging: std::sync::Arc<AtomicBool>) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut guard = registry();
    guard.get_or_insert_with(HashMap::new).insert(id, Outbox { tx, lagging });
    id
}

/// Remove a client.
pub(crate) fn unregister(id: u64) {
    let mut guard = registry();
    if let Some(map) = guard.as_mut() {
        map.remove(&id);
    }
}

/// How many clients are registered.
pub fn session_count() -> usize {
    registry().as_ref().map(|m| m.len()).unwrap_or(0)
}

/// Encode one snapshot as a complete frame.
///
/// Wrapped in [`ServerMsg::Snapshot`] rather than encoded bare: the client decodes
/// every frame as a `ServerMsg`, and a bare snapshot has no `t` tag, so a client
/// would reject it as `missing field t` and lose every update. The message envelope
/// is part of the wire contract, not a wrapper this layer may skip.
///
/// `None` when it cannot be encoded, which is a bug in the snapshot rather than in
/// a client: the caller logs it and drops the frame, because keeping the server
/// alive matters more than one lost update (the next change restates everything).
fn encode(snapshot: &Snapshot) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let msg = crate::ipc::ServerMsg::Snapshot(snapshot.clone());
    match crate::ipc::write_message(&mut buf, &msg) {
        Ok(()) => Some(buf),
        Err(e) => {
            crate::logging::error(
                "server.snapshot_encode_failed",
                &[("revision", &snapshot.revision), ("msg", &e.to_string())],
            );
            None
        }
    }
}

/// Send one snapshot to every registered client.
pub fn publish(snapshot: &Snapshot) {
    let Some(frame) = encode(snapshot) else {
        return;
    };
    let mut guard = registry();
    let Some(map) = guard.as_mut() else {
        return;
    };
    for outbox in map.values() {
        // `try_send` on a full queue is the "slow consumer" signal: mark it so its
        // own thread stops, rather than blocking the server or silently losing
        // every future state change for that client.
        match outbox.tx.try_send(frame.clone()) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                outbox.lagging.store(true, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                // Its thread is gone; the registry entry is cleaned up by
                // `unregister` on that thread's way out.
            }
        }
    }
}

/// Send one already-encoded frame to a single client.
///
/// Unused by the session loop today — a reply is written directly to the socket,
/// because it must be ordered against the frames queued for that client and going
/// through the queue would put the answer behind a stale snapshot. Kept because it
/// is the correct primitive for any future per-client push and because the
/// starvation policy belongs with the queue it applies to.
#[allow(dead_code)]
pub(crate) fn send_to(id: u64, frame: Vec<u8>) -> bool {
    let guard = registry();
    let Some(map) = guard.as_ref() else {
        return false;
    };
    let Some(outbox) = map.get(&id) else {
        return false;
    };
    match outbox.tx.try_send(frame) {
        Ok(()) => true,
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            outbox.lagging.store(true, Ordering::Relaxed);
            false
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{ServerMsg, read_server_msg};

    /// Serialise these tests: the registry is process-global, so two tests running
    /// concurrently would see each other's clients (and `reset` would erase them
    /// mid-test). The lock is the honest fix — it matches production, where there
    /// is exactly one server per process.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Hold the serialisation lock for one test. Poisoning is irrelevant here: each
    /// test starts by emptying the registry anyway.
    fn exclusive() -> MutexGuard<'static, ()> {
        match TEST_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn view(id: u64) -> crate::model::TunnelView {
        let cfg = crate::model::TunnelConfig {
            id,
            name: format!("t{id}"),
            kind: crate::model::ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "127.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        };
        crate::model::TunnelView::new(cfg)
    }

    #[test]
    fn a_published_snapshot_is_readable_as_one_frame() {
        // The regression this pins: the queue used to carry a bare TOML payload, so
        // a client read `revi` (from `revision = …`) as a 1.9 GB frame length and
        // lost every update after its Welcome.
        let _guard = exclusive();
        reset();
        let (tx, rx) = std::sync::mpsc::sync_channel(CLIENT_QUEUE);
        let lagging = std::sync::Arc::new(AtomicBool::new(false));
        let id = register(tx, lagging);
        let snapshot = Snapshot::from_views(7, 0, 1, false, &[view(1)]);
        publish(&snapshot);

        let frame = rx.try_recv().expect("a client must receive the broadcast");
        let mut cursor = frame.as_slice();
        let decoded = read_server_msg(&mut cursor).expect("the frame must decode");
        match decoded {
            Some(ServerMsg::Snapshot(got)) => {
                assert_eq!(got.revision, 7);
                assert_eq!(got.tunnels.len(), 1);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
        unregister(id);
    }

    #[test]
    fn two_clients_both_receive_the_same_frame() {
        let _guard = exclusive();
        reset();
        let (tx_a, rx_a) = std::sync::mpsc::sync_channel(CLIENT_QUEUE);
        let (tx_b, rx_b) = std::sync::mpsc::sync_channel(CLIENT_QUEUE);
        let flag = || std::sync::Arc::new(AtomicBool::new(false));
        let a = register(tx_a, flag());
        let b = register(tx_b, flag());
        assert_eq!(session_count(), 2);

        publish(&Snapshot::from_views(3, 0, 2, false, &[view(1), view(2)]));
        let fa = rx_a.try_recv().expect("client A");
        let fb = rx_b.try_recv().expect("client B");
        assert_eq!(fa, fb, "one snapshot, one encoding, identical bytes");
        unregister(a);
        unregister(b);
    }

    #[test]
    fn a_full_queue_marks_the_client_lagging_instead_of_blocking() {
        let _guard = exclusive();
        reset();
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let lagging = std::sync::Arc::new(AtomicBool::new(false));
        let id = register(tx, lagging.clone());
        let snapshot = Snapshot::from_views(1, 0, 1, false, &[view(1)]);
        // The first fills the single slot; the second must not block.
        publish(&snapshot);
        publish(&snapshot);
        assert!(lagging.load(Ordering::Relaxed), "a slow client must be flagged, not block the server");
        unregister(id);
    }

    #[test]
    fn a_publish_with_no_clients_is_harmless() {
        let _guard = exclusive();
        reset();
        publish(&Snapshot::from_views(1, 0, 0, false, &[]));
        assert_eq!(session_count(), 0);
    }

    #[test]
    fn an_unregistered_client_stops_receiving() {
        let _guard = exclusive();
        reset();
        let (tx, rx) = std::sync::mpsc::sync_channel(CLIENT_QUEUE);
        let id = register(tx, std::sync::Arc::new(AtomicBool::new(false)));
        unregister(id);
        publish(&Snapshot::from_views(1, 0, 1, false, &[]));
        assert!(rx.try_recv().is_err(), "a removed session must not be written to");
    }
}
