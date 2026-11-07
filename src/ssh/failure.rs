//! Turning raw `ssh` stderr into structured failures and readiness markers.
//!
//! Every rule here was derived from measured OpenSSH output (see
//! `docs/research/tech-options.md` §2.3), including the benign lines that must
//! *not* be reported as failures.

use crate::model::{FailureInfo, FailureKind, ForwardKind};
use crate::ssh::SHORT_MAX_WIDTH;
use crate::text::{char_width, display_width};

/// Classify one line of `ssh` stderr into a UI-ready failure.
///
/// Order matters: a partially bound `-L` prints the `bind` failure *and*
/// `Entering interactive session.`, and the failure is the interesting part
/// (§2.3). Callers should classify before consulting [`is_ready_line`].
///
/// Each rule is a small function rather than one long `if` chain: the measured
/// OpenSSH wording is scattered across versions, and keeping the "this specific
/// phrase means this specific cause" knowledge in one place per cause makes it
/// reviewable and testable on its own.
pub fn classify_failure(line: &str) -> Option<FailureInfo> {
    let l = line.trim();
    if l.is_empty() {
        return None;
    }
    let lower = l.to_ascii_lowercase();

    // Ordered most-specific first. `bind` bullets come before the generic
    // `cannot listen` / `Permission denied` catch-alls, because the specific
    // line usually arrives first and the vague one follows.
    bind_rules(l, &lower)
        .or_else(|| listener_rules(l, &lower))
        .or_else(|| remote_forward_rule(l, &lower))
        .or_else(|| target_rule(l, &lower))
        .or_else(|| host_key_rule(l, &lower))
        .or_else(|| dns_rule(l, &lower))
        .or_else(|| refused_rule(l, &lower))
        .or_else(|| timeout_rule(l, &lower))
        .or_else(|| auth_rule(l, &lower))
}

/// `bind …` failures: which address/port we could not grab, and why.
fn bind_rules(l: &str, lower: &str) -> Option<FailureInfo> {
    if !lower.contains("bind ") {
        return None;
    }
    if lower.contains("address already in use") {
        let short = match port_from_line(l) {
            Some(p) => fit_short(format!("本地端口 {p} 被占用")),
            None => fit_short("本地端口被占用".to_string()),
        };
        return Some(FailureInfo::with_short(FailureKind::PortInUse, short, l));
    }
    if lower.contains("permission denied") {
        let short = match port_from_line(l) {
            Some(p) => fit_short(format!("端口 {p} 权限不足")),
            None => fit_short("端口权限不足(<1024)".to_string()),
        };
        return Some(FailureInfo::with_short(FailureKind::PermissionDenied, short, l));
    }
    None
}

/// The vague "the listener never came up" lines.
///
/// On their own these are ambiguous — they follow either a port conflict or a
/// permission error — so they only get a specific kind when the same line also
/// carries `bind` context.
fn listener_rules(l: &str, lower: &str) -> Option<FailureInfo> {
    if !(lower.contains("cannot listen to port") || lower.contains("could not request local forwarding")) {
        return None;
    }
    let kind = if lower.contains("bind ") { FailureKind::PortInUse } else { FailureKind::Unknown };
    Some(FailureInfo::with_short(kind, fit_short(l.to_string()), l))
}

/// `-R` failures: the server rejected our listen request.
fn remote_forward_rule(l: &str, lower: &str) -> Option<FailureInfo> {
    let hit = lower.contains("remote port forwarding failed")
        || lower.contains("remote forward failure")
        || lower.contains("remote forwarding failed");
    if !hit {
        return None;
    }
    let short = match port_from_line(l) {
        Some(p) => fit_short(format!("远端端口 {p} 转发失败")),
        None => fit_short("远端转发失败".to_string()),
    };
    Some(FailureInfo::with_short(FailureKind::RemoteForwardFailed, short, l))
}

/// The target behind a working tunnel refused the connection.
fn target_rule(l: &str, lower: &str) -> Option<FailureInfo> {
    if !(lower.contains("open failed") && lower.contains("connect failed")) {
        return None;
    }
    Some(FailureInfo::with_short(FailureKind::TargetUnreachable, fit_short("目标不可达(隧道内)".to_string()), l))
}

fn host_key_rule(l: &str, lower: &str) -> Option<FailureInfo> {
    let hit = lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
        || (lower.contains("no ") && lower.contains("host key is known for"));
    if !hit {
        return None;
    }
    Some(FailureInfo::with_short(FailureKind::HostKeyMismatch, fit_short("主机密钥校验失败".to_string()), l))
}

fn dns_rule(l: &str, lower: &str) -> Option<FailureInfo> {
    if !lower.contains("could not resolve hostname") {
        return None;
    }
    let short = match host_from_line(l) {
        Some(h) => fit_short(format!("无法解析 {h}")),
        None => fit_short("域名解析失败".to_string()),
    };
    Some(FailureInfo::with_short(FailureKind::DnsFailure, short, l))
}

