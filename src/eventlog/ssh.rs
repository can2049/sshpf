//! SSH-configuration and host-key event vocabulary: `~/.ssh/config` discovery,
//! endpoint resolution, and `known_hosts` trust decisions.
//!
//! The host-key records are the security-relevant ones: `knownhosts.learned` is
//! the line that explains "why does this host now connect without asking?", and
//! `knownhosts.rejected` is the only trace a refused session leaves once the TUI
//! is gone. Keep their `event=` values and field order stable.
//!
//! A record about a *tunnel's* lifecycle belongs in [`super::tunnel`] instead.

use crate::logging::{self, Level};
use crate::model::{TunnelConfig, TunnelId};
use crate::ssh::known_hosts::HostKeyPolicy;

use super::tunnel::tunnel_event;

/// `~/.ssh/config` was read (or failed to be read).
pub fn hosts_loaded(count: usize) {
    logging::info("sshconfig.loaded", &[("hosts", &count)]);
}

pub fn hosts_failed(error: &str) {
    logging::warn("sshconfig.load_failed", &[("msg", &error)]);
}

/// Stable name for the host-key policy, so the log wording is not the enum's
/// debug formatting.
pub fn host_key_policy_name(policy: HostKeyPolicy) -> &'static str {
    match policy {
        HostKeyPolicy::Strict => "strict",
        HostKeyPolicy::AcceptNew => "accept-new",
        HostKeyPolicy::Ask => "ask",
        HostKeyPolicy::Ignore => "ignore",
    }
}

/// A configured host could not be resolved; `detail` is the full error chain.
pub fn host_resolve_failed(cfg: &TunnelConfig, detail: &str) {
    tunnel_event(Level::Error, "sshconfig.resolve_failed", cfg, &[("msg", detail.to_string())]);
}

/// A server key was learned into `known_hosts` under `accept-new`.
///
/// `warn`, not `info`: this is a new trust anchor being created, and it is the
/// line that explains "why does this host now connect without asking?".
pub fn host_key_learned(id: TunnelId, host: &str, port: u16, verdict: &str) {
    logging::warn("knownhosts.learned", &[("tunnel", &id), ("host", &host), ("port", &port), ("verdict", &verdict)]);
}

/// A server key was refused. `changed`/`revoked` are security events; an
/// unknown key under `strict` is a policy refusal. All are `error`.
pub fn host_key_rejected(id: TunnelId, host: &str, port: u16, policy: &str, detail: &str) {
    logging::error(
        "knownhosts.rejected",
        &[("tunnel", &id), ("host", &host), ("port", &port), ("policy", &policy), ("msg", &detail)],
    );
}

/// A *changed* server key was accepted because the host is under
/// [`HostKeyPolicy::Ignore`].
///
/// `warn`, not `info`: the connection proceeds, but a trust anchor the user
/// recorded on disk no longer matches what the server presented, and `known_hosts`
/// is deliberately **not** rewritten. This record is the only place that says so
/// once the TUI is gone.
pub fn host_key_ignored_changed(id: TunnelId, host: &str, port: u16, line: usize, fingerprint: &str) {
    logging::warn(
        "knownhosts.ignored_changed",
        &[("tunnel", &id), ("host", &host), ("port", &port), ("line", &line), ("fingerprint", &fingerprint)],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_names_are_stable() {
        assert_eq!(host_key_policy_name(crate::ssh::known_hosts::HostKeyPolicy::Strict), "strict");
        assert_eq!(host_key_policy_name(crate::ssh::known_hosts::HostKeyPolicy::AcceptNew), "accept-new");
        assert_eq!(host_key_policy_name(crate::ssh::known_hosts::HostKeyPolicy::Ask), "ask");
        assert_eq!(host_key_policy_name(crate::ssh::known_hosts::HostKeyPolicy::Ignore), "ignore");
    }
}
