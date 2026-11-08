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
//! # Threading
//!
//! Every tunnel owns one background thread with a `current_thread` tokio
//! runtime (see [`crate::tunnel`]). The UI thread stays synchronous and receives
//! [`TunnelEvent`]s over a `std::sync::mpsc`
//! channel, exactly as it did with child processes.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use russh::client;
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use tokio::net::{TcpListener, TcpStream};

use crate::app::log as eventlog;
use crate::model::{FailureInfo, FailureKind, ForwardKind, TunnelConfig, TunnelId};
use crate::ssh::config::{self, Endpoint};
use crate::ssh::{known_hosts, socks5};
use crate::tunnel::TunnelEvent;

/// How long to wait for the TCP + SSH handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Keepalive probe interval; matches the old `ServerAliveInterval=15`.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// Give up after this many unanswered keepalives (`ServerAliveCountMax=3`).
const KEEPALIVE_MAX: usize = 3;

// ---------------------------------------------------------------------------
// failure classification (replaces stderr regexes)
// ---------------------------------------------------------------------------

/// Map an error onto a user-facing [`FailureInfo`].
///
/// Replaces the old `classify_failure(stderr_line)` regexes: `russh` reports
/// typed errors, so we branch on the variant and keep the text only as a
/// fallback (and as the `detail` shown in the panel).
pub fn classify(err: &anyhow::Error) -> FailureInfo {
    let text = format!("{err:#}");
    let lower = text.to_lowercase();

    if let Some(rejected) = find_host_key_rejection(err) {
        let detail = rejected.to_string();
        return FailureInfo {
            kind: FailureKind::HostKeyMismatch,
            short: short_reason(FailureKind::HostKeyMismatch, &detail),
            detail,
        };
    }
    let kind = match find_russh_error(err) {
        Some(russh::Error::IO(io)) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => FailureKind::ConnectionRefused,
            std::io::ErrorKind::TimedOut => FailureKind::Timeout,
            std::io::ErrorKind::AddrInUse => FailureKind::PortInUse,
            std::io::ErrorKind::PermissionDenied => FailureKind::PermissionDenied,
            _ if lower.contains("lookup address") || lower.contains("name resolution") => FailureKind::DnsFailure,
            _ => FailureKind::Unknown,
        },
        Some(russh::Error::RequestDenied) => FailureKind::RemoteForwardFailed,
        Some(russh::Error::ChannelOpenFailure(russh::ChannelOpenFailure::ConnectFailed)) => {
            FailureKind::TargetUnreachable
        }
        Some(russh::Error::ChannelOpenFailure(_)) => FailureKind::Unknown,
        Some(russh::Error::Disconnect) => FailureKind::Exited,
        // russh reports a rejected host key as a generic UnknownKey; the detail
        // text is checked below, but recognise the common spellings here too.
        Some(russh::Error::UnknownKey) => FailureKind::HostKeyMismatch,
        Some(_) => FailureKind::Unknown,
        None => classify_text(&lower),
    };

    FailureInfo { kind, short: short_reason(kind, &text), detail: text }
}

/// Text fallback for errors we authored ourselves (context strings).
fn classify_text(lower: &str) -> FailureKind {
    if lower.contains("address already in use") {
        FailureKind::PortInUse
    } else if lower.contains("主机密钥")
        || lower.contains("known_hosts")
        // russh's own wording when `check_server_key` returned false.
        || lower.contains("unknown server key")
        || lower.contains("server key changed")
    {
        FailureKind::HostKeyMismatch
    } else if lower.contains("认证") || lower.contains("agent") {
        FailureKind::AuthFailed
    } else if lower.contains("lookup address") || lower.contains("name resolution") || lower.contains("dns") {
        FailureKind::DnsFailure
    } else if lower.contains("connection refused") {
        FailureKind::ConnectionRefused
    } else if lower.contains("timed out") || lower.contains("超时") {
        FailureKind::Timeout
    } else if lower.contains("permission denied") {
        FailureKind::PermissionDenied
    } else if lower.contains("远端转发") || lower.contains("请求远端监听") {
        FailureKind::RemoteForwardFailed
    } else {
        FailureKind::Unknown
    }
}

/// A refused server key, carrying the verdict so the caller can explain it.
#[derive(Debug)]
pub struct HostKeyRejected {
    pub verdict: known_hosts::HostKeyVerdict,
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for HostKeyRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.verdict.describe(&self.host, self.port))
    }
}

