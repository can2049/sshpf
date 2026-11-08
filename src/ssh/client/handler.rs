//! The per-session `russh` handler: verify the server key, and serve `-R` channels.
//!
//! One [`Handler`] exists per SSH session (one per ProxyJump hop). It owns:
//!
//! * the **host-key verdict**, shared back to the connector through
//!   [`SharedVerdict`] because `client::connect_stream` does not hand the handler
//!   back — and a refusal reported by `russh` is a generic `UnknownKey` with no
//!   explanation of *why*;
//! * `local_target` for `-R` sessions: when the server opens a forwarded channel
//!   we dial that target locally. `-L`/`-D` sessions never receive one.
//!
//! Everything a refusal should leave behind is logged here, because this is the
//! only place that knows the verdict; once the TUI is gone there is no other
//! trace.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use russh::client;
use russh::keys::PublicKeyOrCertificate;
use tokio::net::TcpStream;

use crate::eventlog;
use crate::model::TunnelId;
use crate::ssh::known_hosts;
use crate::tunnel::TunnelEvent;

/// Policy plus the last verdict, shared with the handler so we can explain a
/// refusal after `connect_stream` (which does not hand the handler back).
#[derive(Clone)]
pub(crate) struct SharedVerdict {
    pub(crate) policy: known_hosts::HostKeyPolicy,
    last: Arc<Mutex<Option<known_hosts::HostKeyVerdict>>>,
}

impl SharedVerdict {
    pub(crate) fn new(policy: known_hosts::HostKeyPolicy) -> Self {
        SharedVerdict { policy, last: Arc::new(Mutex::new(None)) }
    }

    fn record(&self, v: known_hosts::HostKeyVerdict) {
        if let Ok(mut slot) = self.last.lock() {
            *slot = Some(v);
        }
    }

    pub(crate) fn take(&self) -> Option<known_hosts::HostKeyVerdict> {
        self.last.lock().ok().and_then(|mut s| s.take())
    }
}

/// `Handler` for one session: verifies the server key and serves `-R` channels.
pub(crate) struct Handler {
    pub(crate) id: TunnelId,
    pub(crate) tx: Sender<TunnelEvent>,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) shared: SharedVerdict,
    /// Where to dial when the server opens a forwarded channel (`-R`). `None`
    /// for `-L`/`-D`, where the server never opens channels towards us.
    pub(crate) local_target: Option<(String, u16)>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        // `public_key()` normalises both variants (certificates unwrap to their
        // key) so verification has one shape to compare.
        let public = key.public_key();
        let verdict = known_hosts::verify(&self.host, self.port, &public);
        let accept = verdict.is_acceptable(self.shared.policy);
        if accept
            && matches!(verdict, known_hosts::HostKeyVerdict::Unknown)
            && self.shared.policy == known_hosts::HostKeyPolicy::AcceptNew
            && let Err(e) = known_hosts::learn(&self.host, self.port, &public)
        {
            // A failed write must not silently fall back to "accept anyway".
            self.shared.record(verdict);
            return Err(russh::Error::IO(std::io::Error::other(format!("写入 known_hosts 失败: {e}"))));
        }
        // Every verdict is recorded: this is the only place that knows whether a
        // session was refused for a security reason, and a refused session leaves
        // no other trace once the TUI is gone.
        let policy = eventlog::host_key_policy_name(self.shared.policy);
        match &verdict {
            known_hosts::HostKeyVerdict::Trusted => {}
            known_hosts::HostKeyVerdict::Unknown if accept => {
                eventlog::host_key_learned(self.id, &self.host, self.port, "unknown")
            }
            _ => {
                if !accept {
                    let detail = verdict.describe(&self.host, self.port);
                    eventlog::host_key_rejected(self.id, &self.host, self.port, policy, &detail);
                }
            }
        }
        self.shared.record(verdict);
        Ok(accept)
    }

    /// `-R`: the server accepted our `tcpip-forward` and has an inbound
    /// connection for the port it listens on. Connect it to our local target.
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> std::result::Result<(), Self::Error> {
        let Some((host, port)) = self.local_target.clone() else {
            reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        reply.accept().await;
        let _ = self.tx.send(TunnelEvent::Line {
            id: self.id,
            line: format!("远端连接 {connected_address}:{connected_port}（来自 {originator_address}）→ {host}:{port}"),
        });
        let id = self.id;
        tokio::spawn(async move {
            let Ok(mut socket) = TcpStream::connect((host.as_str(), port)).await else {
                // Local target down: drop this one connection, keep the tunnel.
                // Worth a record — otherwise the user sees a silent hang on the
                // remote side with nothing explaining it.
                eventlog::forward_target_failed(id, &format!("{host}:{port}"), "本地目标拒绝连接");
                return;
            };
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
        });
        Ok(())
    }
}
