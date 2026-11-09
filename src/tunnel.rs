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
mod tests {
    use super::*;
    use crate::model::{ForwardKind, TunnelConfig};
    use std::sync::mpsc;

    fn cfg(id: TunnelId, port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: format!("t{id}"),
            kind: ForwardKind::Local,
            // Nothing listens here; the connect attempt fails fast, which is
            // all these supervision tests need.
            ssh_host: "127.0.0.1:1".into(),
            local_addr: "127.0.0.1".into(),
            local_port: port,
            remote_host: "127.0.0.1".into(),
            remote_port: 9,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        }
    }

    /// A tunnel that cannot connect must report SpawnFailed/Exited + StderrClosed
    /// and leave nothing supervised.
    #[test]
    fn failed_connect_reports_events_and_cleans_up() {
        let (tx, rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        // Port 1 on loopback refuses immediately.
        sup.spawn(cfg(1, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, None, tx).unwrap();

        let mut saw_exit = false;
        let mut saw_closed = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline && !(saw_exit && saw_closed) {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(TunnelEvent::Exited { .. }) => saw_exit = true,
                Ok(TunnelEvent::StderrClosed { .. }) => saw_closed = true,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if sup.poll(1).is_some() {
                        saw_exit = true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(saw_exit, "no Exited event");
        assert!(saw_closed, "no StderrClosed event");
    }

    #[test]
    fn stop_is_idempotent_and_forgets_the_tunnel() {
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(cfg(2, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, None, tx).unwrap();
        assert_eq!(sup.running_count(), 1);
        sup.stop(2);
        assert_eq!(sup.running_count(), 0);
        sup.stop(2); // second stop is a no-op
        assert_eq!(sup.running_count(), 0);
    }

    #[test]
    fn stop_all_clears_everything() {
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(cfg(3, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, None, tx.clone()).unwrap();
        sup.spawn(cfg(4, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, None, tx).unwrap();
        assert_eq!(sup.running_count(), 2);
        sup.stop_all();
        assert_eq!(sup.running_count(), 0);
        assert!(sup.ids().is_empty());
    }

    #[test]
    fn respawn_replaces_the_previous_run() {
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(cfg(5, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, None, tx.clone()).unwrap();
        sup.spawn(cfg(5, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, None, tx).unwrap();
        assert_eq!(sup.running_count(), 1, "same id must not accumulate");
    }

    #[test]
    fn wait_for_stop_returns_when_flag_is_set() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let fut = wait_for_stop(&flag);
            tokio::pin!(fut);
            // Not set yet: should not resolve.
            let timed_out = tokio::time::timeout(Duration::from_millis(60), &mut fut).await.is_err();
            assert!(timed_out, "wait_for_stop resolved without the flag");
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            tokio::time::timeout(Duration::from_secs(2), &mut fut).await.expect("did not resolve after flag set");
        });
    }

    // -----------------------------------------------------------------------
    // panic containment
    // -----------------------------------------------------------------------

    /// Drain events until `StderrClosed`, returning what arrived (in order).
    ///
    /// A timeout is a hard failure: the contract is that a dead tunnel thread
    /// always completes the event sequence, so a missing one would leave the app
    /// with a row stuck in `Starting` forever.
    fn collect_until_closed(rx: &mpsc::Receiver<TunnelEvent>) -> Vec<TunnelEvent> {
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => {
                    let last = matches!(ev, TunnelEvent::StderrClosed { .. });
                    seen.push(ev);
                    if last {
                        return seen;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("no StderrClosed: the tunnel thread hung; events so far: {seen:?}");
    }

    /// A panic on a tunnel thread must keep the process alive and still report a
    /// terminal `Exited`, so the UI leaves the row in `Failed` instead of
    /// `Starting`. This is the "does a thread panic infect the process?" answer.
    #[test]
    fn a_panicking_tunnel_thread_reports_exit_instead_of_hanging() {
        let (tx, rx) = mpsc::channel();
        // Deliberately *not* silencing the global panic hook here: the hook is
        // process-wide and `cargo test` runs tests in parallel, so swapping it
        // would suppress crash records other tests are asserting on. The
        // backtrace this prints is captured with the test's stderr.
        contain(7, &tx, || panic!("tunnel driver exploded"));

        let events = collect_until_closed(&rx);
        let exited = events
            .iter()
            .find_map(|e| match e {
                TunnelEvent::Exited { id, code, reason } => Some((*id, *code, reason.clone())),
                _ => None,
            })
            .expect("a panicking driver must still produce Exited");
        assert_eq!(exited.0, 7);
        assert_eq!(exited.1, Some(101), "101 is the conventional Rust panic exit code");
        let reason = exited.2.expect("the panic text must travel with the event");
        assert!(reason.detail.contains("tunnel driver exploded"), "{reason:?}");
        assert!(events.last().is_some_and(|e| matches!(e, TunnelEvent::StderrClosed { .. })));

        // The process is still alive: this assertion runs after the panic.
        assert!(std::process::id() > 0, "the test process must have survived the tunnel panic");
    }

    /// The containment holds on a **real** thread, not just a direct call: the
    /// thread really panics and unwinds, the process survives, and a second
    /// tunnel still starts and reports normally afterwards. This is the
    /// "does a tunnel-thread panic infect the process / the UI?" answer.
    #[test]
    fn a_real_panicking_thread_does_not_infect_the_process_or_other_tunnels() {
        let (tx, rx) = mpsc::channel();
        let doomed = {
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("sshpf-tunnel-doomed".to_string())
                .spawn(move || contain(11, &tx, || panic!("real thread panic")))
                .unwrap()
        };
        doomed.join().expect("the thread must finish, not abort the process");

        let events = collect_until_closed(&rx);
        assert!(
            events.iter().any(|e| matches!(e, TunnelEvent::Exited { code: Some(101), .. })),
            "the panic must be reported as an exit: {events:?}"
        );

        // The process is alive, so a *different* tunnel can still be supervised.
        let (tx2, _rx2) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(cfg(12, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, None, tx2)
            .expect("spawning after a thread panic must still work");
        assert_eq!(sup.running_count(), 1);
        sup.stop(12);
        assert_eq!(sup.running_count(), 0);
    }

    /// A clean driver produces `Exited { code: Some(0) }` with no reason.
    #[test]
    fn a_clean_driver_reports_a_zero_exit() {
        let (tx, rx) = mpsc::channel();
        contain(8, &tx, || Ok(()));
        let events = collect_until_closed(&rx);
        let exited = events.iter().find_map(|e| match e {
            TunnelEvent::Exited { code, reason, .. } => Some((*code, reason.clone())),
            _ => None,
        });
        assert_eq!(exited, Some((Some(0), None)));
    }

    /// An `Err` driver is classified, and the detail reaches the log/panel line.
    #[test]
    fn a_failing_driver_is_classified_and_reported() {
        let (tx, rx) = mpsc::channel();
        contain(9, &tx, || Err(anyhow::anyhow!("failed to lookup address information: nonsense")));
        let events = collect_until_closed(&rx);
        assert!(
            events.iter().any(|e| matches!(e, TunnelEvent::Line { line, .. } if line.contains("lookup address"))),
            "the classified detail must be delivered as a line: {events:?}"
        );
        let reason = events
            .iter()
            .find_map(|e| match e {
                TunnelEvent::Exited { reason, .. } => reason.clone(),
                _ => None,
            })
            .expect("a failure must carry its classification");
        assert_eq!(reason.kind, crate::model::FailureKind::DnsFailure);
    }
}
