//! Opening one authenticated session, including a `ProxyJump` chain.
//!
//! The shape is: [`timeout_connect`] dials TCP with a deadline, [`session`]
//! upgrades that stream to an authenticated SSH session, and [`connect`] chains
//! them for `ProxyJump` (each hop rides a `direct-tcpip` channel on the previous
//! connection).
//!
//! Two subtleties that used to be bugs, kept explicit here:
//!
//! * the connect call carries **no** `.context()`: `russh` reports a refused host
//!   key as its generic `UnknownKey`, so the handler's verdict is the only thing
//!   that can explain the refusal — contextualising would drop that;
//! * the handler's recorded verdict is checked *twice* (once when the connect
//!   fails, once after it succeeds), because a refusal can also be recorded
//!   without the connect failing (a later re-exchange).

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use russh::client;
use tokio::net::TcpStream;

use crate::model::TunnelId;
use crate::ssh::config::{self, Endpoint};
use crate::ssh::known_hosts;
use crate::tunnel::TunnelEvent;

use super::auth;
use super::classify::host_key_rejected;
use super::handler::{Handler, SharedVerdict};

/// How long to wait for the TCP + SSH handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Keepalive probe interval; matches the old `ServerAliveInterval=15`.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// Give up after this many unanswered keepalives (`ServerAliveCountMax=3`).
const KEEPALIVE_MAX: usize = 3;

/// Verify-what-you-connect-to settings for one run.
#[derive(Default)]
pub struct Options {
    pub host_key_policy: known_hosts::HostKeyPolicy,
}

/// TCP connect with a timeout and a readable error.
pub(crate) async fn timeout_connect(host: &str, port: u16, what: &str) -> Result<TcpStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow!("{what} {host}:{port} 超时"))?
        .with_context(|| format!("{what} {host}:{port} 失败"))
}

/// Open one authenticated session over `stream`.
pub(crate) async fn session<S>(
    stream: S,
    endpoint: &Endpoint,
    opts: &Options,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    local_target: Option<(String, u16)>,
) -> Result<Arc<client::Handle<Handler>>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let shared = SharedVerdict::new(opts.host_key_policy);
    let handler =
        Handler { id, tx, host: endpoint.hostname.clone(), port: endpoint.port, shared: shared.clone(), local_target };
    let config = Arc::new(client::Config {
        inactivity_timeout: None, // a tunnel must survive idle periods
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        nodelay: true,
        ..<_>::default()
    });

    let host = endpoint.hostname.clone();
    let port = endpoint.port;
    // NB: no `.context()` on this call — see the module docs.
    let mut handle = match client::connect_stream(config, stream, handler).await {
        Ok(h) => h,
        Err(e) => {
            // The handler records its verdict before russh returns the error.
            let verdict = shared.take();
            return Err(match verdict {
                Some(v) if !v.is_acceptable(opts.host_key_policy) => host_key_rejected(v, host, port),
                _ => anyhow::Error::new(e).context(format!("连接 {host}:{port} 失败")),
            });
        }
    };

    // A refusal can also be recorded without the connect failing (e.g. a later
    // re-exchange), so double-check.
    if let Some(v) = shared.take()
        && !v.is_acceptable(opts.host_key_policy)
    {
        return Err(host_key_rejected(v, host, port));
    }

    // Name the hop in auth errors — a ProxyJump chain would otherwise not say
    // *which* host rejected the key.
    auth::authenticate_hop(&mut handle, endpoint).await?;
    Ok(Arc::new(handle))
}

/// Dial one endpoint, following `ProxyJump` when configured.
pub(crate) async fn connect(
    endpoint: &Endpoint,
    opts: &Options,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    local_target: Option<(String, u16)>,
) -> Result<Arc<client::Handle<Handler>>> {
    if endpoint.proxy_jump.is_empty() {
        let stream = timeout_connect(&endpoint.hostname, endpoint.port, "连接目标主机").await?;
        return session(stream, endpoint, opts, id, tx, local_target).await;
    }

    let hops = resolve_hops(endpoint)?;
    let Some(first) = hops.first().cloned() else {
        bail!("ProxyJump 为空，无法确定第一跳");
    };
    let stream = timeout_connect(&first.hostname, first.port, "连接跳板机").await?;
    let mut handle = session(stream, &first, opts, id, tx.clone(), None).await?;

    // Intermediate hops never need the `-R` target; only the final session does.
    for hop in hops.iter().skip(1) {
        handle = hop_through(&handle, hop, opts, id, tx.clone(), None).await?;
    }
    hop_through(&handle, endpoint, opts, id, tx, local_target).await
}

/// Resolve every `ProxyJump` alias into an endpoint, in order.
///
/// `proxy_jump` is non-empty at the only call site and every alias is resolved
/// with `?`, so the returned vector cannot be empty — but the caller still checks
/// `first()` instead of indexing, because that is one line and cannot panic.
fn resolve_hops(endpoint: &Endpoint) -> Result<Vec<Endpoint>> {
    let mut hops: Vec<Endpoint> = Vec::with_capacity(endpoint.proxy_jump.len().saturating_add(1));
    for alias in &endpoint.proxy_jump {
        hops.push(config::jump_endpoint(alias).with_context(|| format!("解析跳板机 {alias} 失败"))?);
    }
    Ok(hops)
}

/// Open a channel on `handle` towards `hop` and authenticate over it.
async fn hop_through(
    handle: &Arc<client::Handle<Handler>>,
    hop: &Endpoint,
    opts: &Options,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    local_target: Option<(String, u16)>,
) -> Result<Arc<client::Handle<Handler>>> {
    let channel = handle
        .channel_open_direct_tcpip(hop.hostname.clone(), u32::from(hop.port), "127.0.0.1", 1234)
        .await
        .with_context(|| format!("经跳板机连接 {}:{} 失败", hop.hostname, hop.port))?;
    session(channel.into_stream(), hop, opts, id, tx, local_target).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_proxy_jump_resolves_to_no_hops() {
        let endpoint =
            Endpoint { hostname: "h".into(), port: 22, user: None, proxy_jump: Vec::new(), identity_file: None };
        assert!(resolve_hops(&endpoint).unwrap().is_empty());
    }
}
