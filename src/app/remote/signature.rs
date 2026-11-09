//! A cheap fingerprint of what a snapshot would show, so the server can tell
//! whether a new revision is worth broadcasting.
//!
//! Comparing whole `Snapshot`s would not work: every snapshot carries
//! `server_now_ms`, so two consecutive ones always differ even when nothing the
//! user can see has changed. The signature deliberately covers only observable
//! fields — identity, status variant and its reason, retries, the log length, and
//! the definition. A collision would cost at most one skipped broadcast; the next
//! real change moves the hash again.
//!
//! Belongs here: the change detector.
//! Does not belong: building a snapshot ([`crate::ipc::snapshot`]) or deciding
//! when to send one ([`crate::server`]).

use crate::app::App;
use crate::model::TunnelStatus;

impl App {
    /// A cheap fingerprint of everything a snapshot would show.
    ///
    /// The server needs to know whether a frame would say anything new; building
    /// a full `Snapshot` every tick and comparing it would work, but the
    /// timestamps inside it change on every frame, so it would always differ.
    /// This hash covers only the observable fields (identity, status, retries,
    /// log length, definition), so two equal hashes mean no client could see a
    /// difference. A hash collision would only ever cost one skipped broadcast;
    /// the next real change moves it again.
    pub fn state_signature(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |value: u64| {
            hash ^= value;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        for view in &self.tunnels {
            eat(view.cfg.id);
            eat(u64::from(view.cfg.local_port));
            eat(u64::from(view.cfg.remote_port));
            eat(u64::from(view.cfg.enabled));
            eat(view.retries as u64);
            // The status key covers every variant; the extra values cover the
            // fields that change without changing the variant.
            for byte in view.status.key().bytes() {
                eat(u64::from(byte));
            }
            let reason = match &view.status {
                TunnelStatus::Retrying { attempt, reason, .. } => {
                    eat(u64::from(*attempt));
                    Some(reason)
                }
                TunnelStatus::Failed { reason } => Some(reason),
                _ => None,
            };
            if let Some(reason) = reason {
                for byte in reason.short.bytes().chain(reason.detail.bytes()) {
                    eat(u64::from(byte));
                }
            }
            eat(view.log.len() as u64);
            for byte in view.cfg.name.bytes().chain(view.cfg.ssh_host.bytes()) {
                eat(u64::from(byte));
            }
        }
        hash
    }
}
