//! The client's outbox: how an `App` that does **not** own the tunnels forwards
//! user intent to the process that does.
//!
//! # The two roles one `App` plays
//!
//! `App` is the state machine that keys, forms, confirmations and the renderer
//! all talk to. In the client-server architecture that same type is used by both
//! processes:
//!
//! | role | where the truth lives | what a mutation does |
//! |---|---|---|
//! | owner (`--server`) | this process's `Supervisor` | acts immediately, then persists |
//! | mirror (a plain `sshpf`) | the server's snapshot | sends a command, then waits |
//!
//! The renderer and the key handlers are identical in both roles, which is the
//! whole point: making the client a *different* type would have meant a second
//! copy of every interaction.
//!
//! # Why a channel and not a `client::Connection`
//!
//! `app` sits above `tunnel`/`config`; `client` sits beside it. Holding a
//! `client::Connection` here would be a sideways dependency and would drag the
//! socket into every `App` unit test. A `Sender<ClientMsg>` is the smallest
//! possible coupling: `app` can drop a command into a queue and forget about it,
//! and the tests can assert on the queue.
//!
//! Belongs here: the `Mirror` record, the outbox, and the "am I a mirror?"
//! decisions that every mutating `App` method consults.
//! Does not belong: connecting, reconnecting, or reading snapshots
//! ([`crate::client`]), or what a command means ([`crate::ipc::Op`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use crate::ipc::{ClientMsg, Op};

use super::App;

/// The outbound half of a mirror: where commands go, and how they are numbered.
#[derive(Debug)]
pub struct Mirror {
    /// Queue to the connection thread. A closed channel means the connection is
    /// gone; the send is then dropped rather than treated as fatal, because the
    /// user's next action will surface the disconnect through the status line.
    tx: Sender<ClientMsg>,
    /// Request ids, so a reply can be matched to the command that caused it.
    next_req: AtomicU64,
    /// The revision this client last applied. Sent as `expect_rev` so the server
    /// can refuse a command built on a view the user was looking at but the
    /// server has since replaced.
    rev: AtomicU64,
}

impl Mirror {
    pub fn new(tx: Sender<ClientMsg>) -> Mirror {
        Mirror { tx, next_req: AtomicU64::new(1), rev: AtomicU64::new(0) }
    }

    /// Queue one command, tagged with the revision this client has applied.
    ///
    /// `expect_rev` is `None` when no snapshot has arrived yet: refusing a
    /// command because the first snapshot is still in flight would make the
    /// first second after launch feel broken.
    pub fn send(&self, op: Op) -> bool {
        let req_id = self.next_req.fetch_add(1, Ordering::Relaxed);
        let applied = self.rev.load(Ordering::Relaxed);
        let expect_rev = if applied == 0 { None } else { Some(applied) };
        self.tx.send(ClientMsg::Command { req_id, expect_rev, op }).is_ok()
    }

    /// Queue a raw message (used for `Bye` and keep-alive).
    pub fn send_raw(&self, msg: ClientMsg) -> bool {
        self.tx.send(msg).is_ok()
    }

    /// Remember which revision is on screen.
    pub fn note_revision(&self, revision: u64) {
        self.rev.store(revision, Ordering::Relaxed);
    }

    /// The revision currently on screen.
    pub fn revision(&self) -> u64 {
        self.rev.load(Ordering::Relaxed)
    }
}

impl App {
    /// Whether this process is a mirror (a client) rather than the owner.
    pub fn is_mirror(&self) -> bool {
        self.mirror.is_some()
    }

    /// Queue a command for the server. Returns whether it was queued, so a
    /// caller can read as "if `self.forward(op) { return; }`".
    pub(crate) fn forward(&self, op: Op) -> bool {
        match self.mirror.as_ref() {
            Some(mirror) => mirror.send(op),
            None => false,
        }
    }

    /// Attach the outbox that turns this `App` into a mirror.
    pub fn attach_mirror(&mut self, mirror: Mirror) {
        self.mirror = Some(mirror);
    }

