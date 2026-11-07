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
//!    [`SshHost::from_params`] applies the fallback.

use std::io::BufReader;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ssh2_config::{ParseRule, SshConfig};

use crate::model::TunnelConfig;

/// Default SSH port.
const DEFAULT_PORT: u16 = 22;

/// A resolved host entry from `~/.ssh/config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    /// The `Host` alias the user typed / picked.
    pub alias: String,
    /// Resolved `HostName` (falls back to `alias`).
    pub hostname: String,
    pub user: Option<String>,
    pub port: u16,
    /// First jump host of `ProxyJump`, if any.
    pub proxy_jump: Option<String>,
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
            proxy_jump: p.proxy_jump.as_ref().and_then(|j| j.first().cloned()).filter(|j| j != "none"),
            identity_file: p.identity_file.as_ref().and_then(|v| v.first()).map(|p| p.display().to_string()),
        }
    }
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
        let starts_with_match = trimmed.len() >= 5
            && trimmed[..5].eq_ignore_ascii_case("match")
            && matches!(trimmed.as_bytes().get(5), Some(b' ') | Some(b'\t'));

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
    let paths = default_config_paths();
    if paths.is_empty() {
        return Ok(None);
    }
    // `ssh2-config` takes a single reader, but honours `Include` itself, so we
    // parse the first (main) file. An env override is a single file too.
    let path = &paths[0];
    let text = std::fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let cleaned = strip_match_blocks(&text);
    let mut reader = BufReader::new(cleaned.as_bytes());
    let cfg = SshConfig::default()
        .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS)
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
            proxy_jump: None,
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
    Some(SshHost { alias: s.to_string(), hostname: host, user, port, proxy_jump: None, identity_file: None })
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
}

