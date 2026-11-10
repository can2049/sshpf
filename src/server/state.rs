//! The server's shared state: one `App` behind a mutex, plus the bookkeeping the
//! sessions and the pump both need.
//!
//! # Why a mutex around the whole `App`
//!
//! The tunnels only change in two places: a command from a client, and the event
//! pump. Both are short, non-blocking operations (spawn a thread, compare a
//! revision, extend a log ring), so the lock is never held across I/O. That is
//! what makes one coarse lock the honest choice: splitting it would let a reader
//! observe a half-applied command, and the whole design rests on a client never
//! seeing an invented state.
//!
//! The one rule this module exists to keep: **every mutation goes through
//! [`Shared::with_app`] and is followed by [`Shared::bump`]** (or the pump's
//! `publish_if_changed`, which does it). A mutation the clients are not told
//! about is a bug that looks like "the other window is stuck".
//!
//! # Why the lock is recovered rather than propagated
//!
//! Both `Mutex`es here guard a handful of field writes with no I/O between them,
//! so a panic while held cannot leave a half-written structure that another
//! reader could not tolerate. Poisoning would instead take the server down for
//! every remaining client because one thread tripped — the larger harm. Recovery
//! is a decision, not an oversight.
//!
//! Belongs here: the shared handle, revision tracking, the client registry, and
//! the flags that end the accept loop.
//! Does not belong: what a command means ([`super::commands`]) or how a frame is
//! written ([`super::session`]).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::app::App;
use crate::ipc::Snapshot;

/// How often the headless event pump drains tunnel events.
///
/// Matches the TUI's 200 ms frame: the same responsiveness, without a terminal to
/// redraw. It is not faster because `drain_events` costs a syscall per empty poll.
pub const PUMP_INTERVAL: Duration = Duration::from_millis(200);

/// How many clients may be attached at once.
///
/// A bound, not a target: each client costs a thread and a snapshot-sized buffer,
/// and an unbounded registry is one client-side loop away from exhausting memory.
/// Ten is well past the "several terminals" case this exists for.
pub const MAX_CLIENTS: u32 = 10;

/// The authoritative state plus everything a session needs to reach it.
pub struct ServerState {
    /// The owner `App`. Its `Supervisor` holds the tunnels and its `cfg` is the
    /// file on disk.
    pub app: App,
    /// Whether the server outlives its clients (`--keep-server`).
    pub keep_server: bool,
    /// Bumped on every applied change; a snapshot carries it.
    pub revision: u64,
}

/// One lock read of the state, in both the shapes the publisher needs.
///
/// A struct rather than a tuple so the two fields cannot be swapped at a call site:
/// they are both "the state" and a mistake between them would publish a snapshot
/// under the wrong signature.
struct Captured {
    /// The revision this reading is at.
    revision: u64,
    /// Everything a client would see, timestamps excluded.
    signature: super::signature::StateSignature,
    /// The wire form, timestamped from one clock reading.
    snapshot: Snapshot,
}

/// A handle to the server, cheap to clone and safe to share with every session.
#[derive(Clone)]
pub struct Shared {
    /// The state and the lock that guards it, as one value: it is impossible to
    /// reach the `App` without holding the guard.
    state: Arc<Mutex<ServerState>>,
    /// How many clients are connected **right now**. Authoritative: this, not a
    /// client's claim, decides whether a `Bye` ends the server. Kept outside the
    /// mutex so the accept path never contends with a slow command.
    client_count: Arc<AtomicU32>,
    /// Set once the server has begun shutting down, so every loop can stop.
    stopping: Arc<AtomicBool>,
    /// Set once a shutdown has begun, so the grace window is armed exactly once.
    pub(crate) grace_armed: Arc<AtomicBool>,
    /// The revision the clients last received.
    ///
    /// Kept outside the state mutex so the pump's very frequent poll
    /// (`publish_if_changed`) does not contend with a command being applied.
    published: Arc<AtomicU64>,
    /// What the clients last received, other than the revision.
    ///
    /// The second trigger for a publish, and the one that makes a state change the
    /// pump caused visible at all: `revision` only moves when a *command* is
    /// applied, so without this a tunnel that reached `Up` on its own would be
    /// published never — the exact bug this field exists to prevent. See
    /// [`super::signature`].
    last_sent: Arc<Mutex<Option<super::signature::StateSignature>>>,
    /// The shared secret published for this run; the handshake compares against it.
    token: String,
}

