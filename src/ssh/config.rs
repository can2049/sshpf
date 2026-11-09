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
mod tests {
    use super::*;

    /// Parse a config snippet the way `load_config` does, without touching the
    /// real `~/.ssh/config` (tests must not depend on the machine they run on).
    fn parsed(src: &str) -> SshConfig {
        let mut reader = BufReader::new(src.as_bytes());
        SshConfig::default().parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS).unwrap()
    }

    /// The chain `Endpoint::resolve` would dial for `alias`.
    fn chain_for(cfg: &SshConfig, alias: &str) -> Vec<String> {
        let host = SshHost::from_params(alias, &cfg.query(alias));
        proxy_chain(cfg, &host.proxy_jump)
    }

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

    /// Regression: byte 5 landing inside a multi-byte character used to panic
    /// (`end byte index 5 is not a char boundary`), so an ssh config whose first
    /// line is a Chinese comment crashed `--list-hosts` at start-up.
    #[test]
    fn strip_match_blocks_survives_a_multibyte_first_line() {
        let src = "中文注释\nHost x\n  HostName 1.2.3.4\n";
        let out = strip_match_blocks(src);
        assert!(out.contains("Host x"), "the Host block must be preserved: {out:?}");
        assert!(out.contains("HostName 1.2.3.4"), "{out:?}");
        assert!(out.contains("中文注释"), "an unrelated comment must be kept verbatim: {out:?}");
    }

    /// Every multi-byte line shape that exercises a *different* boundary relative
    /// to the five-byte keyword window, including 1..=4-byte characters whose
    /// boundary lands exactly on 5.
    #[test]
    fn strip_match_blocks_handles_every_multibyte_boundary_around_the_keyword() {
        for line in ["中文字", "日本語のコメント", "🚀", "é", "a中b", "中文", "备注：服务器列表", "\t中文缩进"]
        {
            let src = format!("{line}\nHost x\n");
            // The assertion that matters is that this cannot panic.
            let out = strip_match_blocks(&src);
            assert!(out.contains("Host x"), "{line:?} broke the scan: {out:?}");
        }
    }

    /// A `Match` line preceded by a multi-byte comment is still stripped — the
    /// bug fix must not turn into "stop recognising Match after non-ASCII text".
    #[test]
    fn strip_match_blocks_still_recognises_match_after_multibyte_text() {
        let src = "中文注释\nHost web1\n  HostName 10.0.0.1\nMatch host foo\n  User ignored\nHost web2\n";
        let out = strip_match_blocks(src);
        assert!(!out.contains("Match"), "{out:?}");
        assert!(!out.contains("ignored"), "{out:?}");
        assert!(out.contains("Host web1") && out.contains("Host web2"), "{out:?}");
    }

    /// A `Match` keyword must not be recognised when the 6th byte is a *multi-byte*
    /// continuation rather than a space/tab — e.g. a host alias named `match中`.
    #[test]
    fn a_word_merely_starting_with_match_is_not_a_match_block() {
        let src = "Host match中文\n  HostName 10.0.0.9\n";
        let out = strip_match_blocks(src);
        assert!(out.contains("Host match中文"), "a Host whose name starts with `match` must survive: {out:?}");
        assert!(out.contains("HostName 10.0.0.9"), "{out:?}");
    }

    /// The keyword comparison has to stay case-insensitive and whitespace-strict.
    #[test]
    fn match_keyword_casing_and_separator_rules_hold() {
        for src in ["MATCH host x\n  User u\nHost web\n", "Match\thost x\n  User u\nHost web\n"] {
            let out = strip_match_blocks(src);
            assert!(!out.contains("User u"), "{src:?} was not stripped: {out:?}");
            assert!(out.contains("Host web"), "{out:?}");
        }
        // `MatchFoo` is a different (unknown) keyword and must be passed through.
        let out = strip_match_blocks("MatchFoo host x\nHost web\n");
        assert!(out.contains("MatchFoo"), "{out:?}");
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

    /// Regression: only the first hop of a comma-separated `ProxyJump` used to
    /// survive. The user's real config is `ProxyJump office_pc,ali_cloud_pc_jump`
    /// and the missing second hop made every attempt fail with
    /// "Failed to open channel (ConnectFailed)": office_pc has no route to the
    /// cloud host, the jump host it was supposed to hand over to does.
    #[test]
    fn proxy_jump_keeps_every_hop() {
        let cfg = parsed(
            "\
Host cloud_pc
  HostName 10.244.119.128
  ProxyJump office_pc,ali_cloud_pc_jump
Host office_pc
  HostName 10.24.96.190
Host ali_cloud_pc_jump
  HostName desk-ssh.srv.deeproute.cn
  Port 80
",
        );
        let host = SshHost::from_params("cloud_pc", &cfg.query("cloud_pc"));
        assert_eq!(host.proxy_jump.join(","), "office_pc,ali_cloud_pc_jump");
        // ... and the dial order is OpenSSH's: office_pc first, then the jump host.
        assert_eq!(chain_for(&cfg, "cloud_pc").join(","), "office_pc,ali_cloud_pc_jump");
    }

    #[test]
    fn proxy_jump_none_means_direct() {
        let cfg = parsed("Host direct\n  HostName 10.0.0.5\n  ProxyJump none\n");
        let host = SshHost::from_params("direct", &cfg.query("direct"));
        assert!(host.proxy_jump.is_empty(), "`none` must not become a hop");
        assert!(chain_for(&cfg, "direct").is_empty());
    }

    /// OpenSSH prepends the first hop's own chain: with `A -> B` and `B -> C`,
    /// reaching `A` dials `C`, then `B`, then `A`. The pre-fix code appended it
    /// instead (`B`, then `C`), which is the wrong order.
    #[test]
    fn nested_proxy_jump_of_first_hop_is_prepended() {
        let cfg = parsed(
            "\
Host A
  HostName 10.244.119.128
  ProxyJump B
Host B
  HostName desk-ssh.srv.deeproute.cn
  Port 80
  ProxyJump office_pc
Host office_pc
  HostName 10.24.96.190
",
        );
        assert_eq!(chain_for(&cfg, "A").join(","), "office_pc,B");
    }

    /// Hops after the first are reached over a channel opened by the previous
    /// hop, so `-J` overrides their own `ProxyJump`; it must not be expanded.
    #[test]
    fn later_hops_do_not_contribute_their_own_proxy_jump() {
        let cfg = parsed(
            "\
Host A
  HostName 10.244.119.128
  ProxyJump B,C
Host B
  HostName b.example
Host C
  HostName c.example
  ProxyJump office_pc
Host office_pc
  HostName 10.24.96.190
",
        );
        assert_eq!(chain_for(&cfg, "A").join(","), "B,C");
    }

    /// A `ProxyJump` loop must stop expanding instead of hanging the UI thread.
    #[test]
    fn proxy_jump_cycle_is_bounded() {
        let cfg = parsed(
            "\
Host A
  HostName 10.0.0.1
  ProxyJump B
Host B
  HostName 10.0.0.2
  ProxyJump A
",
        );
        let chain = chain_for(&cfg, "A");
        assert!(chain.len() <= 2, "a cycle must not grow the chain: {chain:?}");
        let mut unique = chain.clone();
        unique.dedup();
        assert_eq!(unique, chain, "no hop may appear twice: {chain:?}");
    }

    /// `Host *` defaults can carry the whole list too.
    #[test]
    fn proxy_jump_list_is_inherited_from_star() {
        let cfg = parsed(
            "\
Host target
  HostName 10.0.0.9
Host *
  ProxyJump bastion,gateway
Host bastion
  HostName 10.0.0.1
Host gateway
  HostName 10.0.0.2
",
        );
        assert_eq!(chain_for(&cfg, "target").join(","), "bastion,gateway");
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
            proxy_jump: Vec::new(),
            identity_file: None,
        };
        assert_eq!(h.display(), "web (alice@10.0.0.1:2222)");
        assert_eq!(h.endpoint(), "alice@10.0.0.1:2222");

        // Port 22 is omitted from display, user omitted when absent.
        let h = SshHost { user: None, port: 22, ..h };
        assert_eq!(h.display(), "web (10.0.0.1)");
    }
}
