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

use std::sync::Arc;

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
        Auth::Agent(mut agent) => authenticate_with_agent(handle, user, hash, &mut agent).await,
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
    agent: &mut AnyAgent,
) -> Result<()> {
    let ids: Vec<AgentIdentity> = agent.request_identities().await.map_err(|e| anyhow!("读取 ssh-agent 失败: {e}"))?;
    if ids.is_empty() {
        bail!("ssh-agent 中没有任何密钥");
    }
    for id in ids {
        let pk = id.public_key().into_owned();
        // `AgentClient` implements russh's `Signer`, so the real signing
        // happens inside `authenticate_publickey_with`.
        match handle.authenticate_publickey_with(user, pk, hash, &mut *agent).await {
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
