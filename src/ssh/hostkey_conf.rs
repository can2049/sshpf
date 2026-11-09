//! `~/.ssh/config`'s host-key directives → a [`HostKeyPolicy`] per host.
//!
//! # Why this exists
//!
//! `ssh cloud_pc` works while sshpf cannot connect to the same alias, and the
//! reason is usually *not* the target host: it is the jump host. A very common
//! config shape is
//!
//! ```text
//! Host cloud_pc_jump
//!   StrictHostKeyChecking no
//!   UserKnownHostsFile /dev/null
//! ```
//!
//! OpenSSH then never verifies that hop, while sshpf (which owns its own
//! `known_hosts` checker) refused it — a correct refusal, but not the behaviour
//! the user configured. This module closes that gap by reading the same two
//! directives and mapping them onto [`HostKeyPolicy`], so a host the user
//! already told ssh to stop verifying stops being a hard failure.
//!
//! # The mapping
//!
//! | `StrictHostKeyChecking` | `UserKnownHostsFile` | policy |
//! |---|---|---|
//! | `no` / `off` | anything | [`HostKeyPolicy::Ignore`] |
//! | `accept-new` | anything | [`HostKeyPolicy::AcceptNew`] |
//! | `yes` / `ask` / absent | anything | [`HostKeyPolicy::AcceptNew`] (sshpf's default) |
//!
//! Note what is deliberately **not** mapped: `yes` does *not* become
//! [`HostKeyPolicy::Strict`]. sshpf's default is `accept-new` (learn an unknown
//! host, refuse a changed key), and reading a config must not make sshpf stricter
//! than it is without a config at all — that would break every existing user who
//! never wrote the directive. `UserKnownHostsFile /dev/null` on its own is
//! treated as "this host does not really have a stable trust anchor", which is
//! the same intent as `StrictHostKeyChecking no`, so it also yields `Ignore`.
//!
//! # Scope rules
//!
//! These come free from `ssh2-config`: the parser keeps the directives per
//! `Host` block and `query()` merges `Host *` etc. with first-obtained-wins,
//! exactly like OpenSSH. Measured: a host with no directive of its own inherits
//! `Host *`'s value, and a `Host` block's own value beats `Host *`.
//!
//! The directives are *unsupported* fields in `ssh2-config`'s view, so they are
//! only retained when parsing with `ParseRule::ALLOW_UNSUPPORTED_FIELDS`; the
//! values then sit in `HostParams::unsupported_fields`.

use ssh2_config::HostParams;

use super::known_hosts::HostKeyPolicy;

/// The config directive that disables host-key checking for a host.
const STRICT: &str = "stricthostkeychecking";
/// The config directive naming the known-hosts file; `/dev/null` means "none".
const KNOWN_HOSTS_FILE: &str = "userknownhostsfile";
/// The value meaning "do not persist / do not check".
const DEV_NULL: &str = "/dev/null";

/// The host-key policy implied by one host's parsed config parameters.
///
/// Callers pass the *already merged* params for the alias they resolved
/// (`SshConfig::query(alias)`), so `Host *` inheritance has happened.
pub fn policy_for_params(params: &HostParams) -> HostKeyPolicy {
    // `ssh2-config` stores unsupported directives as `name -> arg list`; take the
    // first argument, which is the value (`StrictHostKeyChecking no` → "no").
    let strict = first_arg(params, STRICT).map(|v| v.trim().to_ascii_lowercase());
    let known_hosts = first_arg(params, KNOWN_HOSTS_FILE).map(|v| v.trim().to_string());

    match strict.as_deref() {
        // Explicitly off: the user asked ssh not to verify this host at all.
        Some("no") | Some("off") => HostKeyPolicy::Ignore,
        Some("accept-new") => HostKeyPolicy::AcceptNew,
        // `yes`/`ask`/absent all stay at sshpf's default — see the module docs.
        _ if known_hosts.as_deref() == Some(DEV_NULL) => HostKeyPolicy::Ignore,
        _ => HostKeyPolicy::AcceptNew,
    }
}

