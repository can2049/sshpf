//! The three forwarding drivers: `-L`, `-R` and `-D`.
//!
//! Each one takes an established [`Handle`](russh::client::Handle) and serves
//! connections until it fails or its task is cancelled. `ready()` is called once
//! the forward is *genuinely* established — the `bind` returned `Ok`, or
//! `tcpip_forward` awaited the server's reply. That signal is what replaces the
//! old stderr-marker heuristics and their false-ready cases.
//!
//! A dead target never tears the tunnel down: `ssh -L` and `ssh -D` behave the
//! same way (the connection is refused, the forward survives), so the failure is
//! logged and reported as a progress line instead.
//!
//! # The health check belongs here, not above
//!
//! Every serve loop below races its work against the liveness probe
//! ([`super::health`]). It has to live inside the loop rather than in a second
//! spawned task because the *verdict* is a run-ending error: whichever branch
//! finishes first is what the driver returns, and returning is how a stale
//! session becomes an ordinary `Exited` that the app already knows how to
//! retry. A separate task would leave the serve loop alive next to a dead
//! session, which is the exact bug the probe exists to fix.
//!
//! `serve_remote` needs the same treatment for a different reason: its body is
//! `pending()`, so without the probe it would stay `●` forever after its session
//! died, and the server-side listener would keep pointing at a closed channel.

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use russh::client;
use tokio::net::TcpListener;

use crate::eventlog;
use crate::model::TunnelEvent;
use crate::model::{TunnelConfig, TunnelId};
use crate::ssh::socks5;

use super::handler::Handler;
use super::health;

/// A dead session as an error, or never resolve.
///
/// Split out of the serve loops so each `select!` reads as "serve, or notice the
/// session died" without repeating the `Option` dance three times. The
/// `tunnel.health_failed` record is written here rather than at the three call
/// sites because this is the one place that knows both the cause and the
/// interval — and it must be written *before* the error travels up, so the file
/// explains the exit even if the app is killed mid-retry.
async fn or_stale(
    cfg: &TunnelConfig,
    health_check: Option<Duration>,
    handle: Arc<client::Handle<Handler>>,
) -> Result<()> {
    match health_check {
        Some(interval) => {
            let cause = health::watch(handle, interval).await;
            let info = health::stale_failure(cause);
            eventlog::tunnel_health_failed(cfg, cause.name(), &info.detail, interval.as_millis());
            Err(health::stale_error(cause))
        }
        // Disabled by configuration: this branch can never be selected.
        None => std::future::pending().await,
    }
}

/// `-L`: listen locally, tunnel each connection to a target the server can reach.
pub(crate) async fn serve_local(
    cfg: &TunnelConfig,
    handle: Arc<client::Handle<Handler>>,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    ready: impl FnOnce(),
    health_check: Option<Duration>,
) -> Result<()> {
    let listener = TcpListener::bind((cfg.local_addr.as_str(), cfg.local_port))
        .await
        .with_context(|| format!("监听 {}:{} 失败", cfg.local_addr, cfg.local_port))?;
    let _ = tx.send(TunnelEvent::Line {
        id,
        line: format!(
            "本地转发已就绪：{}:{} → {}:{}",
            cfg.local_addr, cfg.local_port, cfg.remote_host, cfg.remote_port
        ),
    });
    ready();

    // Built **once**, before the loop: the probe counts from the last verdict,
    // not from the last accepted connection. Re-creating it inside `select!`
    // would restart the interval every time a connection arrives, so a busy
    // tunnel would never be probed at all.
    let stale = or_stale(cfg, health_check, handle.clone());
    tokio::pin!(stale);

    loop {
        tokio::select! {
            stale = &mut stale => stale?,
            accepted = listener.accept() => {
                let (mut socket, origin) = accepted.context("accept 失败")?;
                let handle = handle.clone();
                let (th, tp) = (cfg.remote_host.clone(), cfg.remote_port);
                let tx = tx.clone();
                tokio::spawn(async move {
                    match handle
                        .channel_open_direct_tcpip(
                            th.clone(),
                            u32::from(tp),
                            origin.ip().to_string(),
                            u32::from(origin.port()),
                        )
                        .await
                    {
                        Ok(channel) => {
                            let mut stream = channel.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                        }
                        Err(e) => {
                            // A dead target must not tear the tunnel down — this matches
                            // `ssh -L` (the connection is refused, the forward survives).
                            eventlog::forward_target_failed(id, &format!("{th}:{tp}"), &e.to_string());
                            let _ = tx.send(TunnelEvent::Line { id, line: format!("转发到 {th}:{tp} 失败: {e}") });
                        }
                    }
                });
            }
        }
    }
}

