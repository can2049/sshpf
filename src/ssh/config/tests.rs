//! Unit tests for `config.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

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