/// Only *connection* refusals matter.
///
/// OpenSSH emits benign refusals that do not fail the session at all — measured
/// on OpenSSH 8.2 with no agent running:
/// `debug1: pubkey_prepare: ssh_get_authentication_socket: Connection refused`
/// The connection then proceeds and the forward comes up. Treating that line as
/// a failure marked a healthy tunnel as failed (and killed it).
fn refused_rule(l: &str, lower: &str) -> Option<FailureInfo> {
    if !lower.contains("connection refused") {
        return None;
    }
    let agent_socket = lower.contains("ssh_get_authentication_socket")
        || lower.contains("get_authentication_socket")
        || lower.contains("agent")
        || lower.contains("identity agent");
    let real_connection = lower.contains("connect to host") || lower.contains("open failed");
    if agent_socket && !real_connection {
        return None;
    }
    Some(FailureInfo::with_short(FailureKind::ConnectionRefused, fit_short("连接被拒绝".to_string()), l))
}

fn timeout_rule(l: &str, lower: &str) -> Option<FailureInfo> {
    let hit = lower.contains("connection timed out")
        || lower.contains("operation timed out")
        || lower.contains("timeout, server")
        || lower.contains("timed out");
    if !hit {
        return None;
    }
    Some(FailureInfo::with_short(FailureKind::Timeout, fit_short("连接超时".to_string()), l))
}

/// Any `Permission denied` that is not about binding a port is authentication.
fn auth_rule(l: &str, lower: &str) -> Option<FailureInfo> {
    if !lower.contains("permission denied") {
        return None;
    }
    Some(FailureInfo::with_short(FailureKind::AuthFailed, fit_short("认证失败(公钥/agent)".to_string()), l))
}

/// Marks that a tunnel's forwarding is actually established. Failure lines
/// must be checked first (§2.3).
pub fn is_ready_line(kind: ForwardKind, line: &str) -> bool {
    match kind {
        ForwardKind::Local | ForwardKind::Dynamic => line.contains("Entering interactive session."),
        ForwardKind::Remote => {
            line.contains("remote forward success for") || line.contains("Entering interactive session.")
        }
    }
}

/// True for the line that announces a *local* forwarding listener:
/// `debug1: Local forwarding listening on 127.0.0.1 port 18500.`
///
/// Exposed for callers that need staged readiness: with `ProxyJump`, ssh forks
/// `ssh … -W` and the child's stderr is merged into ours, so the child's own
/// `Entering interactive session.` can appear *before* the target is even
/// authenticated (measured). Requiring this listener line first (and only then
/// accepting [`is_ready_line`]) removes that false positive without any state
/// living in this module.
pub fn is_local_listening_line(line: &str) -> bool {
    line.contains("Local forwarding listening on")
}

/// Best-effort port extraction from an ssh stderr line.
fn port_from_line(line: &str) -> Option<u16> {
    // Prefer `]:<port>:` (bind lines) then `port <n>` then any 1..=5 digit run
    // that follows a ':' or whitespace.
    if let Some(close) = line.find("]:") {
        let tail = &line[close + 2..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = digits.parse::<u16>()
            && p != 0
        {
            return Some(p);
        }
    }
    if let Some(idx) = line.to_ascii_lowercase().find("port ") {
        let tail = &line[idx + 5..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = digits.parse::<u16>()
            && p != 0
        {
            return Some(p);
        }
    }
    for token in line.split(|c: char| !c.is_ascii_digit()) {
        if !token.is_empty()
            && token.len() <= 5
            && let Ok(p) = token.parse::<u16>()
            && p != 0
        {
            return Some(p);
        }
    }
    None
}

fn host_from_line(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let idx = lower.find("could not resolve hostname")?;
    let tail = &line[idx + "could not resolve hostname".len()..];
    let tail = tail.trim_start_matches([':', ' ']);
    let host: String = tail.chars().take_while(|c| *c != ':' && !c.is_whitespace()).collect();
    if host.is_empty() { None } else { Some(host) }
}

/// Truncate to at most [`SHORT_MAX_WIDTH`] display cells, ending in `…`.
pub(crate) fn fit_short(mut s: String) -> String {
    if display_width(&s) <= SHORT_MAX_WIDTH {
        return s;
    }
    let budget = SHORT_MAX_WIDTH - 1;
    let mut width = 0;
    let mut cut = 0;
    for (i, c) in s.char_indices() {
        let w = char_width(c);
        if width + w > budget {
            break;
        }
        width += w;
        cut = i + c.len_utf8();
    }
    s.truncate(cut);
    s.push('…');
    s
}