impl std::error::Error for HostKeyRejected {}

/// Find a [`HostKeyRejected`] anywhere in the error chain.
fn find_host_key_rejection(err: &anyhow::Error) -> Option<&HostKeyRejected> {
    err.chain().find_map(|c| c.downcast_ref::<HostKeyRejected>())
}

/// Find a `russh::Error` anywhere in the error chain.
fn find_russh_error(err: &anyhow::Error) -> Option<&russh::Error> {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<russh::Error>() {
            return Some(e);
        }
    }
    None
}

/// A short (<= 34 display cells) phrase for the table's error column.
fn short_reason(kind: FailureKind, text: &str) -> String {
    let s = match kind {
        FailureKind::PortInUse => "端口已被占用",
        FailureKind::PermissionDenied => "权限不足",
        FailureKind::AuthFailed => "认证失败",
        FailureKind::DnsFailure => "域名解析失败",
        FailureKind::ConnectionRefused => "连接被拒绝",
        FailureKind::Timeout => "连接超时",
        FailureKind::HostKeyMismatch => "主机密钥不匹配",
        FailureKind::RemoteForwardFailed => "远程转发失败",
        FailureKind::TargetUnreachable => "目标不可达",
        FailureKind::SpawnFailed => "连接失败",
        FailureKind::Exited => "连接已断开",
        FailureKind::Unsupported => "暂不支持",
        FailureKind::Unknown => "未知错误",
    };
    if kind == FailureKind::Unknown {
        // Give the table something actionable rather than a bare "unknown".
        let mut t = s.to_string();
        if let Some(first) = text.lines().next().map(str::trim).filter(|l| !l.is_empty()) {
            t.push_str(": ");
            t.push_str(first);
        }
        return crate::text::truncate_to_width(&t, 34);
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// host-key verification plumbing
// ---------------------------------------------------------------------------

/// Policy plus the last verdict, shared with the handler so we can explain a
/// refusal after `connect_stream` (which does not hand the handler back).
#[derive(Clone)]
struct SharedVerdict {
    policy: known_hosts::HostKeyPolicy,
    last: Arc<Mutex<Option<known_hosts::HostKeyVerdict>>>,
}

impl SharedVerdict {
    fn new(policy: known_hosts::HostKeyPolicy) -> Self {
        SharedVerdict { policy, last: Arc::new(Mutex::new(None)) }
    }

    fn record(&self, v: known_hosts::HostKeyVerdict) {
        if let Ok(mut slot) = self.last.lock() {
            *slot = Some(v);
        }
    }

    fn take(&self) -> Option<known_hosts::HostKeyVerdict> {
        self.last.lock().ok().and_then(|mut s| s.take())
    }
}

/// `Handler` for one session: verifies the server key and serves `-R` channels.
struct Handler {
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    host: String,
    port: u16,
    shared: SharedVerdict,
    /// Where to dial when the server opens a forwarded channel (`-R`). `None`
    /// for `-L`/`-D`, where the server never opens channels towards us.
    local_target: Option<(String, u16)>,
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
        let policy = match self.shared.policy {
            known_hosts::HostKeyPolicy::Strict => "strict",
            known_hosts::HostKeyPolicy::AcceptNew => "accept-new",
            known_hosts::HostKeyPolicy::Ask => "ask",
        };
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

// ---------------------------------------------------------------------------
// authentication
// ---------------------------------------------------------------------------

/// Credentials for one connection.
enum Auth {
    Key(Arc<russh::keys::PrivateKey>),
    /// Boxed so Unix socket and Windows named-pipe transports share one type.
    Agent(AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>),
}

/// Load credentials, preferring an explicit `IdentityFile`, then ssh-agent.
///
/// Async on purpose: `session` already runs inside the tunnel's runtime, so
/// building a second runtime here would panic with "Cannot start a runtime from
/// within a runtime" on every hop that has no `IdentityFile` (e.g. a jump host
/// whose key comes from the agent).
async fn load_auth(endpoint: &Endpoint) -> Result<Auth> {
    if let Some(path) = &endpoint.identity_file {
        let expanded = expand_path(path);
        if let Ok(key) = load_secret_key(&expanded, None) {
            return Ok(Auth::Key(Arc::new(key)));
        }
    }
    match connect_agent().await {
        Ok(agent) => Ok(Auth::Agent(agent)),
        Err(_) => bail!("没有可用的认证方式：未找到可读取的私钥，也连接不上 ssh-agent"),
    }
}

/// Boxed agent client, so the Unix socket and the Windows transports share one
/// type.
type AnyAgent = AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>;

/// Connect to whatever ssh-agent the platform offers.
///
/// * Unix: `$SSH_AUTH_SOCK`.
/// * Windows: the OpenSSH agent's named pipe, then Pageant. There is no
///   `SSH_AUTH_SOCK` on Windows, which is why this cannot be a single
///   `connect_env()` call — that function only exists in russh's `cfg(unix)` impl
///   and failed the Windows build until this was split out.
#[cfg(unix)]
async fn connect_agent() -> Result<AnyAgent> {
    let agent = AgentClient::connect_env().await.map_err(|e| anyhow!("连接 ssh-agent 失败: {e}"))?;
    Ok(agent.dynamic())
}

#[cfg(windows)]
async fn connect_agent() -> Result<AnyAgent> {
    // The OpenSSH-for-Windows agent listens on this well-known pipe.
    const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    match AgentClient::connect_named_pipe(OPENSSH_AGENT_PIPE).await {
        Ok(agent) => return Ok(agent.dynamic()),
        Err(pipe_err) => {
            // Fall back to PuTTY's Pageant, which is still common.
            match AgentClient::connect_pageant().await {
                Ok(agent) => Ok(agent.dynamic()),
                Err(pageant_err) => {
                    Err(anyhow!("连接 ssh-agent 失败: OpenSSH pipe: {pipe_err}; Pageant: {pageant_err}"))
                }
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
async fn connect_agent() -> Result<AnyAgent> {
    bail!("此平台没有可用的 ssh-agent 支持")
}

/// Expand a leading `~` / `%d` in an `IdentityFile` value.
fn expand_path(path: &str) -> String {
    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
    let p = path.replacen("%d", &home, 1);
    if let Some(rest) = p.strip_prefix("~/") { format!("{home}/{rest}") } else { p }
}

/// Try to authenticate, in the same order OpenSSH uses.
///
/// The `none` method is probed first: it is how a server says "this user needs
/// no authentication", and some gateways (e.g. a desk-ssh jump host) accept it
/// while rejecting a public key offered afterwards. Only when `none` is refused
/// do we load the configured key / ssh-agent. Returns `Ok(())` only when the
/// server accepted; there is no interactive password fallback by design
/// (`BatchMode=yes` in the old subprocess implementation).
async fn authenticate(handle: &mut client::Handle<Handler>, user: &str, endpoint: &Endpoint) -> Result<()> {
    let none = handle.authenticate_none(user).await.map_err(|e| anyhow!("{e}"))?;
    if none.success() {
        return Ok(());
    }
    let hash = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
    match load_auth(endpoint).await? {
        Auth::Key(key) => {
            let res = handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, hash))
                .await
                .map_err(|e| anyhow!("{e}"))?;
            if res.success() { Ok(()) } else { bail!("认证失败：密钥被拒绝（用户 {user}）") }
        }
        Auth::Agent(mut agent) => {
            let ids: Vec<AgentIdentity> =
                agent.request_identities().await.map_err(|e| anyhow!("读取 ssh-agent 失败: {e}"))?;
            if ids.is_empty() {
                bail!("ssh-agent 中没有任何密钥");
            }
            for id in ids {
                let pk = id.public_key().into_owned();
                // `AgentClient` implements russh's `Signer`, so the real signing
                // happens inside `authenticate_publickey_with`.
                match handle.authenticate_publickey_with(user, pk, hash, &mut agent).await {
                    Ok(r) if r.success() => return Ok(()),
                    Ok(_) => continue,
                    Err(e) => return Err(anyhow!("ssh-agent 签名失败: {e}")),
                }
            }
            bail!("认证失败：ssh-agent 中的密钥都被拒绝")
        }
    }
}

// ---------------------------------------------------------------------------
// connect (with ProxyJump)
// ---------------------------------------------------------------------------

/// Verify-what-you-connect-to settings for one run.
#[derive(Default)]
pub struct Options {
    pub host_key_policy: known_hosts::HostKeyPolicy,
}

/// Open one authenticated session over `stream`.
async fn session<S>(
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
    // NB: no `.context()` on this call. russh reports a refused host key as its
    // generic `UnknownKey`, so the *handler's* verdict is the only thing that can
    // explain the refusal. Contextualising the error here would drop that.
    let mut handle = match client::connect_stream(config, stream, handler).await {
        Ok(h) => h,
        Err(e) => {
            // The handler records its verdict before russh returns the error.
            let verdict = shared.take();
            return Err(match verdict {
                Some(v) if !v.is_acceptable(opts.host_key_policy) => {
                    anyhow::Error::new(HostKeyRejected { verdict: v, host, port })
                }
                _ => anyhow::Error::new(e).context(format!("连接 {host}:{port} 失败")),
            });
        }
    };

    // A refusal can also be recorded without the connect failing (e.g. a later
    // re-exchange), so double-check.
    if let Some(v) = shared.take()
        && !v.is_acceptable(opts.host_key_policy)
    {
        return Err(anyhow::Error::new(HostKeyRejected { verdict: v, host, port }));
    }

    // Name the hop in auth errors: with a ProxyJump chain the message would
    // otherwise not say *which* host rejected the key.
    authenticate(&mut handle, &endpoint.user_or_current(), endpoint)
        .await
        .with_context(|| format!("认证 {}:{} 失败", endpoint.hostname, endpoint.port))?;
    Ok(Arc::new(handle))
}

/// Dial one endpoint, following `ProxyJump` when configured.
async fn connect(
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

    // Resolve every hop, then chain: each hop rides a direct-tcpip channel on
    // the previous connection.
    let mut hops: Vec<Endpoint> = Vec::with_capacity(endpoint.proxy_jump.len() + 1);
    for alias in &endpoint.proxy_jump {
        hops.push(config::jump_endpoint(alias).with_context(|| format!("解析跳板机 {alias} 失败"))?);
    }
    let first = hops[0].clone();
    let stream = timeout_connect(&first.hostname, first.port, "连接跳板机").await?;
    let mut handle = session(stream, &first, opts, id, tx.clone(), None).await?;

    // Intermediate hops never need the `-R` target; only the final session does.
    for hop in hops.iter().skip(1) {
        handle = hop_through(&handle, hop, opts, id, tx.clone(), None).await?;
    }
    hop_through(&handle, endpoint, opts, id, tx, local_target).await
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

/// TCP connect with a timeout and a readable error.
async fn timeout_connect(host: &str, port: u16, what: &str) -> Result<TcpStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow!("{what} {host}:{port} 超时"))?
        .with_context(|| format!("{what} {host}:{port} 失败"))
}

// ---------------------------------------------------------------------------
// the three forwarding drivers
// ---------------------------------------------------------------------------

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

    match cfg.kind {
        ForwardKind::Local => serve_local(cfg, handle, id, tx, ready).await,
        ForwardKind::Remote => serve_remote(cfg, handle, id, tx, ready).await,
        ForwardKind::Dynamic => serve_dynamic(cfg, handle, id, tx, ready).await,
    }
}

/// `-L`: listen locally, tunnel each connection to a target the server can reach.
async fn serve_local(
    cfg: &TunnelConfig,
    handle: Arc<client::Handle<Handler>>,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    ready: impl FnOnce(),
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

    loop {
        let (mut socket, origin) = listener.accept().await.context("accept 失败")?;
        let handle = handle.clone();
        let (th, tp) = (cfg.remote_host.clone(), cfg.remote_port);
        let tx = tx.clone();
        tokio::spawn(async move {
            match handle
                .channel_open_direct_tcpip(th.clone(), u32::from(tp), origin.ip().to_string(), u32::from(origin.port()))
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

/// `-R`: ask the server to listen, serving inbound connections to our target.
async fn serve_remote(
    cfg: &TunnelConfig,
    handle: Arc<client::Handle<Handler>>,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    ready: impl FnOnce(),
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
    // local target); this future only has to keep the session alive.
    std::future::pending::<()>().await;
    Ok(())
}

/// `-D`: local SOCKS5 proxy over the session.
async fn serve_dynamic(
    cfg: &TunnelConfig,
    handle: Arc<client::Handle<Handler>>,
    id: TunnelId,
    tx: Sender<TunnelEvent>,
    ready: impl FnOnce(),
) -> Result<()> {
    let listener = TcpListener::bind((cfg.local_addr.as_str(), cfg.local_port))
        .await
        .with_context(|| format!("监听 {}:{} 失败", cfg.local_addr, cfg.local_port))?;
    let _ = tx
        .send(TunnelEvent::Line {
            id, line: format!("SOCKS5 代理已就绪：{}:{}", cfg.local_addr, cfg.local_port)
        });
    ready();

    loop {
        let (mut socket, _origin) = listener.accept().await.context("accept 失败")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn err_with(msg: &str) -> anyhow::Error {
        anyhow!("{msg}")
    }

    #[test]
    fn classifies_connection_refused() {
        let e = russh::Error::IO(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"));
        let info = classify(&anyhow::Error::new(e));
        assert_eq!(info.kind, FailureKind::ConnectionRefused);
        assert_eq!(info.short, "连接被拒绝");
    }

    #[test]
    fn classifies_addr_in_use() {
        let e = russh::Error::IO(std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use"));
        let info = classify(&anyhow::Error::new(e));
        assert_eq!(info.kind, FailureKind::PortInUse);
        assert_eq!(info.short, "端口已被占用");
    }

    #[test]
    fn classifies_permission_denied() {
        let e = russh::Error::IO(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"));
        assert_eq!(classify(&anyhow::Error::new(e)).kind, FailureKind::PermissionDenied);
    }

    #[test]
    fn classifies_timeout() {
        let e = russh::Error::IO(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"));
        assert_eq!(classify(&anyhow::Error::new(e)).kind, FailureKind::Timeout);
    }

    #[test]
    fn classifies_remote_forward_denied() {
        assert_eq!(classify(&anyhow::Error::new(russh::Error::RequestDenied)).kind, FailureKind::RemoteForwardFailed);
    }

    #[test]
    fn classifies_dead_target() {
        let e = russh::Error::ChannelOpenFailure(russh::ChannelOpenFailure::ConnectFailed);
        assert_eq!(classify(&anyhow::Error::new(e)).kind, FailureKind::TargetUnreachable);
    }

    #[test]
    fn classifies_dns_failure_from_text() {
        let info = classify(&err_with("failed to lookup address information: Name or service not known"));
        assert_eq!(info.kind, FailureKind::DnsFailure);
        assert_eq!(info.short, "域名解析失败");
    }

    #[test]
    fn classifies_auth_failure_from_text() {
        assert_eq!(classify(&err_with("没有可用的认证方式，也连接不上 ssh-agent")).kind, FailureKind::AuthFailed);
    }

    #[test]
    fn classifies_host_key_failure_from_text() {
        assert_eq!(classify(&err_with("example.com 的主机密钥已改变")).kind, FailureKind::HostKeyMismatch);
    }

    #[test]
    fn finds_wrapped_russh_error() {
        let e: anyhow::Error = anyhow::Error::new(russh::Error::RequestDenied).context("请求远端监听失败");
        assert_eq!(classify(&e).kind, FailureKind::RemoteForwardFailed);
    }

    #[test]
    fn unknown_short_reason_is_truncated() {
        let long = "x".repeat(200);
        let info = classify(&err_with(&long));
        assert!(crate::text::display_width(&info.short) <= 34, "short too wide: {:?}", info.short);
    }

    #[test]
    fn detail_keeps_the_full_error() {
        let info = classify(&err_with("some detailed reason here"));
        assert!(info.detail.contains("some detailed reason here"));
    }

    #[test]
    fn expand_path_handles_tilde_and_percent_d() {
        let expanded = expand_path("~/.ssh/id_ed25519");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.ends_with(".ssh/id_ed25519"));
        let expanded = expand_path("%d/.ssh/id_rsa");
        assert!(!expanded.starts_with('%'));
    }

    /// Regression: `load_auth` used to build a second runtime and `block_on` it
    /// from inside the tunnel's runtime, panicking with "Cannot start a runtime
    /// from within a runtime" on every hop without an `IdentityFile` — which is
    /// exactly a jump host that authenticates through the agent. Awaiting it here
    /// (inside a runtime, like `session` does) must return, not abort.
    #[tokio::test]
    async fn agent_auth_does_not_build_a_nested_runtime() {
        let endpoint = Endpoint {
            hostname: "127.0.0.1".to_string(),
            port: 22,
            user: None,
            proxy_jump: Vec::new(),
            identity_file: None,
        };
        match load_auth(&endpoint).await {
            Ok(_) => {} // this machine runs an agent
            Err(e) => assert!(
                e.to_string().contains("没有可用的认证方式"),
                "a missing agent must be a clean error, not a panic: {e}"
            ),
        }
    }
}