impl Shared {
    pub fn new(app: App, keep_server: bool, token: String) -> Shared {
        Shared {
            state: Arc::new(Mutex::new(ServerState { app, keep_server, revision: 1 })),
            client_count: Arc::new(AtomicU32::new(0)),
            stopping: Arc::new(AtomicBool::new(false)),
            grace_armed: Arc::new(AtomicBool::new(false)),
            published: Arc::new(AtomicU64::new(0)),
            last_sent: Arc::new(Mutex::new(None)),
            token,
        }
    }

    /// Run `f` with exclusive access to the owner `App`.
    pub fn with_app<R>(&self, f: impl FnOnce(&mut App) -> R) -> R {
        let mut state = self.lock_state();
        f(&mut state.app)
    }

    /// Record that the state changed, and return the new revision.
    ///
    /// Takes the lock, so the revision and the change it describes are updated
    /// atomically with respect to any reader that takes a snapshot.
    pub fn bump(&self) -> u64 {
        let mut state = self.lock_state();
        state.revision = state.revision.saturating_add(1);
        state.revision
    }

    /// Take a copy of the current state as a wire snapshot.
    pub fn snapshot(&self) -> Snapshot {
        self.capture().snapshot
    }

    /// Read the state once, as both the wire snapshot and the comparable signature.
    ///
    /// One lock for both on purpose: a signature computed from state A paired with a
    /// snapshot of state B would either publish a change twice or — worse — record
    /// "already sent" for a state no client ever received. Cheap enough to be the
    /// shared path because the clone is of the same per-tunnel views the snapshot
    /// needs anyway.
    fn capture(&self) -> Captured {
        let state = self.lock_state();
        let keep = state.keep_server;
        let revision = state.revision;
        let views = state.app.tunnels.clone();
        drop(state);
        let count = self.client_count.load(Ordering::Relaxed);
        let signature = super::signature::StateSignature::of(count, keep, &views);
        Captured { revision, signature, snapshot: Snapshot::from_views(revision, now_ms(), count, keep, &views) }
    }

    /// Send the current state to every client, if they would see anything new.
    ///
    /// Two independent triggers, because the state changes for two unrelated
    /// reasons:
    ///
    /// * **the revision moved** — a command was applied. Even when it changes
    ///   nothing a client renders, the client needs the new revision to stamp its
    ///   next `expect_rev` with, so this publishes by itself.
    /// * **the signature moved** — the event pump did something on its own. This is
    ///   the trigger that was missing: `Ready`, a retry being scheduled, a progress
    ///   line, a closed session. None of them applies a command, so the revision
    ///   never moved and no window ever learned that its forward had come up.
    ///
    /// Safe to call on every pump tick: with nothing new it costs one lock, one
    /// clone of the views, and a comparison.
    pub fn publish_if_changed(&self) -> bool {
        let captured = self.capture();
        let revision_sent = self.published.load(Ordering::Relaxed) == captured.revision;
        let signature_sent = self.last_sent_is(&captured.signature);
        if revision_sent && signature_sent {
            return false;
        }
        self.published.store(captured.revision, Ordering::Relaxed);
        self.remember_signature(captured.signature);
        super::broadcast::publish(&captured.snapshot);
        true
    }

    /// Whether `signature` is exactly what the clients were last sent.
    ///
    /// Kept outside the state mutex for the same reason as `published`: the pump
    /// calls this every tick, and it must not contend with a command being applied.
    fn last_sent_is(&self, signature: &super::signature::StateSignature) -> bool {
        match self.last_sent.lock() {
            Ok(guard) => guard.as_ref() == Some(signature),
            Err(poisoned) => poisoned.into_inner().as_ref() == Some(signature),
        }
    }

    /// Record what the clients are about to be sent.
    fn remember_signature(&self, signature: super::signature::StateSignature) {
        match self.last_sent.lock() {
            Ok(mut guard) => *guard = Some(signature),
            Err(poisoned) => *poisoned.into_inner() = Some(signature),
        }
    }

