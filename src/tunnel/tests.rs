//! Unit tests for `tunnel.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

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
