//! `~/.ssh/config` discovery — pure Rust, no `ssh -G` subprocess.
//!
//! Replaces the previous `config_parse` + `ssh -G -T` two-step: `ssh2-config`
//! parses the file (including `Include`, `Host` wildcards, `Host *` defaults,
//! negation and `ProxyJump`), and we evaluate a host by querying it.
//!
//! # Why `ssh2-config`
//!
//! Measured (see `docs/research/pure-rust-ssh-config.md`):
//! * pure Rust — `libssh2-sys` / `openssl-sys` / `cc` all absent from its tree,
//!   despite the crate name; `ssh2` is only a dev-dependency for its examples.
//! * handles `Include`, multi-pattern `Host`, negation, `Match host`, and
//!   inherits `Host *` defaults into every query result.
//!
//! # Two gaps we must close ourselves
//!
//! 1. **`Match` blocks are not understood and silently corrupt the preceding
//!    `Host` block.** The parser has no `Match` variant, so the line falls into
//!    `UnknownField`; with `ALLOW_UNKNOWN_FIELDS` it is *ignored*, but the
//!    options that follow it keep being appended to the previous `Host`.
//!    Reproduced:
//!
//!    ```text
//!    Host web1  { HostName 10.0.0.1; User alice }
//!    Match host foo { User ignored; Port 9999 }
//!
//!    ssh2-config query(web1) -> user="ignored" port=9999   ← wrong
//!    ssh -G -F ...        web1 -> user=alice    port=22     ← correct
//!    ```
//!
//!    [`strip_match_blocks`] removes them before parsing, and is unit-tested.
//! 2. **`HostName` does not fall back to the alias.** OpenSSH uses the alias as
//!    the hostname when a block only sets `Port`; `ssh2-config` returns `None`.
//!    `SshHost::from_params` applies the fallback.

use std::io::BufReader;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ssh2_config::{ParseRule, SshConfig};

use crate::model::TunnelConfig;

/// Default SSH port.
const DEFAULT_PORT: u16 = 22;

/// The `Match` keyword, as bytes: matched with `eq_ignore_ascii_case` against a
/// byte slice, which cannot panic on a char boundary the way `trimmed[..5]` can.
const MATCH_KEYWORD: &[u8] = b"match";

/// A resolved host entry from `~/.ssh/config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    /// The `Host` alias the user typed / picked.
    pub alias: String,
    /// Resolved `HostName` (falls back to `alias`).
    pub hostname: String,
    pub user: Option<String>,
    pub port: u16,
    /// `ProxyJump` hops exactly as written, first hop first.
    ///
    /// Empty when the host declares no `ProxyJump` (or sets it to `none`).
    /// Keeping the whole list matters: OpenSSH treats `ProxyJump a,b` as the
    /// chain `a → b`, so dropping everything after `a` silently removes hops.
    pub proxy_jump: Vec<String>,
    /// First `IdentityFile` (token-expanded only for `~` / `%d`).
    pub identity_file: Option<String>,
}

impl SshHost {
    /// `alias (user@hostname:port)`, omitting the user when absent and the port
    /// when it is the default 22.
    pub fn display(&self) -> String {
        let mut who = String::new();
        if let Some(user) = &self.user {
            who.push_str(user);
            who.push('@');
        }
        who.push_str(&self.hostname);
        if self.port != DEFAULT_PORT {
            who.push(':');
            who.push_str(&self.port.to_string());
        }
        format!("{} ({})", self.alias, who)
    }

    /// `user@hostname:port` — for the detail panel / copy-paste.
    pub fn endpoint(&self) -> String {
        let mut s = String::new();
        if let Some(u) = &self.user {
            s.push_str(u);
            s.push('@');
        }
        s.push_str(&self.hostname);
        s.push(':');
        s.push_str(&self.port.to_string());
        s
    }

    /// Build from a parsed `HostParams`, applying the alias fallback.
    fn from_params(alias: &str, p: &ssh2_config::HostParams) -> Self {
        SshHost {
            alias: alias.to_string(),
            // OpenSSH falls back to the alias when HostName is unset.
            hostname: p.host_name.clone().unwrap_or_else(|| alias.to_string()),
            user: p.user.clone(),
            port: p.port.unwrap_or(DEFAULT_PORT),
            proxy_jump: proxy_jump_list(p),
            identity_file: p.identity_file.as_ref().and_then(|v| v.first()).map(|p| p.display().to_string()),
        }
    }
}

/// The whole `ProxyJump` list of a host, with the `none` sentinel removed.
fn proxy_jump_list(p: &ssh2_config::HostParams) -> Vec<String> {
    p.proxy_jump.clone().unwrap_or_default().into_iter().filter(|j| j != "none").collect()
}