    /// Register a client, or refuse it when the registry is full or the server is
    /// stopping.
    ///
    /// The claim is a compare-and-swap on the count alone, so two simultaneous
    /// connections cannot both pass a "one slot left" check while a command holds
    /// the state lock.
    pub fn admit_client(&self, peer: &SocketAddr) -> bool {
        if self.is_stopping() {
            return false;
        }
        // Claim a slot by swapping in `current + 1`, retrying if another client
        // claimed it first. `None` is how the swap says "already at the limit".
        let mut current = self.client_count.load(Ordering::Acquire);
        loop {
            if current >= MAX_CLIENTS {
                return false;
            }
            match self.client_count.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        let count = self.client_count.load(Ordering::Relaxed);
        crate::eventlog::server_client_connected(count, &peer.to_string());
        // A new client changes `client_count`, which is part of every snapshot: the
        // *other* clients must learn they are no longer as few as they thought, or
        // one of them may warn about being the last when it is not. Publishing here
        // rather than waiting for the pump keeps the count accurate within one
        // round trip instead of up to a pump interval.
        self.bump();
        self.publish_if_changed();
        true
    }

    /// Unregister a client and report the count that remains.
    ///
    /// Saturating on purpose: a `Bye` followed by the EOF of the closed socket is
    /// two reports for one client, and it must not wrap the count to four billion
    /// and keep the server alive forever.
    pub fn release_client(&self, reason: &str) -> u32 {
        let mut current = self.client_count.load(Ordering::Acquire);
        let remaining = loop {
            let next = current.saturating_sub(1);
            match self.client_count.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break next,
                Err(observed) => current = observed,
            }
        };
        crate::eventlog::server_client_disconnected(remaining, reason);
        self.bump();
        self.publish_if_changed();
        remaining
    }

    pub fn client_count(&self) -> u32 {
        self.client_count.load(Ordering::Relaxed)
    }

    pub fn keep_server(&self) -> bool {
        self.lock_state().keep_server
    }

    /// The config path this server serves, as the handshake compares it.
    pub fn config_display(&self) -> String {
        self.with_app(|app| app.paths.config.display().to_string())
    }

