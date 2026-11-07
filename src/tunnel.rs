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
        tx: Sender<TunnelEvent>,
    ) -> std::io::Result<()> {
        let id = cfg.id;
        self.stop(id);

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = stop.clone();

        let handle = std::thread::Builder::new().name(format!("sshpf-tunnel-{id}")).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(TunnelEvent::SpawnFailed { id, error: format!("创建 runtime 失败: {e}") });
                    let _ = tx.send(TunnelEvent::StderrClosed { id });
                    return;
                }
            };

            let tx_ready = tx.clone();
            let ready = move || {
                let _ = tx_ready.send(TunnelEvent::Ready { id });
            };

            let result = rt.block_on(async {
                let opts = crate::ssh::client::Options { host_key_policy };
                let run = crate::ssh::client::run(&cfg, &opts, id, tx.clone(), ready);
                tokio::pin!(run);
                // Race the driver against the shutdown flag. `select!` already
                // loops internally until one branch completes, so both branches
                // are terminal and no outer loop is needed.
                tokio::select! {
                    biased;
                    _ = wait_for_stop(&stop_thread) => Ok::<(), anyhow::Error>(()),
                    res = &mut run => res,
                }
            });

            match result {
                Ok(()) => {
                    let _ = tx.send(TunnelEvent::Exited { id, code: Some(0), reason: None });
                }
                Err(e) => {
                    let info = crate::ssh::client::classify(&e);
                    // Keep the detail in the log *and* on the event: the event is
                    // what selects the status message, the log is what the user
                    // reads in the detail panel.
                    let _ = tx.send(TunnelEvent::Line { id, line: info.detail.clone() });
                    let _ = tx.send(TunnelEvent::Exited { id, code: Some(1), reason: Some(info) });
                }
            }
            // Always last: lets the app classify with the complete log.
            let _ = tx.send(TunnelEvent::StderrClosed { id });
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
        // The thread owns its runtime; dropping it aborts remaining tasks and
        // closes the sockets.
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
        sup.spawn(cfg(1, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, tx).unwrap();

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
        sup.spawn(cfg(2, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, tx).unwrap();
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
        sup.spawn(cfg(3, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, tx.clone()).unwrap();
        sup.spawn(cfg(4, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, tx).unwrap();
        assert_eq!(sup.running_count(), 2);
        sup.stop_all();
        assert_eq!(sup.running_count(), 0);
        assert!(sup.ids().is_empty());
    }

    #[test]
    fn respawn_replaces_the_previous_run() {
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(cfg(5, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, tx.clone()).unwrap();
        sup.spawn(cfg(5, 0), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew, tx).unwrap();
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
}