/// First argument of a directive, or `None` when the directive is absent/empty.
fn first_arg<'a>(params: &'a HostParams, directive: &str) -> Option<&'a str> {
    params.unsupported_fields.get(directive)?.first().map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh2_config::{ParseRule, SshConfig};
    use std::io::BufReader;

    /// Parse a snippet the way `ssh/config.rs` does, then query one alias.
    fn policy_of(src: &str, alias: &str) -> HostKeyPolicy {
        let mut reader = BufReader::new(src.as_bytes());
        let cfg = SshConfig::default()
            .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS)
            .unwrap();
        policy_for_params(&cfg.query(alias))
    }

    #[test]
    fn no_means_ignore() {
        let src = "Host jump\n  HostName j.example.com\n  StrictHostKeyChecking no\n";
        assert_eq!(policy_of(src, "jump"), HostKeyPolicy::Ignore);
    }

    #[test]
    fn off_is_accepted_as_a_synonym_for_no() {
        // OpenSSH accepts `off`; `yes`/`no` and `on`/`off` are the same switch.
        let src = "Host h\n  StrictHostKeyChecking off\n";
        assert_eq!(policy_of(src, "h"), HostKeyPolicy::Ignore);
    }

    #[test]
    fn accept_new_is_accept_new() {
        let src = "Host h\n  StrictHostKeyChecking accept-new\n";
        assert_eq!(policy_of(src, "h"), HostKeyPolicy::AcceptNew);
    }

    /// The reference case: the jump host says `no`, the target host does not, and
    /// each hop must get its own policy.
    #[test]
    fn a_jump_host_and_its_target_get_separate_policies() {
        let src = "\
Host cloud_pc_jump
  HostName desk-ssh.example.com
  Port 80
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
Host cloud_pc
  HostName 10.244.119.128
  ProxyJump cloud_pc_jump
";
        assert_eq!(policy_of(src, "cloud_pc_jump"), HostKeyPolicy::Ignore);
        // The target declares nothing, so it keeps sshpf's default even though a
        // `Host *`-style inheritance could otherwise leak the jump host's setting.
        assert_eq!(policy_of(src, "cloud_pc"), HostKeyPolicy::AcceptNew);
    }

    #[test]
    fn dev_null_alone_means_ignore() {
        // The intent of `UserKnownHostsFile /dev/null` is "no trust anchor at
        // all", which is the same intent as disabling the check.
        let src = "Host h\n  UserKnownHostsFile /dev/null\n";
        assert_eq!(policy_of(src, "h"), HostKeyPolicy::Ignore);
    }

    #[test]
    fn a_real_known_hosts_path_is_not_ignore() {
        let src = "Host h\n  UserKnownHostsFile ~/.ssh/known_hosts2\n";
        assert_eq!(policy_of(src, "h"), HostKeyPolicy::AcceptNew);
    }

    #[test]
    fn host_star_is_inherited() {
        let src = "Host *\n  StrictHostKeyChecking no\nHost specific\n  HostName s.example.com\n";
        assert_eq!(policy_of(src, "specific"), HostKeyPolicy::Ignore);
    }

    #[test]
    fn a_host_block_beats_host_star() {
        // OpenSSH's first-obtained-wins: `Host specific` is read first and its
        // explicit `accept-new` must survive the later `Host *`.
        let src = "\
Host specific
  HostName s.example.com
  StrictHostKeyChecking accept-new
Host *
  StrictHostKeyChecking no
";
        assert_eq!(policy_of(src, "specific"), HostKeyPolicy::AcceptNew);
    }

    /// `yes` must NOT turn into `Strict`: sshpf's default is `accept-new`, and a
    /// config that merely restates the OpenSSH default must not tighten it.
    #[test]
    fn explicit_yes_does_not_become_strict() {
        let src = "Host h\n  StrictHostKeyChecking yes\n";
        assert_eq!(policy_of(src, "h"), HostKeyPolicy::AcceptNew);
    }

    #[test]
    fn absent_directives_keep_the_default() {
        let src = "Host h\n  HostName h.example.com\n";
        assert_eq!(policy_of(src, "h"), HostKeyPolicy::AcceptNew);
    }

    #[test]
    fn directive_value_matching_is_case_insensitive() {
        let src = "Host h\n  StrictHostKeyChecking NO\n";
        assert_eq!(policy_of(src, "h"), HostKeyPolicy::Ignore);
    }
}
