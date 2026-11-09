//! The wire types: the snapshot a client renders from.
//!
//! What belongs here: the serialisable mirror of the real crate's
//! `TunnelView` / `TunnelStatus`, plus the size estimate the protocol's
//! "full snapshot, not deltas" decision rests on. What does not: network IO
//! (`frame`), or the message enums (`msg`).
//!
//! The mirror exists because `TunnelStatus` carries `Instant`, which cannot
//! cross a process boundary. Here every time is an integer millisecond stamp on
//! the **server's** wall clock, and the client derives ages from differences.

use serde::{Deserialize, Serialize};

/// One tunnel's state as it travels over the socket.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireTunnelView {
    pub id: u64,
    pub name: String,
    /// `stopped` / `starting` / `up` / `retrying` / `failed`.
    pub state: String,
    /// When `state` began, on the server's wall clock (epoch ms).
    #[serde(default)]
    pub since_ms: u64,
    /// Which retry attempt is pending; only meaningful for `retrying`.
    #[serde(default)]
    pub attempt: u32,
    /// When the next retry fires, server clock (epoch ms).
    #[serde(default)]
    pub next_at_ms: u64,
    /// Short failure reason, only for `failed` / `retrying`.
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub retries: u32,
    /// Local listen port, so the snapshot is big enough to be a realistic size.
    #[serde(default)]
    pub local_port: u16,
    #[serde(default)]
    pub remote_port: u16,
    #[serde(default)]
    pub ssh_host: String,
}

/// The whole authoritative state of a server, as broadcast to every client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Strictly increasing; a client discards anything not newer.
    pub revision: u64,
    /// The server's wall clock when this snapshot was produced (epoch ms).
    pub server_now_ms: u64,
    /// How many clients are attached — drives the "last client exit" warning.
    pub client_count: u32,
    /// True once the server has entered its shutdown grace window.
    #[serde(default)]
    pub shutting_down: bool,
    pub tunnels: Vec<WireTunnelView>,
}

impl Snapshot {
    /// A deterministic snapshot of `n` tunnels, used for the codec size probe.
    pub fn sample(revision: u64, n: usize) -> Snapshot {
        let tunnels = (0..n)
            .map(|i| WireTunnelView {
                id: i as u64,
                name: format!("forward-{i}"),
                state: "up".to_string(),
                since_ms: 1_760_000_000_000 + i as u64,
                attempt: 0,
                next_at_ms: 0,
                reason: String::new(),
                retries: i as u32,
                local_port: 18_000 + i as u16,
                remote_port: 22,
                ssh_host: "bastion-prod".to_string(),
            })
            .collect();
        Snapshot { revision, server_now_ms: 1_760_000_000_000, client_count: 1, shutting_down: false, tunnels }
    }

    /// A compact human-readable digest used to assert two clients agree.
    pub fn digest(&self) -> String {
        format!("rev={} count={} tunnels={}", self.revision, self.client_count, self.tunnels.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_round_trips_through_toml() {
        let s = Snapshot::sample(7, 50);
        let text = toml::to_string(&s).unwrap();
        let back: Snapshot = toml::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    /// The number the protocol document quotes for "a 50-tunnel snapshot".
    #[test]
    fn report_the_size_of_a_fifty_tunnel_snapshot() {
        let s = Snapshot::sample(1, 50);
        let text = toml::to_string(&s).unwrap();
        println!("50-tunnel snapshot = {} bytes", text.len());
        assert!(text.len() < crate::common::frame::MAX_FRAME);
    }
}
