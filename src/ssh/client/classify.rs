//! Turning a `russh`/`anyhow` error into a user-facing [`FailureInfo`].
//!
//! This replaces the old `classify_failure(stderr_line)` regexes: `russh` reports
//! *typed* errors, so the variant decides the [`FailureKind`] and the text is kept
//! only as a fallback (and as the `detail` shown in the detail panel). The text
//! fallback still matters: a few failures originate in *our own* context
//! strings ("认证 … 失败", "请求远端监听 … 失败") rather than in a `russh::Error`.
//!
//! A refused host key is special: `russh` reports it as a generic `UnknownKey`, so
//! the handler's recorded verdict is the only explanation available. That is why
//! [`HostKeyRejected`] travels inside the `anyhow` chain rather than as a string.

use crate::model::{FailureInfo, FailureKind};
use crate::ssh::known_hosts;

/// Map an error onto a user-facing [`FailureInfo`].
///
/// Replaces the old `classify_failure(stderr_line)` regexes: `russh` reports
/// typed errors, so we branch on the variant and keep the text only as a
/// fallback (and as the `detail` shown in the panel).
pub fn classify(err: &anyhow::Error) -> FailureInfo {
    let text = format!("{err:#}");
    let lower = text.to_lowercase();
    let resolution = is_resolution_error(err, &lower);

    if let Some(stale) = find_stale_session(err) {
        let detail = stale.to_string();
        return FailureInfo {
            kind: FailureKind::StaleConnection,
            short: short_reason(FailureKind::StaleConnection, &detail),
            detail,
        };
    }
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
            _ if resolution => FailureKind::DnsFailure,
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
        None if resolution => FailureKind::DnsFailure,
        None => classify_text(&lower),
    };

    FailureInfo { kind, short: short_reason(kind, &text), detail: text }
}

/// Whether an error, or any cause in its chain, is a failed name lookup.
///
/// The message itself cannot be relied on: Windows localises it, so a Chinese
/// install reports `不知道这样的主机。 (os error 11001)` instead of the English
/// `failed to lookup address information` that Unix produces. The numeric code
/// is stable across locales, so it is checked first; the English wording stays
/// as the fallback for Unix (and for `getaddrinfo` failures raised as text).
fn is_resolution_error(err: &anyhow::Error, lower: &str) -> bool {
    let coded = err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().and_then(std::io::Error::raw_os_error).is_some_and(is_resolution_code)
    });
    coded || lower.contains("lookup address") || lower.contains("name resolution")
}

/// Winsock codes for "the name could not be resolved": `WSAHOST_NOT_FOUND`,
/// `WSATRY_AGAIN`, `WSANO_RECOVERY` and `WSANO_DATA`.
///
/// No `cfg(windows)` is needed: these numbers are not Unix errno values, so the
/// predicate can be exercised — and is unit-tested — on every platform.
fn is_resolution_code(code: i32) -> bool {
    matches!(code, 11001..=11004)
}

/// Text fallback for errors we authored ourselves (context strings).
pub(crate) fn classify_text(lower: &str) -> FailureKind {
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
///
/// Carried as an [`anyhow::Error`] cause (rather than formatted to a string)
/// because [`classify`] needs the machine-readable verdict, not its wording.
#[derive(Debug)]
pub(crate) struct HostKeyRejected {
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

/// Find a [`StaleSession`](super::health::StaleSession) anywhere in the chain.
///
/// Checked before the text fallback because its own wording ("探活超时…")
/// would otherwise classify as a generic `Timeout`, which sends a reader looking
/// at the last connection attempt instead of at the run that had been healthy.
fn find_stale_session(err: &anyhow::Error) -> Option<&super::health::StaleSession> {
    err.chain().find_map(|c| c.downcast_ref::<super::health::StaleSession>())
}

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
        FailureKind::StaleConnection => "连接已失效",
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

/// A refused server key as an [`anyhow::Error`] cause, so [`classify`] can read
/// the verdict instead of parsing its wording. One constructor keeps the three
/// fields from being wired up wrong at the two places that raise it.
pub(crate) fn host_key_rejected(verdict: known_hosts::HostKeyVerdict, host: String, port: u16) -> anyhow::Error {
    anyhow::Error::new(HostKeyRejected { verdict, host, port })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

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
    fn classifies_dns_failure_from_a_winsock_code() {
        // Windows localises the message (`不知道这样的主机。`), so text matching
        // alone classifies this as Unknown. The code is the stable signal, and
        // constructing the error directly keeps the test valid on every platform.
        let e = anyhow::Error::new(std::io::Error::from_raw_os_error(11001)).context("连接目标主机 a:22 失败");
        let info = classify(&e);
        assert_eq!(info.kind, FailureKind::DnsFailure, "detail: {}", info.detail);
        assert_eq!(info.short, "域名解析失败");
    }

    #[test]
    fn an_unrelated_uncategorised_io_error_stays_unknown() {
        // The Winsock range must not turn every Uncategorized I/O failure into a
        // DNS one: only 11001–11004 are name-lookup failures.
        let e = anyhow::Error::new(std::io::Error::from_raw_os_error(10054)).context("连接目标主机 a:22 失败");
        assert_eq!(classify(&e).kind, FailureKind::Unknown);
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
}