    /// Replace the rendered state with the server's, and remember the revision.
    ///
    /// Only the *shared* half is replaced: the cursor, overlay, form and status
    /// line are this screen's own, and overwriting them from a snapshot would
    /// close a form someone is typing into because another client pressed a key.
    ///
    /// A snapshot that is not newer than what is already applied is ignored, so a
    /// duplicated or out-of-order frame cannot roll the table backwards.
    pub fn apply_snapshot(&mut self, snapshot: &crate::ipc::Snapshot) -> bool {
        let Some(mirror) = self.mirror.as_ref() else {
            return false;
        };
        if snapshot.revision < mirror.revision() {
            return false;
        }
        mirror.note_revision(snapshot.revision);
        self.tunnels = snapshot.to_views();
        self.client_count = snapshot.client_count;
        self.keep_server = snapshot.keep_server;
        self.cfg.tunnels = snapshot.tunnels.iter().map(|t| t.cfg.clone()).collect();
        self.clamp_selection();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::ipc::Snapshot;

    fn mirror_app() -> (tempfile::TempDir, App, std::sync::mpsc::Receiver<ClientMsg>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(paths);
        app.attach_mirror(Mirror::new(tx));
        (dir, app, rx)
    }

    #[test]
    fn a_mirror_sends_the_command_it_was_asked_to() {
        let (_d, app, rx) = mirror_app();
        assert!(app.is_mirror());
        assert!(app.forward(Op::Start { tunnel_id: 4 }));
        match rx.try_recv().expect("a command must be queued") {
            ClientMsg::Command { op, .. } => assert_eq!(op, Op::Start { tunnel_id: 4 }),
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn an_owner_never_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let app = App::new(paths);
        assert!(!app.is_mirror());
        assert!(!app.forward(Op::StopAll), "an owner must act locally, not queue");
    }

    #[test]
    fn request_ids_are_unique() {
        let (_d, app, rx) = mirror_app();
        for _ in 0..3 {
            assert!(app.forward(Op::StopAll));
        }
        let mut ids = Vec::new();
        while let Ok(ClientMsg::Command { req_id, .. }) = rx.try_recv() {
            ids.push(req_id);
        }
        assert_eq!(ids.len(), 3);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3, "every command needs its own request id");
    }

    #[test]
    fn the_first_command_carries_no_expect_rev() {
        // Before the first snapshot there is no view to be stale against.
        let (_d, app, rx) = mirror_app();
        app.forward(Op::StopAll);
        match rx.try_recv().expect("queued") {
            ClientMsg::Command { expect_rev, .. } => assert_eq!(expect_rev, None),
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn after_a_snapshot_the_expect_rev_is_the_applied_revision() {
        let (_d, mut app, rx) = mirror_app();
        app.apply_snapshot(&Snapshot::from_views(9, 0, 1, false, &[]));
        app.forward(Op::StopAll);
        match rx.try_recv().expect("queued") {
            ClientMsg::Command { expect_rev, .. } => {
                assert_eq!(expect_rev, Some(9), "the command must name the view it was built on")
            }
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn an_older_snapshot_is_ignored() {
        let (_d, mut app, _rx) = mirror_app();
        assert!(app.apply_snapshot(&Snapshot::from_views(9, 0, 1, false, &[])));
        assert!(!app.apply_snapshot(&Snapshot::from_views(4, 0, 1, false, &[])), "must not roll back");
        assert_eq!(app.mirror.as_ref().map(|m| m.revision()), Some(9));
    }

    #[test]
    fn a_snapshot_replaces_the_shared_state_but_not_the_cursor_or_overlay() {
        let (_d, mut app, _rx) = mirror_app();
        app.selected = 0;
        app.open_help();
        let cfg = crate::model::TunnelConfig {
            id: 3,
            name: "from-server".into(),
            kind: crate::model::ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 1234,
            remote_host: "127.0.0.1".into(),
            remote_port: 80,
            enabled: true,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        };
        let views = vec![crate::model::TunnelView::new(cfg)];
        app.apply_snapshot(&Snapshot::from_views(2, 0, 2, false, &views));
        assert_eq!(app.tunnels.len(), 1, "the table must come from the server");
        assert_eq!(app.tunnels[0].cfg.name, "from-server");
        assert_eq!(app.client_count, 2);
        assert!(matches!(app.overlay, crate::app::Overlay::Help), "a local overlay must survive a snapshot");
    }

    #[test]
    fn a_closed_connection_is_reported_not_panicked_on() {
        let (_d, app, rx) = mirror_app();
        drop(rx);
        assert!(!app.forward(Op::StopAll), "a dead connection reports failure instead of panicking");
    }
}
