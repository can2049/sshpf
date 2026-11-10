//! Tunnel supervision: one background thread per tunnel, each driving a
//! `current_thread` tokio runtime that runs the russh session.
//!
//! # Why this shape
//!
//! The UI thread must stay synchronous — `app.rs` polls `crossterm` events with a
//! 200 ms timeout and must never block. `russh` is async, so each tunnel gets its
//! own thread + runtime and reports back over a `std::sync::mpsc` channel. The
//! event vocabulary is unchanged from the subprocess era, so the app's event
//! pump logic is untouched.
//!
//! # What replaced what
//!
//! | subprocess era | now |
//! |---|---|
//! | `Child` + process group | `JoinHandle` of a runtime thread |
//! | `kill(-pgid, SIGTERM→SIGKILL)` | `task::abort()` + `disconnect()` |
//! | `PR_SET_PDEATHSIG` orphan guard | unnecessary — no child process exists |
//! | Windows Job Object | unnecessary — ditto |
//! | stderr reader thread → `Line` | the driver sends `Line` for milestones |
//! | `try_wait()` polling | thread completion is reported once |
//!
//! A hard `SIGKILL` of sshpf still leaves nothing behind: the OS closes the
//! sockets, which closes the SSH sessions.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::model::TunnelId;

/// A tunnel that has been started and not yet observed to finish.
struct Running {
    /// Requested shutdown. The thread checks this between driver steps.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: JoinHandle<()>,
}

/// Events produced by supervised tunnels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelEvent {
    /// The forward is genuinely established (the local `bind` succeeded, or the
    /// server accepted `tcpip-forward`). This replaces every stderr heuristic.
    Ready { id: TunnelId },
    /// One progress/diagnostic line for the tunnel log.
    Line { id: TunnelId, line: String },
    /// The session ended.
    ///
    /// `reason` carries the failure the driver already classified — unlike the
    /// subprocess era, there is nothing left to infer from a log tail, so the
    /// precise cause travels with the event. `None` means a clean shutdown.
    Exited { id: TunnelId, code: Option<i32>, reason: Option<crate::model::FailureInfo> },
    /// Every line for this run has been delivered, so the consumer may now
    /// classify the failure using the complete log.
    StderrClosed { id: TunnelId },
    /// The tunnel could not even be started.
    SpawnFailed { id: TunnelId, error: String },
}

/// Owns every live tunnel thread. Dropping it stops them all.
#[derive(Default)]
pub struct Supervisor {
    running: HashMap<TunnelId, Running>,
}

impl Supervisor {
    pub fn new() -> Self {
        Supervisor { running: HashMap::new() }
    }

    /// Start one tunnel on its own thread + runtime.
    ///
    /// `ready` fires when the forward is genuinely established; the app maps
    /// that to `TunnelStatus::Up`.
    pub fn spawn(
        &mut self,
        cfg: crate::model::TunnelConfig,
        host_key_policy: crate::ssh::known_hosts::HostKeyPolicy,
        health_check: Option<Duration>,
        tx: Sender<TunnelEvent>,
    ) -> std::io::Result<()> {
        let id = cfg.id;
        self.stop(id);

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = stop.clone();

        let handle = std::thread::Builder::new().name(format!("sshpf-tunnel-{id}")).spawn(move || {
            contain(id, &tx, || drive(id, cfg, host_key_policy, health_check, &tx, stop_thread));
        })?;

        self.running.insert(id, Running { stop, handle });
        Ok(())
    }