/// `-R`: ask the server to listen, serving inbound connections to our target.
pub(crate) async fn serve_remote(
    cfg: &TunnelConfig,
    handle: Arc<client::Handle<Handler>>,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    ready: impl FnOnce(),
    health_check: Option<Duration>,
) -> Result<()> {
    let listen_host = cfg.remote_host.clone();
    let listen_port = cfg.remote_port;
    let granted = handle
        .tcpip_forward(listen_host.clone(), u32::from(listen_port))
        .await
        .with_context(|| format!("请求远端监听 {listen_host}:{listen_port} 失败"))?;
    // For a non-zero requested port russh returns `Ok(0)`; only a request of 0
    // yields the server-assigned port. Do not use `granted` blindly.
    let effective_port = if listen_port == 0 { u16::try_from(granted).unwrap_or(0) } else { listen_port };
    let _ = tx.send(TunnelEvent::Line {
        id,
        line: format!("远端转发已就绪：{listen_host}:{effective_port} → {}:{}", cfg.local_addr, cfg.local_port),
    });
    ready();

    // Inbound channels are dispatched by the handler (which already knows the
    // local target); this future only has to keep the session alive — and to
    // notice when it is not alive any more.
    tokio::select! {
        stale = or_stale(cfg, health_check, handle) => stale,
        () = std::future::pending::<()>() => Ok(()),
    }
}

/// `-D`: local SOCKS5 proxy over the session.
pub(crate) async fn serve_dynamic(
    cfg: &TunnelConfig,
    handle: Arc<client::Handle<Handler>>,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    ready: impl FnOnce(),
    health_check: Option<Duration>,
) -> Result<()> {
    let listener = TcpListener::bind((cfg.local_addr.as_str(), cfg.local_port))
        .await
        .with_context(|| format!("监听 {}:{} 失败", cfg.local_addr, cfg.local_port))?;
    let _ = tx
        .send(TunnelEvent::Line {
            id, line: format!("SOCKS5 代理已就绪：{}:{}", cfg.local_addr, cfg.local_port)
        });
    ready();

    // Built **once**, before the loop: the probe counts from the last verdict,
    // not from the last accepted connection. Re-creating it inside `select!`
    // would restart the interval every time a connection arrives, so a busy
    // tunnel would never be probed at all.
    let stale = or_stale(cfg, health_check, handle.clone());
    tokio::pin!(stale);

    loop {
        tokio::select! {
            stale = &mut stale => stale?,
            accepted = listener.accept() => {
                let (mut socket, _origin) = accepted.context("accept 失败")?;
                let handle = handle.clone();
                let socks_id = id;
                tokio::spawn(async move {
                    if socks5::negotiate(&mut socket).await.is_err() {
                        eventlog::socks5_rejected(socks_id, "SOCKS5 握手失败");
                        return;
                    }
                    let Ok(req) = socks5::read_request(&mut socket).await else {
                        eventlog::socks5_rejected(socks_id, "SOCKS5 请求无法解析");
                        return;
                    };
                    match handle.channel_open_direct_tcpip(req.host.clone(), u32::from(req.port), "127.0.0.1", 0).await {
                        Ok(channel) => {
                            if socks5::reply_success(&mut socket).await.is_err() {
                                return;
                            }
                            let mut stream = channel.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                        }
                        Err(e) => {
                            eventlog::socks5_rejected(socks_id, &format!("{}:{} 打开失败: {e}", req.host, req.port));
                            socks5::reply_failure(&mut socket).await
                        }
                    }
                });
            }
        }
    }
}
