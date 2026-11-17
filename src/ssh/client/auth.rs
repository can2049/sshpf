//! Credential loading and authentication for one session.
//!
//! Two-thirds of this file is platform plumbing: there is no `SSH_AUTH_SOCK` on
//! Windows, so "connect to the agent" is three `cfg` variants rather than one
//! `connect_env()` call (which only exists in russh's `cfg(unix)` impl).
//!
//! Authentication order is the same as OpenSSH's: probe the `none` method first,
//! then an explicit `IdentityFile`, then the agent's identities. There is no
//! interactive password fallback by design (the old subprocess implementation ran
//! `ssh` with `BatchMode=yes`).
//!
//! # Reading identities may need a second connection
//!
//! `request_identities` looks like a pure read, but the agent may be a *proxy*:
//! GNOME Keyring 48 exposes `$SSH_AUTH_SOCK` itself and forwards to an OpenSSH
//! `ssh-agent` that it spawns on first use. Until that backend answers, the
//! proxy closes client connections, which reaches us as `ECONNRESET` and the
//! message "读取 ssh-agent 失败" — the failure that made two of three restored
//! tunnels fail while a hand-run `ssh` (one connection, seconds later) worked.
//! The connection is dead after such an error, so [`agent_with_identities`]
//! reconnects instead of re-reading, and hands the live connection back for
//! signing.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use russh::client;
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key};

use crate::ssh::config::Endpoint;

use super::handler::Handler;

/// Credentials for one connection.
pub(crate) enum Auth {
    Key(Arc<russh::keys::PrivateKey>),
    /// Boxed so Unix socket and Windows named-pipe transports share one type.
    Agent(AnyAgent),
}

/// Boxed agent client, so the Unix socket and the Windows transports share one
/// type.
type AnyAgent = AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>;

/// How often the identities read may be attempted, including the first try.
///
/// Three is enough for the case that motivated it — a backend process that needs
/// roughly a second before it serves — and still lets a permanently broken agent
/// reach the status bar within a few seconds.
const AGENT_ATTEMPTS: u32 = 3;

/// Wait before attempt 2 and 3: 250 ms, then 500 ms.
const AGENT_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Load credentials, preferring an explicit `IdentityFile`, then ssh-agent.
///
/// Async on purpose: [`super::connect::session`] already runs inside the tunnel's
/// runtime, so building a second runtime here would panic with "Cannot start a
/// runtime from within a runtime" on every hop that has no `IdentityFile` (e.g. a
/// jump host whose key comes from the agent).
pub(crate) async fn load_auth(endpoint: &Endpoint) -> Result<Auth> {
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
        Ok(agent) => Ok(agent.dynamic()),
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

/// List the agent's identities, reconnecting when the answer was a broken
/// connection rather than an agent decision.
async fn agent_with_identities(agent: AnyAgent) -> Result<(AnyAgent, Vec<AgentIdentity>)> {
    agent_with_identities_with(agent, connect_agent).await
}

/// [`agent_with_identities`] with the reconnect injected, so the retry can be
/// exercised without a real agent (see the tests at the bottom of this file).
///
/// Only a *transport* failure is retried: a refused key, an empty list or an
/// agent that answered "failure" is a decision, and asking again cannot change
/// it.
async fn agent_with_identities_with<F, Fut>(
    mut agent: AnyAgent,
    mut reconnect: F,
) -> Result<(AnyAgent, Vec<AgentIdentity>)>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<AnyAgent>>,
{
    let mut attempt = 1;
    loop {
        match agent.request_identities().await {
            Ok(identities) => return Ok((agent, identities)),
            Err(e) => {
                let err = anyhow::Error::new(e).context("读取 ssh-agent 失败");
                if attempt >= AGENT_ATTEMPTS || !is_transient_agent_error(&err) {
                    return Err(err);
                }
                // `attempt` is the attempt we are about to make: 2 waits
                // `AGENT_RETRY_DELAY`, 3 waits twice that.
                attempt += 1;
                tokio::time::sleep(AGENT_RETRY_DELAY * (attempt - 1)).await;
                agent = reconnect().await?;
            }
        }
    }
}

/// Whether an agent error is a dead connection rather than an agent verdict.
fn is_transient_agent_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(russh::keys::Error::IO(io)) = cause.downcast_ref::<russh::keys::Error>() {
            return is_transient_io(io);
        }
        cause.downcast_ref::<std::io::Error>().is_some_and(is_transient_io)
    })
}

