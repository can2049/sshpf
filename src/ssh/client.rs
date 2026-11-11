//! russh client: connect, authenticate, verify the host key, and run the three
//! forwarding modes.
//!
//! This replaces the `ssh` subprocess entirely. Unlike the process model it
//! gives us *typed* outcomes instead of stderr text, so readiness and failure
//! are no longer inferred:
//!
//! * a `-L` listener is "ready" because `TcpListener::bind` returned `Ok`;
//! * a `-R` forward is "ready" because `tcpip_forward()` awaited the server's
//!   `GLOBAL_REQUEST` reply;
//! * failures arrive as [`russh::Error`] variants, mapped to [`FailureKind`].
//!
//! The old stderr markers (`Entering interactive session.`,
//! `remote forward success for`, `Local forwarding listening on`) and the whole
//! staged-readiness state machine they supported are gone — the false-ready
//! cases they worked around cannot occur here.
//!
//! # Where things live
//!
//! | file | contents |
//! |---|---|
//! | [`classify`] | `classify`, the text fallback, `short_reason`, `HostKeyRejected` |
//! | [`handler`] | `russh::client::Handler` impl: server-key verification, `-R` channels |
//! | [`auth`] | credential loading (IdentityFile / ssh-agent) and `authenticate` |
//! | [`connect`] | TCP connect with deadline, `session()`, the `ProxyJump` chain |
//! | [`driver`] | `serve_local` / `serve_remote` / `serve_dynamic` |
//! | [`health`] | periodic liveness probe of a live session, its verdicts |
//! | [`probe`] | run one command on the host and capture its stdout (port scan) |
//!
//! The public entry point is [`run`]; [`Options`] and [`classify`] are
//! re-exported so `crate::ssh::client::{run, classify, Options}` keeps working.
//!
//! # Threading
//!
//! Every tunnel owns one background thread with a `current_thread` tokio
//! runtime (see [`crate::tunnel`]). The UI thread stays synchronous and receives
//! [`TunnelEvent`](crate::model::TunnelEvent)s over a `std::sync::mpsc` channel.
//! That enum lives in `model` precisely so this layer does not have to depend on
//! `tunnel`, which sits above it.

use std::sync::mpsc::Sender;

use anyhow::Result;

use crate::model::TunnelEvent;
use crate::model::{ForwardKind, TunnelConfig, TunnelId};
use crate::ssh::config;

mod auth;
mod classify;
mod connect;
mod driver;
mod handler;
mod health;
pub mod probe;

pub use classify::classify;
pub use connect::Options;

use connect::connect;
use driver::{serve_dynamic, serve_local, serve_remote};

/// Run one tunnel until it fails or its task is cancelled.
///
/// `ready` is signalled once the forward is genuinely established, which is what
/// replaces the old stderr-marker heuristics.
pub async fn run(
    cfg: &TunnelConfig,
    opts: &Options,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    ready: impl FnOnce(),
) -> Result<()> {
    let endpoint = config::endpoint_for(cfg)?;
    let _ = tx.send(TunnelEvent::Line { id, line: format!("连接 {}:{} …", endpoint.hostname, endpoint.port) });
    // Only `-R` needs a local target for inbound channels.
    let local_target = matches!(cfg.kind, ForwardKind::Remote).then(|| (cfg.local_addr.clone(), cfg.local_port));
    let handle = connect(&endpoint, opts, id, tx.clone(), local_target).await?;
    let _ = tx.send(TunnelEvent::Line { id, line: format!("已连接 {}", endpoint.hostname) });

    // The probe interval travels with the run: it is a per-run setting, and the
    // drivers are the only place that can race it against the serve loop.
    let health_check = opts.health_check;
    match cfg.kind {
        ForwardKind::Local => serve_local(cfg, handle, id, tx, ready, health_check).await,
        ForwardKind::Remote => serve_remote(cfg, handle, id, tx, ready, health_check).await,
        ForwardKind::Dynamic => serve_dynamic(cfg, handle, id, tx, ready, health_check).await,
    }
}