    /// The published token, for the handshake.
    ///
    /// Read from memory rather than the file: the file is the *publication*, this
    /// is the source it was published from, and a read of the file could race a
    /// rewrite on the next run.
    pub fn token(&self) -> String {
        self.token.clone()
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    /// Begin stopping. Idempotent, so the accept loop and a lifecycle trigger can
    /// both call it without fighting over who got there first.
    pub fn begin_stopping(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// True when the count reached zero and the server is not staying up anyway.
    pub fn last_client_left(&self) -> bool {
        self.client_count() == 0 && !self.keep_server()
    }

    /// How many tunnels the snapshot would report as running.
    pub fn active_count(&self) -> usize {
        self.with_app(|app| app.tunnels.iter().filter(|t| t.status.is_active()).count())
    }

    /// The current revision, for a reply.
    pub fn revision(&self) -> u64 {
        self.lock_state().revision
    }

    /// Ask the server to stop, recording why.
    ///
    /// Reachable from a signal watcher, which is why it takes no `Self` argument:
    /// the watcher holds the handle registered by [`register_stop_handle`], not a
    /// `Shared`, so a signal-induced stop does not need to clone the whole server
    /// handle into a thread whose only job is to set one flag.
    pub fn request_stop(&self, trigger: &str) {
        crate::eventlog::server_shutdown_begin(self.client_count(), self.active_count(), trigger);
        self.begin_stopping();
    }

    /// Lock the state, recovering from poisoning rather than propagating it.
    pub(crate) fn lock_state(&self) -> MutexGuard<'_, ServerState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Epoch milliseconds, read once per snapshot.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The stop hook a signal watcher can hold, published by [`register_stop_handle`].
///
/// A signal handler may only store to an atomic, so the actual work happens on a
/// watcher thread that calls this. It is a boxed closure rather than a `Shared`
/// because the watcher must not keep the whole server state alive — it only needs
/// "please stop".
static STOP_HOOK: Mutex<Option<Box<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Publish the hook that asks this server to stop.
///
/// Called once by [`super::run`] before the accept loop starts.
pub fn register_stop_hook(hook: Box<dyn Fn() + Send + Sync>) {
    match STOP_HOOK.lock() {
        Ok(mut slot) => *slot = Some(hook),
        Err(poisoned) => *poisoned.into_inner() = Some(hook),
    }
}

/// Ask the running server to stop, if one is registered.
///
/// Returns whether a hook was found. Called from the signal watcher thread; a
/// no-op before `register_stop_hook` runs, which is the normal case for a client.
pub fn request_stop_via_hook() -> bool {
    let guard = match STOP_HOOK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.as_ref() {
        Some(hook) => {
            hook();
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{FailureInfo, FailureKind, ForwardKind, TunnelConfig, TunnelStatus};
    use std::time::Instant;

    /// A server handle over an empty temp config.
    ///
    /// **No client is registered.** Publishing to an empty registry is a documented
    /// no-op, and registering one would mean touching the process-global session
    /// table that [`super::broadcast`]'s own tests serialise; the return value of
    /// `publish_if_changed` is the whole property under test, and it does not depend
    /// on anyone listening.
    fn shared_with_one_tunnel() -> (tempfile::TempDir, Shared) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).expect("saving the fixture config");
        (dir, Shared::new(App::new(paths), false, "test-token".to_string()))
    }

    fn config(id: u64) -> TunnelConfig {
        TunnelConfig {
            id,
            name: format!("t{id}"),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "127.0.0.1".into(),
            remote_port: 80,
            enabled: true,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        }
    }

    /// The reported bug, end to end inside one process: the event pump moves a row
    /// to `Up` without applying any command, so `revision` does not move — and the
    /// old implementation therefore published nothing, leaving every window on
    /// `◐ 连接中` forever while `ssh <host>` worked.
    #[test]
    fn a_status_change_from_the_event_pump_is_published() {
        let (_dir, shared) = shared_with_one_tunnel();
        shared.with_app(|app| {
            app.tunnels.push(crate::model::TunnelView::new(config(1)));
            app.tunnels[0].status = TunnelStatus::Starting { since: Instant::now() };
        });

        // First tick: the state has never been sent, so it must be.
        assert!(shared.publish_if_changed(), "the initial state must reach the clients");

        // Nothing changed: a second tick must stay quiet, or every client would be
        // republished five times a second forever.
        let revision_before = shared.revision();
        assert!(!shared.publish_if_changed(), "an idle tick must not publish");
        assert_eq!(shared.revision(), revision_before, "an idle tick must not invent a revision");

        // `Ready` arrives from the tunnel thread. No command is applied, so the
        // revision is untouched — this is precisely the case the old code missed.
        shared.with_app(|app| app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() });
        assert_eq!(shared.revision(), revision_before, "the pump must not bump the revision");
        assert!(shared.publish_if_changed(), "a row that came up must reach every window");

        // And it settles again once the clients have it.
        assert!(!shared.publish_if_changed(), "a published change must not repeat on every tick");
    }

    /// A log line is a change too: it is what the detail panel renders, and a
    /// reconnect loop that only ever logs would otherwise look frozen.
    #[test]
    fn a_log_line_from_the_event_pump_is_published() {
        let (_dir, shared) = shared_with_one_tunnel();
        shared.with_app(|app| {
            app.tunnels.push(crate::model::TunnelView::new(config(1)));
            app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
        });
        assert!(shared.publish_if_changed());

        shared.with_app(|app| app.on_line(1, "已连接 10.0.0.1".into()));
        assert!(shared.publish_if_changed(), "a progress line must reach the detail panel");
        assert!(!shared.publish_if_changed());
    }

    /// A retry being scheduled is a state change with no command behind it.
    #[test]
    fn a_scheduled_retry_is_published() {
        let (_dir, shared) = shared_with_one_tunnel();
        shared.with_app(|app| {
            app.tunnels.push(crate::model::TunnelView::new(config(1)));
            app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
        });
        assert!(shared.publish_if_changed());

        shared.with_app(|app| {
            app.tunnels[0].status = TunnelStatus::Retrying {
                attempt: 2,
                next_at: Instant::now(),
                reason: FailureInfo::new(FailureKind::Timeout, "read timed out"),
            };
        });
        assert!(shared.publish_if_changed(), "a scheduled retry must be visible to every window");
    }
}