    /// True when the tunnel thread is still supervised.
    pub fn is_running(&self, id: TunnelId) -> bool {
        self.running.contains_key(&id)
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// Non-blocking reap. Returns `Some(exit_code)` when the tunnel has just
    /// finished (and forgets it); `None` while it is still alive.
    pub fn poll(&mut self, id: TunnelId) -> Option<Option<i32>> {
        let running = self.running.get(&id)?;
        if !running.handle.is_finished() {
            return None;
        }
        self.running.remove(&id);
        Some(Some(0))
    }

    /// Ask one tunnel to stop and wait for its thread to finish.
    pub fn stop(&mut self, id: TunnelId) {
        self.stop_with_grace(id, Duration::from_secs(2));
    }

    fn stop_with_grace(&mut self, id: TunnelId, grace: Duration) {
        let Some(running) = self.running.remove(&id) else {
            return;
        };
        running.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // The driver notices the flag within one select tick; join with a bound
        // so a stuck driver cannot freeze the UI.
        let deadline = std::time::Instant::now() + grace;
        while !running.handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // A thread that outlived the grace period is a wedged driver, not a slow
        // one: say so once instead of silently detaching it (which is also what
        // makes "the UI says stopped but the port is still bound" a diagnosable
        // report instead of a mystery).
        if !running.handle.is_finished() {
            crate::logging::warn(
                "tunnel.stop_timeout",
                &[("tunnel", &id), ("grace_ms", &grace.as_millis()), ("msg", &"隧道线程未在宽限期内退出")],
            );
        }
        // The thread owns its runtime; dropping the `JoinHandle` detaches it, and
        // dropping the runtime (inside the thread) closes the sockets. We
        // deliberately never `join()`: joining here would reintroduce the freeze
        // this bound exists to prevent.
    }

    /// Stop everything (used on quit and in `Drop`).
    pub fn stop_all(&mut self) {
        let ids: Vec<TunnelId> = self.running.keys().copied().collect();
        for id in ids {
            self.stop(id);
        }
    }

    /// Ids currently supervised.
    pub fn ids(&self) -> Vec<TunnelId> {
        self.running.keys().copied().collect()
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Run one tunnel thread's body and turn its outcome into the event sequence.
///
/// A panic on a tunnel thread must not take the process (or the UI) down:
/// `std::thread` unwinds only the panicking thread and the UI thread keeps
/// running. What it *must* do is leave the app with a truthful verdict instead of
/// a tunnel that silently never reports again — hence the `catch_unwind` here.
///
/// This is **not** `catch_unwind` used to hide a bug: every reachable panic in
/// this crate is fixed at its root (see the rest of this change). This is
/// last-ditch containment for what we cannot reach — third-party code inside
/// `russh`/tokio, and allocation failure — and its whole job is to keep the
/// process alive and record the failure.
fn contain(id: TunnelId, tx: &Sender<TunnelEvent>, body: impl FnOnce() -> anyhow::Result<()>) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    match outcome {
        Ok(Ok(())) => {
            let _ = tx.send(TunnelEvent::Exited { id, code: Some(0), reason: None });
        }
        Ok(Err(e)) => {
            let info = crate::ssh::client::classify(&e);
            // Keep the detail in the log *and* on the event: the event is what
            // selects the status message, the log is what the user reads in the
            // detail panel.
            let _ = tx.send(TunnelEvent::Line { id, line: info.detail.clone() });
            let _ = tx.send(TunnelEvent::Exited { id, code: Some(1), reason: Some(info) });
        }
        Err(payload) => {
            // The panic hook already wrote `crash.panic`. Tell the state machine
            // too, so the row goes to `Failed` instead of staying `Starting`
            // forever.
            let message = panic_message(&payload);
            crate::logging::error("tunnel.thread_panic", &[("tunnel", &id), ("msg", &message)]);
            let info =
                crate::model::FailureInfo::with_short(crate::model::FailureKind::Unknown, "隧道线程崩溃", message);
            let _ = tx.send(TunnelEvent::Line { id, line: info.detail.clone() });
            let _ = tx.send(TunnelEvent::Exited { id, code: Some(101), reason: Some(info) });
        }
    }
    // Always last: lets the app classify with the complete log.
    let _ = tx.send(TunnelEvent::StderrClosed { id });
}

/// Run one tunnel thread's body: build the runtime and race the driver against
/// the shutdown flag.
///
/// Split out of [`Supervisor::spawn`] so the panic containment around it stays
/// readable, and so a runtime-build failure is a plain `Err` the caller reports
/// rather than an early `return` that skips the event sequence.
fn drive(
    id: TunnelId,
    cfg: crate::model::TunnelConfig,
    host_key_policy: crate::ssh::known_hosts::HostKeyPolicy,
    health_check: Option<Duration>,
    tx: &Sender<TunnelEvent>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let message = format!("创建 runtime 失败: {e}");
            let _ = tx.send(TunnelEvent::SpawnFailed { id, error: message.clone() });
            // Reported as an error return too, so the app's normal exit
            // classification runs and the row does not stay `Starting`.
            anyhow::bail!(message);
        }
    };

    let tx_ready = tx.clone();
    let ready = move || {
        let _ = tx_ready.send(TunnelEvent::Ready { id });
    };

    rt.block_on(async {
        let opts = crate::ssh::client::Options { host_key_policy, health_check };
        let run = crate::ssh::client::run(&cfg, &opts, id, tx.clone(), ready);
        tokio::pin!(run);
        // Race the driver against the shutdown flag. `select!` already loops
        // internally until one branch completes, so both branches are terminal and
        // no outer loop is needed.
        tokio::select! {
            biased;
            _ = wait_for_stop(&stop) => Ok::<(), anyhow::Error>(()),
            res = &mut run => res,
        }
    })
}

/// Best-effort text of a caught panic payload, for the log and the status table.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<非字符串 panic>".to_string()
    }
}

/// Resolve as soon as the stop flag is set (polled cheaply).
async fn wait_for_stop(stop: &std::sync::atomic::AtomicBool) {
    loop {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod tests;
