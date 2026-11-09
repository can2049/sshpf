//! Turning "the state changed" into frames for every attached client.
//!
//! # Full snapshots, not deltas
//!
//! A snapshot restates everything, so a lost, duplicated or reordered frame is
//! harmless: the next one repairs it. That property is what lets this file stay
//! simple — there is no acknowledgement, no per-client sequence bookkeeping, and
//! no way for two clients to end up with different pictures because one missed an
//! increment.
//!
//! # Revision discipline
//!
//! `revision` strictly increases and a client ignores anything not newer than what
//! it already applied. It is bumped only when something a client could *see*
//! changed (`App::state_signature`) or when the client count did — otherwise every
//! 50 ms tick would send a frame whose only difference was the clock.
//!
//! # Nothing here may block
//!
//! Every send is a `try_send` into a bounded queue. A client that stopped reading
//! is reported back as "drop this id", never waited on: one wedged window must not
//! freeze every other window's view.
//!
//! Belongs here: revision bookkeeping and fan-out.
//! Does not belong: who the clients are ([`super::session`]), what triggered the
//! change ([`super::commands`]), or when the server stops ([`super::lifecycle`]).

use crate::app::App;
use crate::ipc::{ServerMsg, Snapshot};

use super::session::{Session, now_ms};

/// Assign the next revision, build the snapshot, and queue it everywhere.
///
/// Returns the ids that must be dropped. They are not removed here: the caller
/// owns the session list, and mutating it while iterating is how a session ends up
/// half-closed.
pub fn publish(
    app: &App,
    sessions: &mut [Session],
    revision: &mut u64,
    client_count: u32,
    keep_server: bool,
) -> Vec<u64> {
    *revision = revision.saturating_add(1);
    let snapshot = Snapshot::from_views(*revision, now_ms(), client_count, keep_server, &app.tunnels);
    fan_out(sessions, &ServerMsg::Snapshot(snapshot))
}

/// Queue one message to every session, returning the ids that could not take it.
pub fn fan_out(sessions: &mut [Session], msg: &ServerMsg) -> Vec<u64> {
    let mut doomed = Vec::new();
    for session in sessions.iter_mut() {
        // `ServerMsg` is `Clone`, and a snapshot is a few hundred bytes, so
        // cloning per client is cheap at this rate (a few frames per second).
        if !session.try_send(msg.clone()) {
            doomed.push(session.id);
        }
    }
    doomed
}

/// Queue one message to exactly one client, by id.
///
/// A message for a session that already went away is not an error: the command
/// arrived, the reply was built, and the requester left meanwhile.
pub fn send_to(sessions: &mut [Session], id: u64, msg: ServerMsg) -> Vec<u64> {
    match sessions.iter_mut().find(|s| s.id == id) {
        Some(session) => {
            if session.try_send(msg) {
                Vec::new()
            } else {
                vec![id]
            }
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, sync_channel};

    fn session_with_queue(id: u64, cap: usize) -> (Session, Receiver<ServerMsg>) {
        let (tx, rx) = sync_channel(cap);
        (Session::with_queue(id, tx), rx)
    }

    fn snapshot(rev: u64) -> ServerMsg {
        ServerMsg::Snapshot(Snapshot::from_views(rev, 0, 1, false, &[]))
    }

    fn an_app() -> App {
        let dir = tempfile::tempdir().unwrap();
        let paths =
            crate::config::Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        App::new(paths)
    }

    #[test]
    fn a_publish_bumps_the_revision_once_for_everyone() {
        // Keep the receivers alive: dropping one disconnects the channel, which
        // would make every send fail for a reason unrelated to the revision.
        let (a, _a_rx) = session_with_queue(1, 2);
        let (b, _b_rx) = session_with_queue(2, 2);
        let mut sessions = vec![a, b];
        let mut rev = 7;
        let doomed = publish(&an_app(), &mut sessions, &mut rev, 2, false);
        assert!(doomed.is_empty(), "both clients had room");
        assert_eq!(rev, 8, "one publish is one revision, shared by every client");
    }

    #[test]
    fn a_full_queue_marks_that_client_for_removal_and_no_other() {
        let (slow, _slow_rx) = session_with_queue(1, 1);
        let (fast, fast_rx) = session_with_queue(2, 4);
        let mut sessions = vec![slow, fast];
        // The slow client's single slot is taken by this first frame…
        assert!(fan_out(&mut sessions, &snapshot(1)).is_empty());
        // …so the next one finds it full, and only it is dropped.
        let doomed = fan_out(&mut sessions, &snapshot(2));
        assert_eq!(doomed, vec![1], "only the client that cannot keep up is dropped");
        assert!(fast_rx.try_recv().is_ok(), "the healthy client still received a frame");
    }

    #[test]
    fn a_closed_writer_also_marks_the_client_for_removal() {
        let (session, rx) = session_with_queue(9, 2);
        drop(rx);
        let mut sessions = vec![session];
        assert_eq!(fan_out(&mut sessions, &snapshot(1)), vec![9], "a client whose writer is gone must be removed");
    }

    #[test]
    fn a_reply_goes_only_to_its_own_client() {
        let (a, a_rx) = session_with_queue(1, 2);
        let (b, b_rx) = session_with_queue(2, 2);
        let mut sessions = vec![a, b];
        let reply = ServerMsg::Reply { req_id: 1, ok: true, rev: 3, error: None, remaining: None, ports: vec![] };
        assert!(send_to(&mut sessions, 1, reply).is_empty());
        assert!(a_rx.try_recv().is_ok(), "the requester must get its reply");
        assert!(b_rx.try_recv().is_err(), "another client must not see it");
    }

    #[test]
    fn a_message_for_a_vanished_session_is_not_an_error() {
        let mut sessions: Vec<Session> = Vec::new();
        let reply = ServerMsg::Reply { req_id: 1, ok: true, rev: 1, error: None, remaining: None, ports: vec![] };
        assert!(send_to(&mut sessions, 42, reply).is_empty());
    }

    #[test]
    fn a_published_snapshot_carries_the_live_table() {
        let mut app = an_app();
        let paths = app.paths.clone();
        let mut cfg = crate::config::ConfigFile::default();
        cfg.tunnels.push(crate::model::TunnelConfig {
            id: 1,
            name: "web".into(),
            kind: crate::model::ForwardKind::Local,
            ssh_host: "host".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "127.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        });
        crate::config::save(&paths.config, &cfg).unwrap();
        app = App::new(paths);

        let (session, rx) = session_with_queue(1, 2);
        let mut sessions = vec![session];
        let mut rev = 0;
        let _ = publish(&app, &mut sessions, &mut rev, 1, false);
        match rx.try_recv() {
            Ok(ServerMsg::Snapshot(s)) => {
                assert_eq!(s.tunnels.len(), 1, "the snapshot must carry the server's table");
                assert_eq!(s.client_count, 1);
                assert_eq!(s.revision, 1);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }
}
