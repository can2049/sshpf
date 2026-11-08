//! Classified ssh failures: a coarse `FailureKind` plus the raw text to show.
//!
//! One classification is shared by the supervisor (`tunnel`), the stderr
//! classifier (`ssh::client`) and the renderer, so the table column, the detail
//! panel and the retry decision can never disagree about what went wrong.
//!
//! Belongs here: the failure vocabulary, its short/English/ASCII wording, and
//! the retryability policy.
//! Does not belong: matching stderr text (that is `ssh::client::classify`).

/// Coarse failure classification, rendered as a short phrase in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// `bind ...: Address already in use`
    PortInUse,
    /// `bind ...: Permission denied` — privileged port without CAP_NET_BIND_SERVICE
    PermissionDenied,
    /// `Permission denied (publickey)` — no usable key / agent
    AuthFailed,
    /// `Could not resolve hostname`
    DnsFailure,
    /// `Connection refused`
    ConnectionRefused,
    /// `Connection timed out` / `Operation timed out`
    Timeout,
    /// `Host key verification failed` / `REMOTE HOST IDENTIFICATION HAS CHANGED`
    HostKeyMismatch,
    /// `remote port forwarding failed`
    RemoteForwardFailed,
    /// Target host:port behind the tunnel refused/reset on first use.
    TargetUnreachable,
    /// Could not spawn the `ssh` binary at all.
    SpawnFailed,
    /// ssh exited without a more specific classification.
    Exited,
    /// Feature not implemented for this forward kind yet.
    Unsupported,
    /// Anything else; `detail` carries the raw line.
    Unknown,
}

impl FailureKind {
    /// Short Chinese phrase for the table's LAST ERROR column (no newlines).
    pub fn short(self) -> &'static str {
        match self {
            FailureKind::PortInUse => "本地端口被占用",
            FailureKind::PermissionDenied => "端口权限不足(<1024)",
            FailureKind::AuthFailed => "认证失败(公钥/agent)",
            FailureKind::DnsFailure => "域名解析失败",
            FailureKind::ConnectionRefused => "连接被拒绝",
            FailureKind::Timeout => "连接超时",
            FailureKind::HostKeyMismatch => "主机密钥校验失败",
            FailureKind::RemoteForwardFailed => "远端转发失败",
            FailureKind::TargetUnreachable => "目标不可达",
            FailureKind::SpawnFailed => "无法启动 ssh",
            FailureKind::Exited => "ssh 异常退出",
            FailureKind::Unsupported => "该类型暂未支持",
            FailureKind::Unknown => "未知错误",
        }
    }

    /// English half of [`FailureKind::short`], for the bilingual UI.
    pub fn short_en(self) -> &'static str {
        match self {
            FailureKind::PortInUse => "Local port in use",
            FailureKind::PermissionDenied => "Port needs root (<1024)",
            FailureKind::AuthFailed => "Authentication failed",
            FailureKind::DnsFailure => "DNS lookup failed",
            FailureKind::ConnectionRefused => "Connection refused",
            FailureKind::Timeout => "Connection timed out",
            FailureKind::HostKeyMismatch => "Host key verification failed",
            FailureKind::RemoteForwardFailed => "Remote forward rejected",
            FailureKind::TargetUnreachable => "Target unreachable",
            FailureKind::SpawnFailed => "Cannot start ssh",
            FailureKind::Exited => "ssh exited unexpectedly",
            FailureKind::Unsupported => "Not supported yet",
            FailureKind::Unknown => "Unknown error",
        }
    }

    /// ASCII name, used for log fields where a stable greppable token beats the
    /// UI phrasing (`short()` is Chinese and may be reworded).
    pub fn name(self) -> &'static str {
        match self {
            FailureKind::PortInUse => "port_in_use",
            FailureKind::PermissionDenied => "permission_denied",
            FailureKind::AuthFailed => "auth_failed",
            FailureKind::DnsFailure => "dns_failure",
            FailureKind::ConnectionRefused => "connection_refused",
            FailureKind::Timeout => "timeout",
            FailureKind::HostKeyMismatch => "host_key_mismatch",
            FailureKind::RemoteForwardFailed => "remote_forward_failed",
            FailureKind::TargetUnreachable => "target_unreachable",
            FailureKind::SpawnFailed => "spawn_failed",
            FailureKind::Exited => "exited",
            FailureKind::Unsupported => "unsupported",
            FailureKind::Unknown => "unknown",
        }
    }

    /// Config-class failures are terminal: retrying cannot help until the user
    /// edits the definition. Network-class failures may be retried.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            FailureKind::ConnectionRefused
                | FailureKind::Timeout
                | FailureKind::DnsFailure
                | FailureKind::TargetUnreachable
                | FailureKind::Exited
                | FailureKind::Unknown
        )
    }
}

/// A classified failure: short phrase for the table, raw text for the detail view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureInfo {
    pub kind: FailureKind,
    /// Human readable short reason, may be more specific than `kind.short()`.
    pub short: String,
    /// Raw ssh stderr line(s) — shown verbatim in the detail panel.
    pub detail: String,
}

impl FailureInfo {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        FailureInfo { kind, short: kind.short().to_string(), detail }
    }

    pub fn with_short(kind: FailureKind, short: impl Into<String>, detail: impl Into<String>) -> Self {
        FailureInfo { kind, short: short.into(), detail: detail.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_kind_has_a_non_empty_short_phrase() {
        let kinds = [
            FailureKind::PortInUse,
            FailureKind::PermissionDenied,
            FailureKind::AuthFailed,
            FailureKind::DnsFailure,
            FailureKind::ConnectionRefused,
            FailureKind::Timeout,
            FailureKind::HostKeyMismatch,
            FailureKind::RemoteForwardFailed,
            FailureKind::TargetUnreachable,
            FailureKind::SpawnFailed,
            FailureKind::Exited,
            FailureKind::Unsupported,
            FailureKind::Unknown,
        ];
        for k in kinds {
            assert!(!k.short().is_empty(), "{k:?} has no short phrase");
            assert!(!k.short().contains('\n'), "{k:?} must stay single-line for the table");
        }
    }

    #[test]
    fn retryable_covers_network_class_only() {
        // Network hiccups may heal on their own.
        for k in [
            FailureKind::ConnectionRefused,
            FailureKind::Timeout,
            FailureKind::DnsFailure,
            FailureKind::TargetUnreachable,
            FailureKind::Exited,
            FailureKind::Unknown,
        ] {
            assert!(k.is_retryable(), "{k:?} should be retryable");
        }
        // Configuration problems need the user to change something.
        for k in [
            FailureKind::PortInUse,
            FailureKind::PermissionDenied,
            FailureKind::AuthFailed,
            FailureKind::HostKeyMismatch,
            FailureKind::RemoteForwardFailed,
            FailureKind::SpawnFailed,
            FailureKind::Unsupported,
        ] {
            assert!(!k.is_retryable(), "{k:?} must not auto-retry");
        }
    }

    #[test]
    fn failure_info_constructors() {
        let f = FailureInfo::new(FailureKind::Timeout, "raw stderr");
        assert_eq!(f.short, FailureKind::Timeout.short());
        assert_eq!(f.detail, "raw stderr");

        let g = FailureInfo::with_short(FailureKind::PortInUse, "端口 8080 被占用", "bind ...");
        assert_eq!(g.short, "端口 8080 被占用");
        assert_eq!(g.kind, FailureKind::PortInUse);
    }
}