/// Configuration file locations to read.
///
/// `SSHPF_SSH_CONFIG` overrides everything (tests and experimentation); when
/// it is set we parse *only* that file.
pub fn default_config_paths() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("SSHPF_SSH_CONFIG") {
        let p = PathBuf::from(p);
        return if p.is_file() { vec![p] } else { Vec::new() };
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let main = home.join(".ssh").join("config");
    if main.is_file() { vec![main] } else { Vec::new() }
}

/// Remove `Match` blocks from an ssh config so `ssh2-config` cannot let their
/// options leak into the preceding `Host` block.
///
/// A `Match` line and every following *indented* line are dropped; a
/// non-indented line ends the block. Comments and blanks inside the block are
/// dropped too (harmless). `Match` inside an `Include`d file is a pre-existing
/// limitation — we only pre-process the top-level file, which is where users
/// actually put `Match`; `ssh2-config` resolves `Include`s itself.
pub fn strip_match_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_match = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        // Compare **bytes**, never a `str` slice: `trimmed[..5]` panics when byte
        // 5 lands inside a multi-byte character, and a config whose first line is
        // a Chinese comment (「中文注释」 — '文' spans bytes 3..6) is completely
        // ordinary. Byte comparison is also exactly what OpenSSH's keyword match
        // is (case-insensitive ASCII).
        let starts_with_match =
            trimmed.as_bytes().get(..MATCH_KEYWORD.len()).is_some_and(|word| word.eq_ignore_ascii_case(MATCH_KEYWORD))
                && matches!(trimmed.as_bytes().get(MATCH_KEYWORD.len()), Some(b' ') | Some(b'\t'));

        if starts_with_match {
            in_match = true;
            continue;
        }
        if in_match {
            if trimmed.is_empty() || trimmed.starts_with('#') || is_indented {
                continue;
            }
            // A new top-level directive ends the Match block.
            in_match = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Read and parse every config file. Returns `Ok(None)` when there is nothing
/// to read (no config file, or an unreadable one).
fn load_config() -> Result<Option<SshConfig>> {
    // `ssh2-config` takes a single reader, but honours `Include` itself, so we
    // parse the first (main) file. An env override is a single file too.
    //
    // `first()` rather than `paths[0]`: the emptiness check above makes the index
    // safe today, but this is a parser entry point and the index would be a panic
    // if that check ever moved.
    let Some(path) = default_config_paths().first().cloned() else {
        return Ok(None);
    };
    let path = &path;
    let text = std::fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let cleaned = strip_match_blocks(&text);
    let mut reader = BufReader::new(cleaned.as_bytes());
    let cfg = SshConfig::default()
        // `ALLOW_UNSUPPORTED_FIELDS` is what keeps `StrictHostKeyChecking` and
        // `UserKnownHostsFile` available for [`super::hostkey_conf`]: without it
        // they are dropped by the parser and a host the user told ssh to stop
        // verifying would still be a hard failure here.
        .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS)
        .with_context(|| format!("解析 {} 失败", path.display()))?;
    Ok(Some(cfg))
}

/// Enumerate the concrete (non-wildcard, non-negated) aliases from the config.
pub fn list_hosts() -> Result<Vec<SshHost>> {
    let Some(cfg) = load_config()? else {
        return Ok(Vec::new());
    };
    let mut aliases: Vec<String> = Vec::new();
    for host in cfg.get_hosts() {
        for clause in &host.pattern {
            let p = &clause.pattern;
            if clause.negated || p.contains('*') || p.contains('?') || p == "*" {
                continue;
            }
            if !aliases.contains(p) {
                aliases.push(p.clone());
            }
        }
    }
    aliases.sort();
    let hosts = aliases.iter().map(|a| SshHost::from_params(a, &cfg.query(a))).collect();
    Ok(hosts)
}

/// Resolve one host.
///
/// A destination shaped `user@host:port` (or `host:port`) never appears in the
/// config as a block, so parse it directly instead of querying.
pub fn resolve_host(alias: &str) -> Result<SshHost> {
    if let Some(h) = parse_inline_destination(alias) {
        return Ok(h);
    }
    let Some(cfg) = load_config()? else {
        // No config file: treat the argument as a plain hostname.
        return Ok(SshHost {
            alias: alias.to_string(),
            hostname: alias.to_string(),
            user: None,
            port: DEFAULT_PORT,
            proxy_jump: Vec::new(),
            identity_file: None,
        });
    };
    Ok(SshHost::from_params(alias, &cfg.query(alias)))
}

