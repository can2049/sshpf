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

impl Handler {
    /// Place every record about a verdict, in one place.
    ///
    /// Split out of [`client::Handler::check_server_key`] so the four outcomes
    /// (`trusted` / learned / ignored-changed / rejected) stay visible as one
    /// table instead of being interleaved with the accept/reject bookkeeping.
    fn record_verdict(&self, verdict: &known_hosts::HostKeyVerdict, public: &russh::keys::PublicKey, accept: bool) {
        let policy = eventlog::host_key_policy_name(self.shared.policy);
        match verdict {
            known_hosts::HostKeyVerdict::Trusted => {}
            // `knownhosts.learned` is the line that explains "why does this host
            // now connect without asking?" — i.e. it is a claim that a trust
            // anchor was written. Only `accept-new` writes one; under `Ignore`
            // an unknown host connects without anything being persisted, so
            // claiming it was learned would send the reader looking for a
            // `known_hosts` entry that does not exist.
            known_hosts::HostKeyVerdict::Unknown
                if accept && self.shared.policy == known_hosts::HostKeyPolicy::AcceptNew =>
            {
                eventlog::host_key_learned(self.id, &self.host, self.port, "unknown")
            }
            known_hosts::HostKeyVerdict::Unknown if accept => {}
            // A changed key that was accepted is only possible under `Ignore`
            // (`is_acceptable` admits nothing else), and it is the one accepted
            // outcome the user must be told about: the trust anchor on disk and
            // what the server presented no longer agree.
            known_hosts::HostKeyVerdict::Changed { line } if accept => {
                eventlog::host_key_ignored_changed(
                    self.id,
                    &self.host,
                    self.port,
                    *line,
                    &known_hosts::fingerprint(public),
                );
                // A log record alone is invisible to someone watching the TUI.
                // The line is prefixed 「警告」 so `App::on_line` raises it to a
                // visible status warning instead of a normal progress line.
                let _ = self.tx.send(TunnelEvent::Line {
                    id: self.id,
                    line: format!(
                        "警告: {} 的主机密钥已改变（已按忽略策略接受，未写入 known_hosts）",
                        known_hosts::describe_host(&self.host, self.port)
                    ),
                });
            }
            _ if !accept => {
                let detail = verdict.describe(&self.host, self.port);
                eventlog::host_key_rejected(self.id, &self.host, self.port, policy, &detail);
            }
            _ => {}
        }
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        // `public_key()` normalises both variants (certificates unwrap to their
        // key) so verification has one shape to compare.
        let public = key.public_key();
        let verdict = known_hosts::verify(&self.host, self.port, &public);
        let policy = self.shared.policy;
        let accept = verdict.is_acceptable(policy);
        // Only `accept-new` persists what it learns. `Ignore` deliberately does
        // not: rewriting the trust anchor is the one thing a user who said
        // "ignore the change" is least likely to want done silently.
        if accept
            && matches!(verdict, known_hosts::HostKeyVerdict::Unknown)
            && policy == known_hosts::HostKeyPolicy::AcceptNew
            && let Err(e) = known_hosts::learn(&self.host, self.port, &public)
        {
            // A failed write must not silently fall back to "accept anyway".
            self.shared.record(verdict);
            return Err(russh::Error::IO(std::io::Error::other(format!("写入 known_hosts 失败: {e}"))));
        }
        self.record_verdict(&verdict, &public, accept);
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