fn is_transient_io(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::Interrupted
    )
}

/// Try to authenticate, in the same order OpenSSH uses.
///
/// The `none` method is probed first: it is how a server says "this user needs
/// no authentication", and some gateways (e.g. a desk-ssh jump host) accept it
/// while rejecting a public key offered afterwards. Only when `none` is refused
/// do we load the configured key / ssh-agent. Returns `Ok(())` only when the
/// server accepted.
pub(crate) async fn authenticate(handle: &mut client::Handle<Handler>, user: &str, endpoint: &Endpoint) -> Result<()> {
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
        Auth::Agent(agent) => authenticate_with_agent(handle, user, hash, agent).await,
    }
}

/// Offer every identity the agent holds, in order, until one is accepted.
///
/// Split out of [`authenticate`] so the key path and the agent path stay
/// readable side by side; both share the `ServerAuthStatus` handling.
async fn authenticate_with_agent(
    handle: &mut client::Handle<Handler>,
    user: &str,
    hash: Option<russh::keys::HashAlg>,
    agent: AnyAgent,
) -> Result<()> {
    // The identities and the connection they came from travel together: signing
    // must reuse that connection (a proxying agent may hand each connection a
    // different backend).
    let (mut agent, identities) = agent_with_identities(agent).await?;
    if identities.is_empty() {
        bail!("ssh-agent 中没有任何密钥");
    }
    for id in identities {
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

/// Name the hop in auth errors: with a ProxyJump chain the message would
/// otherwise not say *which* host rejected the key.
pub(crate) async fn authenticate_hop(handle: &mut client::Handle<Handler>, endpoint: &Endpoint) -> Result<()> {
    authenticate(handle, &endpoint.user_or_current(), endpoint)
        .await
        .with_context(|| format!("认证 {}:{} 失败", endpoint.hostname, endpoint.port))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context as TaskContext, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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
            host_key_policy: crate::ssh::known_hosts::HostKeyPolicy::AcceptNew,
        };
        match load_auth(&endpoint).await {
            Ok(_) => {} // this machine runs an agent
            Err(e) => assert!(
                e.to_string().contains("没有可用的认证方式"),
                "a missing agent must be a clean error, not a panic: {e}"
            ),
        }
    }

    /// Stands in for one agent connection: serves one canned reply, then every
    /// read fails the way a reset proxy connection does.
    struct FakeAgent {
        incoming: VecDeque<u8>,
        reset: bool,
    }

    impl FakeAgent {
        fn answering(frame: Vec<u8>) -> Self {
            Self { incoming: frame.into(), reset: false }
        }

        fn resetting() -> Self {
            Self { incoming: VecDeque::new(), reset: true }
        }
    }

    impl AsyncRead for FakeAgent {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.reset {
                return Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionReset)));
            }
            let n = self.incoming.len().min(buf.remaining());
            let head: Vec<u8> = self.incoming.drain(..n).collect();
            buf.put_slice(&head);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for FakeAgent {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Wire-encoded `ssh-ed25519` public key, copied from russh's own doc example.
    const ED25519_BLOB: [u8; 51] = [
        0x00, 0x00, 0x00, 0x0b, 0x73, 0x73, 0x68, 0x2d, 0x65, 0x64, 0x32, 0x35, 0x35, 0x31, 0x39, 0x00, 0x00, 0x00,
        0x20, 0x97, 0x43, 0xef, 0x2d, 0xda, 0x2e, 0xae, 0x39, 0xe3, 0x25, 0x81, 0x77, 0x02, 0xd6, 0x6e, 0x27, 0x94,
        0xd5, 0xe6, 0xf3, 0xf7, 0xf7, 0x2e, 0xef, 0xf4, 0x04, 0x5e, 0x7f, 0x4e, 0x21, 0xe6, 0x49,
    ];

    /// `SSH_AGENT_IDENTITIES_ANSWER` carrying [`ED25519_BLOB`] and a comment.
    fn identities_answer() -> Vec<u8> {
        let mut payload = vec![12u8]; // msg::IDENTITIES_ANSWER
        payload.extend_from_slice(&1u32.to_be_bytes());
        payload.extend_from_slice(&(ED25519_BLOB.len() as u32).to_be_bytes());
        payload.extend_from_slice(&ED25519_BLOB);
        payload.extend_from_slice(&4u32.to_be_bytes());
        payload.extend_from_slice(b"test");
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&payload);
        frame
    }

    #[test]
    fn only_transport_failures_are_worth_reconnecting() {
        let reset = anyhow::Error::new(russh::keys::Error::IO(io::Error::from(io::ErrorKind::ConnectionReset)));
        let eof = anyhow::Error::new(russh::keys::Error::IO(io::Error::from(io::ErrorKind::UnexpectedEof)));
        let denied = anyhow::Error::new(russh::keys::Error::IO(io::Error::from(io::ErrorKind::PermissionDenied)));
        let refused_key = anyhow::Error::new(russh::keys::Error::AgentFailure);
        let no_socket = anyhow::Error::new(russh::keys::Error::BadAuthSock);
        assert!(is_transient_agent_error(&reset));
        assert!(is_transient_agent_error(&eof));
        assert!(!is_transient_agent_error(&denied));
        assert!(!is_transient_agent_error(&refused_key));
        assert!(!is_transient_agent_error(&no_socket));
        assert!(!is_transient_agent_error(&anyhow::Error::msg("认证失败")));
    }

    /// The diagnosed failure: the first connection is answered with a reset
    /// (a proxy whose backend is still starting), the second one works. The
    /// identities must come from the second connection, and that same
    /// connection must be the one handed back for signing.
    #[tokio::test]
    async fn a_reset_identities_read_is_retried_on_a_fresh_connection() {
        let reconnects = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reconnects);
        let first = AgentClient::connect(FakeAgent::resetting()).dynamic();
        let reconnect = move || {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(AgentClient::connect(FakeAgent::answering(identities_answer())).dynamic())
            }
        };

        let (_agent, identities) =
            agent_with_identities_with(first, reconnect).await.expect("a reset must be retried, not reported");

        assert_eq!(reconnects.load(Ordering::SeqCst), 1, "exactly one reconnect");
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].public_key().algorithm().as_str(), "ssh-ed25519");
    }

    /// The retry budget is finite and the final error still names the agent, so
    /// the status bar and the log keep saying "读取 ssh-agent 失败" instead of a
    /// bare "认证失败".
    #[tokio::test]
    async fn a_dead_agent_is_reported_after_the_last_attempt() {
        let reconnects = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reconnects);
        let first = AgentClient::connect(FakeAgent::resetting()).dynamic();
        let reconnect = move || {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(AgentClient::connect(FakeAgent::resetting()).dynamic())
            }
        };

        let err = match agent_with_identities_with(first, reconnect).await {
            Ok(_) => panic!("a dead agent must fail"),
            Err(e) => e,
        };

        assert_eq!(reconnects.load(Ordering::SeqCst), (AGENT_ATTEMPTS - 1) as usize);
        assert!(format!("{err:#}").starts_with("读取 ssh-agent 失败"), "{err:#}");
    }
}