/// Parse `[user@]host[:port]` written directly by the user.
///
/// Deliberately conservative: returns `None` unless there is a `@` or a
/// trailing `:port`, so a bare alias always goes through config resolution.
fn parse_inline_destination(s: &str) -> Option<SshHost> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (user, rest) = match s.rsplit_once('@') {
        Some((u, r)) if !u.is_empty() && !r.is_empty() => (Some(u.to_string()), r),
        Some(_) => return None,
        None => (None, s),
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => match p.parse::<u16>() {
            Ok(port) if port > 0 => (h.to_string(), port),
            _ => return None,
        },
        Some(_) => return None,
        None => (rest.to_string(), DEFAULT_PORT),
    };
    // A bare alias with no user and no explicit port is not an inline
    // destination — it must be looked up in the config.
    if user.is_none() && port == DEFAULT_PORT {
        return None;
    }
    Some(SshHost { alias: s.to_string(), hostname: host, user, port, proxy_jump: Vec::new(), identity_file: None })
}

/// Everything the tunnel layer needs to reach a host, resolved from config.
///
/// Kept separate from [`SshHost`] because the tunnel needs the *effective*
/// values (hostname/user/port after `Host *` inheritance), not the display
/// fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub hostname: String,
    pub port: u16,
    pub user: Option<String>,
    /// Full ProxyJump chain, first hop first.
    pub proxy_jump: Vec<String>,
    pub identity_file: Option<String>,
    /// Host-key policy this host's own `~/.ssh/config` block implies.
    ///
    /// Per endpoint, not per tunnel: a `ProxyJump` chain re-verifies every hop,
    /// and it is normal for the *jump* host to be the one the user stopped
    /// verifying while the target stays strict. See [`super::hostkey_conf`].
    pub host_key_policy: super::known_hosts::HostKeyPolicy,
}

impl Endpoint {
    /// Resolve the effective endpoint for a tunnel's `ssh_host`.
    pub fn resolve(ssh_host: &str) -> Result<Endpoint> {
        let host = resolve_host(ssh_host)?;
        let cfg = load_config()?;
        let proxy_jump = match &cfg {
            Some(cfg) => proxy_chain(cfg, &host.proxy_jump),
            None => Vec::new(),
        };
        // With no config there are no directives to honour, so the default holds.
        let host_key_policy = match &cfg {
            Some(cfg) => super::hostkey_conf::policy_for_params(&cfg.query(ssh_host)),
            None => super::known_hosts::HostKeyPolicy::default(),
        };
        Ok(Endpoint {
            hostname: host.hostname,
            port: host.port,
            user: host.user,
            proxy_jump,
            identity_file: host.identity_file,
            host_key_policy,
        })
    }

    /// Resolve the user for a host, falling back to the current OS user.
    pub fn user_or_current(&self) -> String {
        self.user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .or_else(|| std::env::var("USERNAME").ok())
            .unwrap_or_else(|| "root".to_string())
    }
}

/// The hops to dial for `jumps` (a host's `ProxyJump` list), first hop first.
fn proxy_chain(cfg: &SshConfig, jumps: &[String]) -> Vec<String> {
    let mut chain = Vec::new();
    let mut seen = Vec::new();
    expand_proxy_jump(cfg, jumps, &mut seen, &mut chain);
    chain
}

/// Expand one `ProxyJump` list into the hops to dial, first hop first.
///
/// `ProxyJump a,b` is OpenSSH's explicit chain `a → b`: `b` is reached over a
/// channel opened by `a`. The list therefore has to stay whole — keeping only
/// `a` silently drops every hop after the first.
///
/// The *first* hop is reached by a plain `ssh`, which still honours that hop's
/// own `ProxyJump`, so its chain is expanded and prepended recursively (measured
/// against OpenSSH: `Host A ProxyJump B` plus `Host B ProxyJump C` produces the
/// chain `C → B`). Hops after the first are reached through `-J`, which
/// overrides their own `ProxyJump`, so they are appended as written.
///
/// A repeated alias stops the expansion instead of looping forever.
fn expand_proxy_jump(cfg: &SshConfig, jumps: &[String], seen: &mut Vec<String>, chain: &mut Vec<String>) {
    let Some((first, rest)) = jumps.split_first() else {
        return;
    };
    if seen.contains(first) {
        return; // cycle (a → b → a) or a hop already on the chain
    }
    seen.push(first.clone());
    let nested = proxy_jump_list(&cfg.query(first));
    expand_proxy_jump(cfg, &nested, seen, chain);
    chain.push(first.clone());
    for hop in rest {
        if seen.contains(hop) {
            continue;
        }
        seen.push(hop.clone());
        chain.push(hop.clone());
    }
}

/// Resolve the endpoint for a tunnel definition.
pub fn endpoint_for(cfg: &TunnelConfig) -> Result<Endpoint> {
    Endpoint::resolve(&cfg.ssh_host)
}

/// Look up a single jump host's endpoint by alias (used by the tunnel layer).
pub fn jump_endpoint(alias: &str) -> Result<Endpoint> {
    Endpoint::resolve(alias)
}

#[cfg(test)]
mod tests;