impl Endpoint {
    /// Resolve the effective endpoint for a tunnel's `ssh_host`.
    pub fn resolve(ssh_host: &str) -> Result<Endpoint> {
        let host = resolve_host(ssh_host)?;
        let cfg = load_config()?;
        let mut proxy_jump = Vec::new();
        if let Some(cfg) = &cfg {
            // `query` already folds in aliases; walk the chain so a jump host
            // that itself declares ProxyJump is followed too.
            let mut current = host.proxy_jump.clone();
            let mut guard = 0;
            while let Some(jump) = current {
                if guard > 8 || proxy_jump.contains(&jump) {
                    break; // cycle / runaway guard
                }
                proxy_jump.push(jump.clone());
                current = cfg.query(&jump).proxy_jump.as_ref().and_then(|v| v.first().cloned()).filter(|j| j != "none");
                guard += 1;
            }
        }
        Ok(Endpoint {
            hostname: host.hostname,
            port: host.port,
            user: host.user,
            proxy_jump,
            identity_file: host.identity_file,
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

/// Resolve the endpoint for a tunnel definition.
pub fn endpoint_for(cfg: &TunnelConfig) -> Result<Endpoint> {
    Endpoint::resolve(&cfg.ssh_host)
}

/// Look up a single jump host's endpoint by alias (used by the tunnel layer).
pub fn jump_endpoint(alias: &str) -> Result<Endpoint> {
    Endpoint::resolve(alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_destination_parsing() {
        let h = parse_inline_destination("alice@example.com:2222").unwrap();
        assert_eq!(h.user.as_deref(), Some("alice"));
        assert_eq!(h.hostname, "example.com");
        assert_eq!(h.port, 2222);

        let h = parse_inline_destination("alice@example.com").unwrap();
        assert_eq!(h.user.as_deref(), Some("alice"));
        assert_eq!(h.port, 22);

        // Bare alias must NOT be treated as inline (it needs a config lookup).
        assert!(parse_inline_destination("myalias").is_none());
        // Bare host with default port is also a plain name.
        assert!(parse_inline_destination("example.com").is_none());
    }

    #[test]
    fn strip_match_blocks_removes_the_block_only() {
        let src = "\
Host web1
  HostName 10.0.0.1
  User alice
Match host foo
  User ignored
  Port 9999
Host web2
  HostName 10.0.0.2
";
        let out = strip_match_blocks(src);
        assert!(!out.contains("Match"), "Match line must be gone: {out}");
        assert!(!out.contains("ignored"), "Match body must be gone: {out}");
        assert!(!out.contains("9999"), "Match body must be gone: {out}");
        assert!(out.contains("HostName 10.0.0.1"));
        assert!(out.contains("Host web2"));
        assert!(out.contains("HostName 10.0.0.2"));
    }

    #[test]
    fn strip_match_blocks_handles_no_match() {
        let src = "Host a\n  HostName 1.2.3.4\n";
        assert_eq!(strip_match_blocks(src), src);
    }

    /// The bug this guard exists for: without stripping, `Match` leaks into the
    /// preceding Host block.
    #[test]
    fn match_block_would_otherwise_corrupt_the_previous_host() {
        let src = "\
Host web1
  HostName 10.0.0.1
  User alice
Match host foo
  User ignored
  Port 9999
";
        // Unstripped: leaked.
        let mut r = BufReader::new(src.as_bytes());
        let cfg = SshConfig::default().parse(&mut r, ParseRule::ALLOW_UNKNOWN_FIELDS).unwrap();
        let leaked = cfg.query("web1");
        assert_eq!(leaked.user.as_deref(), Some("ignored"), "documents the upstream bug");

        // Stripped: correct.
        let cleaned = strip_match_blocks(src);
        let mut r = BufReader::new(cleaned.as_bytes());
        let cfg = SshConfig::default().parse(&mut r, ParseRule::ALLOW_UNKNOWN_FIELDS).unwrap();
        let fixed = cfg.query("web1");
        assert_eq!(fixed.user.as_deref(), Some("alice"));
        assert_eq!(fixed.port, None);
    }

    #[test]
    fn hostname_falls_back_to_alias_and_port_defaults_to_22() {
        let src = "Host onlyport\n  Port 2200\n";
        let mut r = BufReader::new(src.as_bytes());
        let cfg = SshConfig::default().parse(&mut r, ParseRule::ALLOW_UNKNOWN_FIELDS).unwrap();
        let h = SshHost::from_params("onlyport", &cfg.query("onlyport"));
        assert_eq!(h.hostname, "onlyport", "OpenSSH uses the alias as hostname");
        assert_eq!(h.port, 2200);
    }

    #[test]
    fn host_star_defaults_are_inherited() {
        let src = "\
Host myserver
  HostName 203.0.113.10
Host *
  Port 2222
  User globaluser
";
        let mut r = BufReader::new(src.as_bytes());
        let cfg = SshConfig::default().parse(&mut r, ParseRule::ALLOW_UNKNOWN_FIELDS).unwrap();
        let h = SshHost::from_params("myserver", &cfg.query("myserver"));
        assert_eq!(h.hostname, "203.0.113.10");
        assert_eq!(h.user.as_deref(), Some("globaluser"));
        assert_eq!(h.port, 2222);
    }

    #[test]
    fn wildcard_hosts_are_not_listed_as_aliases() {
        let src = "\
Host concrete
  HostName 10.0.0.1
Host *.example.com
  User wild
Host *
  Port 22
";
        let mut r = BufReader::new(src.as_bytes());
        let cfg = SshConfig::default().parse(&mut r, ParseRule::ALLOW_UNKNOWN_FIELDS).unwrap();
        let mut aliases = Vec::new();
        for host in cfg.get_hosts() {
            for c in &host.pattern {
                if c.negated || c.pattern.contains('*') || c.pattern.contains('?') || c.pattern == "*" {
                    continue;
                }
                aliases.push(c.pattern.clone());
            }
        }
        assert_eq!(aliases, vec!["concrete".to_string()]);
    }

    #[test]
    fn display_and_endpoint() {
        let h = SshHost {
            alias: "web".into(),
            hostname: "10.0.0.1".into(),
            user: Some("alice".into()),
            port: 2222,
            proxy_jump: None,
            identity_file: None,
        };
        assert_eq!(h.display(), "web (alice@10.0.0.1:2222)");
        assert_eq!(h.endpoint(), "alice@10.0.0.1:2222");

        // Port 22 is omitted from display, user omitted when absent.
        let h = SshHost { user: None, port: 22, ..h };
        assert_eq!(h.display(), "web (10.0.0.1)");
    }
}
