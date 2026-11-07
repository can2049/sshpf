//! Shared domain model. Pure data types, no `ratatui`, no I/O.
//!
//! This module is the contract between the persistence layer (`config`),
//! the runtime layer (`tunnel` + `ssh`) and the presentation layer (`ui`).

use std::collections::VecDeque;
use std::time::Instant;

pub type TunnelId = u64;

/// Which OpenSSH forwarding flavour a tunnel uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardKind {
    /// `-L`: listen locally, forward to an address reachable from the SSH server.
    #[default]
    Local,
    /// `-R`: listen on the SSH server, forward back to an address local to us.
    Remote,
    /// `-D`: local SOCKS5 proxy.
    Dynamic,
}

impl ForwardKind {
    pub fn tag(self) -> char {
        match self {
            ForwardKind::Local => 'L',
            ForwardKind::Remote => 'R',
            ForwardKind::Dynamic => 'D',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ForwardKind::Local => "本地转发 -L",
            ForwardKind::Remote => "远程转发 -R",
            ForwardKind::Dynamic => "动态转发 -D",
        }
    }

    pub fn all() -> [ForwardKind; 3] {
        [ForwardKind::Local, ForwardKind::Remote, ForwardKind::Dynamic]
    }
}

/// A persisted tunnel definition (the user's intent).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TunnelConfig {
    pub id: TunnelId,
    pub name: String,
    #[serde(default)]
    pub kind: ForwardKind,
    /// ssh config alias (e.g. `cloud_pc`) or `user@host:port`.
    pub ssh_host: String,
    /// Local listen address for `-L`/`-D`, local target address for `-R`.
    #[serde(default = "default_local_addr")]
    pub local_addr: String,
    pub local_port: u16,
    /// Remote target address for `-L`, remote listen address for `-R`.
    #[serde(default = "default_local_addr")]
    pub remote_host: String,
    pub remote_port: u16,
    /// User intent: should this tunnel be running? Restored on start-up.
    #[serde(default)]
    pub enabled: bool,
    /// Auto reconnect on network-class failures.
    #[serde(default)]
    pub auto_restart: bool,
    /// Extra raw `ssh -o KEY=VALUE` options.
    #[serde(default)]
    pub ssh_options: Vec<String>,
    /// Free-form note shown in the detail panel.
    #[serde(default)]
    pub note: String,
    /// Unix timestamp of creation, informational only.
    #[serde(default)]
    pub created_at: i64,
}

fn default_local_addr() -> String {
    "127.0.0.1".to_string()
}

impl TunnelConfig {
    /// `127.0.0.1:8080`
    pub fn local_display(&self) -> String {
        format!("{}:{}", self.local_addr, self.local_port)
    }

    /// `127.0.0.1:22`, or `(SOCKS5)` for dynamic forwards.
    pub fn remote_display(&self) -> String {
        match self.kind {
            ForwardKind::Dynamic => "(SOCKS5)".to_string(),
            _ => format!("{}:{}", self.remote_host, self.remote_port),
        }
    }

    /// Spec string passed to `ssh -L/-R/-D`.
    pub fn forward_spec(&self) -> String {
        match self.kind {
            ForwardKind::Local => {
                format!("{}:{}:{}:{}", self.local_addr, self.local_port, self.remote_host, self.remote_port)
            }
            ForwardKind::Remote => {
                format!("{}:{}:{}:{}", self.remote_host, self.remote_port, self.local_addr, self.local_port)
            }
            ForwardKind::Dynamic => format!("{}:{}", self.local_addr, self.local_port),
        }
    }

    /// 0..=33 are privileged ports on Linux.
    pub fn uses_privileged_port(&self) -> bool {
        let listen_port = match self.kind {
            ForwardKind::Local | ForwardKind::Dynamic => self.local_port,
            ForwardKind::Remote => self.remote_port,
        };
        listen_port <= 1023
    }
}

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

/// Live status of a tunnel. Rebuilt from scratch on every start/stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelStatus {
    /// Not running (user intent off, or stopped by hand).
    Stopped,
    /// ssh spawned, waiting for the forwarding-ready marker.
    Starting { since: Instant },
    /// Forwarding is listening and the session is established.
    Up { since: Instant },
    /// Waiting for the next automatic reconnect attempt.
    Retrying { attempt: u32, next_at: Instant, reason: FailureInfo },
    /// Terminal failure; no automatic retry.
    Failed { reason: FailureInfo },
}

impl TunnelStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, TunnelStatus::Up { .. } | TunnelStatus::Starting { .. })
    }

    pub fn is_running_or_pending(&self) -> bool {
        !matches!(self, TunnelStatus::Stopped)
    }

    /// Stable lowercase key used by the UI for styling.
    pub fn key(&self) -> &'static str {
        match self {
            TunnelStatus::Stopped => "stopped",
            TunnelStatus::Starting { .. } => "starting",
            TunnelStatus::Up { .. } => "up",
            TunnelStatus::Retrying { .. } => "retrying",
            TunnelStatus::Failed { .. } => "failed",
        }
    }

    pub fn failure(&self) -> Option<&FailureInfo> {
        match self {
            TunnelStatus::Failed { reason } => Some(reason),
            TunnelStatus::Retrying { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// One line in a tunnel's per-tunnel log ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Pre-formatted `HH:MM:SS`.
    pub time: String,
    pub level: LogLevel,
    pub text: String,
}

impl LogLine {
    pub fn new(level: LogLevel, text: impl Into<String>) -> Self {
        LogLine { time: now_hms(), level, text: text.into() }
    }
}

/// Bounded FIFO of log lines.
#[derive(Debug, Clone)]
pub struct LogRing {
    buf: VecDeque<LogLine>,
    cap: usize,
}

impl LogRing {
    pub fn new(cap: usize) -> Self {
        LogRing { buf: VecDeque::with_capacity(cap.min(1024)), cap }
    }

    pub fn push(&mut self, line: LogLine) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(line);
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &LogLine> {
        self.buf.iter()
    }

    pub fn last(&self) -> Option<&LogLine> {
        self.buf.back()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let skip = self.buf.len().saturating_sub(n);
        self.buf.iter().skip(skip).cloned().collect()
    }
}

impl Default for LogRing {
    fn default() -> Self {
        LogRing::new(1000)
    }
}

/// A tunnel as presented by the UI: definition + live state + its log.
#[derive(Debug, Clone)]
pub struct TunnelView {
    pub cfg: TunnelConfig,
    pub status: TunnelStatus,
    pub log: LogRing,
    /// Number of automatic reconnect attempts made so far (for the detail panel).
    pub retries: u32,
}

impl TunnelView {
    pub fn new(cfg: TunnelConfig) -> Self {
        let running = cfg.enabled;
        TunnelView {
            cfg,
            status: if running { TunnelStatus::Starting { since: Instant::now() } } else { TunnelStatus::Stopped },
            log: LogRing::new(1000),
            retries: 0,
        }
    }

    /// Short reason shown in the LAST ERROR column, `None` when healthy.
    pub fn last_error(&self) -> Option<String> {
        self.status.failure().map(|f| f.short.clone())
    }
}

/// Format the current wall clock as `HH:MM:SS` in local time.
pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Unix timestamp right now.
pub fn now_unix() -> i64 {
    chrono::Local::now().timestamp()
}

// ---------------------------------------------------------------------------
// UI-visible transient state (form / host picker). Kept here so the `ui`
// module can render it without depending on the `app` module.
// ---------------------------------------------------------------------------

/// Editable fields of the create/edit form, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    Name,
    Kind,
    SshHost,
    LocalAddr,
    LocalPort,
    RemoteHost,
    RemotePort,
    AutoRestart,
}

impl FormField {
    /// Field order, chosen so the form reads top-to-bottom in the order a user
    /// actually thinks about a forward: pick the machine, name it, then say
    /// *where* the traffic goes (remote port) before *where it lands locally*
    /// (local port) — because the remote port is what the user knows, and the
    /// local port is derived from it as a suggestion.
    pub const ORDER: [FormField; 8] = [
        FormField::SshHost,
        FormField::Name,
        FormField::Kind,
        FormField::RemoteHost,
        FormField::RemotePort,
        FormField::LocalAddr,
        FormField::LocalPort,
        FormField::AutoRestart,
    ];

    /// Fields the user types into (as opposed to toggles/selects).
    pub fn is_text(self) -> bool {
        matches!(
            self,
            FormField::Name
                | FormField::SshHost
                | FormField::LocalAddr
                | FormField::LocalPort
                | FormField::RemoteHost
                | FormField::RemotePort
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            FormField::Name => "名称",
            FormField::Kind => "类型",
            FormField::SshHost => "SSH 主机",
            FormField::LocalAddr => "本地地址",
            FormField::LocalPort => "本地端口",
            FormField::RemoteHost => "远端地址",
            FormField::RemotePort => "远端端口",
            FormField::AutoRestart => "自动重连",
        }
    }

    pub fn next(self) -> FormField {
        let i = FormField::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        FormField::ORDER[(i + 1) % FormField::ORDER.len()]
    }

    pub fn prev(self) -> FormField {
        let i = FormField::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        FormField::ORDER[(i + FormField::ORDER.len() - 1) % FormField::ORDER.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    Create,
    Edit(TunnelId),
}

/// A text input buffer with a cursor position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBuf {
    pub text: String,
    pub cursor: usize,
}

impl InputBuf {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        InputBuf { text, cursor }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.chars().count();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        let byte = self.byte_index(self.cursor);
        self.text.insert(byte, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_index(self.cursor);
        let start = self.byte_index(self.cursor - 1);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        let len = self.text.chars().count();
        if self.cursor >= len {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        let len = self.text.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.text.len())
    }
}

/// The SSH-HOST dropdown of the create/edit form.
///
/// Rendered as an inline list attached to the SSH HOST field (not a separate
/// modal), so choosing a host from `~/.ssh/config` is the obvious path rather
/// than a hidden shortcut. `open` controls whether the list is expanded.
#[derive(Debug, Clone, Default)]
pub struct HostPicker {
    pub open: bool,
    pub query: String,
    /// Indices into the caller-supplied host list, best match first.
    pub matches: Vec<usize>,
    pub selected: usize,
    /// First visible row in the dropdown (scrolling window).
    pub scroll: u16,
}

impl HostPicker {
    pub fn new() -> Self {
        HostPicker::default()
    }

    /// `DOWN` arrow marker shown inside the field when the list is collapsed.
    pub fn marker(&self) -> &'static str {
        if self.open { "▴" } else { "▾" }
    }

    pub fn query_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn query_backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.scroll = 0;
    }

    /// Move down, keeping the selection inside a `visible`-row window.
    pub fn down(&mut self, visible: usize) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
            self.ensure_visible(visible);
        }
    }

    /// Move up, keeping the selection inside a `visible`-row window.
    pub fn up(&mut self, visible: usize) {
        self.selected = self.selected.saturating_sub(1);
        self.ensure_visible(visible);
    }

    pub fn home(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn end(&mut self) {
        self.selected = self.matches.len().saturating_sub(1);
    }

    /// Clip `scroll` so the selected row is inside the viewport.
    pub fn ensure_visible(&mut self, visible: usize) {
        if visible == 0 {
            self.scroll = self.selected.min(u16::MAX as usize) as u16;
            return;
        }
        let sel = self.selected as u16;
        if sel < self.scroll {
            self.scroll = sel;
        } else if sel >= self.scroll + visible as u16 {
            self.scroll = sel + 1 - visible as u16;
        }
    }

    pub fn current(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }
}

/// Non-modal single-line message shown at the bottom of the screen.
#[derive(Debug, Clone)]
pub struct StatusMsg {
    pub text: String,
    pub level: LogLevel,
    pub at: Instant,
}

impl StatusMsg {
    pub fn info(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Info, at: Instant::now() }
    }

    pub fn ok(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Info, at: Instant::now() }
    }

    pub fn warn(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Warn, at: Instant::now() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Error, at: Instant::now() }
    }

    /// Messages fade out after ~4s.
    pub fn expired(&self) -> bool {
        self.at.elapsed().as_millis() > 4000
    }
}

/// Simple case-insensitive subsequence matcher used by the host picker and the
/// table filter. Empty needle matches everything.
pub fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut hay = haystack.chars().flat_map(|c| c.to_lowercase());
    for nc in needle.chars().flat_map(|c| c.to_lowercase()) {
        if nc == ' ' {
            continue;
        }
        loop {
            match hay.next() {
                Some(hc) if hc == nc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_spec_local() {
        let cfg = TunnelConfig {
            id: 1,
            name: "x".into(),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "10.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        };
        assert_eq!(cfg.forward_spec(), "127.0.0.1:8080:10.0.0.1:80");
        assert_eq!(cfg.local_display(), "127.0.0.1:8080");
        assert_eq!(cfg.remote_display(), "10.0.0.1:80");
    }

    #[test]
    fn input_buf_edits_multibyte() {
        let mut b = InputBuf::new("中文");
        assert_eq!(b.cursor, 2);
        b.left();
        b.insert('X');
        assert_eq!(b.value(), "中X文");
        b.backspace();
        assert_eq!(b.value(), "中文");
        b.delete();
        assert_eq!(b.value(), "中");
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("dbs", "db-staging"));
        assert!(fuzzy_match("DB", "db-staging"));
        assert!(!fuzzy_match("zz", "db-staging"));
    }

    #[test]
    fn log_ring_bounded() {
        let mut r = LogRing::new(3);
        for i in 0..10 {
            r.push(LogLine::new(LogLevel::Info, format!("{i}")));
        }
        assert_eq!(r.len(), 3);
        let tail: Vec<_> = r.iter().map(|l| l.text.clone()).collect();
        assert_eq!(tail, vec!["7", "8", "9"]);
    }

    fn sample(kind: ForwardKind) -> TunnelConfig {
        TunnelConfig {
            id: 1,
            name: "t".into(),
            kind,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "10.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        }
    }

    #[test]
    fn forward_kind_tags_and_labels() {
        assert_eq!(ForwardKind::Local.tag(), 'L');
        assert_eq!(ForwardKind::Remote.tag(), 'R');
        assert_eq!(ForwardKind::Dynamic.tag(), 'D');
        assert!(ForwardKind::Local.label().contains("-L"));
        assert_eq!(ForwardKind::all().len(), 3);
        assert_eq!(ForwardKind::default(), ForwardKind::Local);
    }

    #[test]
    fn forward_spec_for_each_kind() {
        // -L: local listen -> remote target
        assert_eq!(sample(ForwardKind::Local).forward_spec(), "127.0.0.1:8080:10.0.0.1:80");
        // -R reverses the two ends: remote listen -> local target
        assert_eq!(sample(ForwardKind::Remote).forward_spec(), "10.0.0.1:80:127.0.0.1:8080");
        // -D only needs a listen address
        assert_eq!(sample(ForwardKind::Dynamic).forward_spec(), "127.0.0.1:8080");
    }

    #[test]
    fn displays_are_readable_for_each_kind() {
        let local = sample(ForwardKind::Local);
        assert_eq!(local.local_display(), "127.0.0.1:8080");
        assert_eq!(local.remote_display(), "10.0.0.1:80");

        // A dynamic forward has no remote target: say so rather than printing
        // a stale host:port.
        assert_eq!(sample(ForwardKind::Dynamic).remote_display(), "(SOCKS5)");
    }

    #[test]
    fn privileged_port_detection_follows_the_listen_side() {
        // -L and -D listen locally, -R listens remotely.
        let mut local = sample(ForwardKind::Local);
        local.local_port = 80;
        assert!(local.uses_privileged_port());

        let mut remote = sample(ForwardKind::Remote);
        remote.remote_port = 80;
        assert!(remote.uses_privileged_port(), "-R binds the remote port");
        // Once the remote listen port is unprivileged we are fine again, even
        // though the local *target* port is 8080 (we never bind it for -R).
        remote.remote_port = 9999;
        assert!(!remote.uses_privileged_port());

        let mut dynamic = sample(ForwardKind::Dynamic);
        dynamic.local_port = 1080;
        assert!(!dynamic.uses_privileged_port());
        dynamic.local_port = 1023;
        assert!(dynamic.uses_privileged_port());
        dynamic.local_port = 1024;
        assert!(!dynamic.uses_privileged_port(), "1024 is the first unprivileged port");
    }

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
    fn status_helpers_agree_with_the_variant() {
        let up = TunnelStatus::Up { since: Instant::now() };
        assert!(up.is_active());
        assert!(up.is_running_or_pending());
        assert_eq!(up.key(), "up");
        assert!(up.failure().is_none());

        let starting = TunnelStatus::Starting { since: Instant::now() };
        assert!(starting.is_active());
        assert_eq!(starting.key(), "starting");

        let stopped = TunnelStatus::Stopped;
        assert!(!stopped.is_active());
        assert!(!stopped.is_running_or_pending());
        assert_eq!(stopped.key(), "stopped");

        let info = FailureInfo::new(FailureKind::AuthFailed, "detail");
        let failed = TunnelStatus::Failed { reason: info.clone() };
        assert!(!failed.is_active());
        // A failed tunnel is not "running", but it is not "off" either: the user
        // asked for it to run and it is not running, so retry/stop apply.
        assert!(failed.is_running_or_pending());
        assert_eq!(failed.key(), "failed");
        assert_eq!(failed.failure(), Some(&info));

        let retrying = TunnelStatus::Retrying { attempt: 3, next_at: Instant::now(), reason: info };
        assert_eq!(retrying.key(), "retrying");
        assert!(retrying.failure().is_some());
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

    #[test]
    fn tunnel_view_has_no_error_when_healthy() {
        let mut cfg = sample(ForwardKind::Local);
        cfg.enabled = false;
        let mut v = TunnelView::new(cfg);
        v.status = TunnelStatus::Up { since: Instant::now() };
        assert!(v.last_error().is_none());

        v.status =
            TunnelStatus::Failed { reason: FailureInfo::with_short(FailureKind::AuthFailed, "认证失败", "stderr") };
        assert_eq!(v.last_error().as_deref(), Some("认证失败"));
    }

    #[test]
    fn log_ring_tail_and_clear() {
        let mut r = LogRing::new(5);
        for i in 0..8 {
            r.push(LogLine::new(LogLevel::Info, format!("{i}")));
        }
        assert_eq!(r.len(), 5);
        let tail: Vec<_> = r.tail(2).into_iter().map(|l| l.text).collect();
        assert_eq!(tail, vec!["6", "7"]);
        // Asking for more than we have returns everything, oldest first.
        assert_eq!(r.tail(100).len(), 5);
        assert_eq!(r.last().map(|l| l.text.as_str()), Some("7"));
        r.clear();
        assert!(r.is_empty());
        assert!(r.last().is_none());
        assert_eq!(r.tail(3).len(), 0);
    }

    #[test]
    fn log_line_timestamps_are_hms() {
        let l = LogLine::new(LogLevel::Warn, "x");
        assert_eq!(l.time.len(), 8, "expected HH:MM:SS, got {}", l.time);
        assert_eq!(l.time.matches(':').count(), 2);
    }

    #[test]
    fn input_buf_cursor_navigation() {
        let mut b = InputBuf::new("abc");
        assert_eq!(b.cursor, 3, "cursor starts at the end");
        b.home();
        assert_eq!(b.cursor, 0);
        b.insert('X');
        assert_eq!(b.value(), "Xabc");
        // Currently "Xabc" with the cursor after "X".
        b.end();
        assert_eq!(b.cursor, 4);
        b.left();
        b.left();
        assert_eq!(b.cursor, 2); // between 'a' and 'b'
        b.delete(); // delete the char under the cursor
        assert_eq!(b.value(), "Xac");
        b.right();
        b.right();
        b.right(); // no-op past the end
        assert_eq!(b.cursor, 3);
        b.clear();
        assert!(b.value().is_empty());
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn input_buf_backspace_at_start_is_a_noop() {
        let mut b = InputBuf::new("ab");
        b.home();
        b.backspace();
        assert_eq!(b.value(), "ab");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn input_buf_set_moves_cursor_to_end() {
        let mut b = InputBuf::new("abc");
        b.home();
        b.set("xyz");
        assert_eq!(b.value(), "xyz");
        assert_eq!(b.cursor, 3);
    }

    #[test]
    fn form_field_metadata() {
        // Every field is reachable by cycling, and text fields are flagged.
        for f in FormField::ORDER {
            assert!(!f.label().is_empty());
        }
        assert!(FormField::Name.is_text());
        assert!(FormField::SshHost.is_text());
        assert!(FormField::LocalPort.is_text());
        assert!(!FormField::Kind.is_text());
        assert!(!FormField::AutoRestart.is_text());
        // Host comes first: it is the field the user fills first.
        assert_eq!(FormField::ORDER[0], FormField::SshHost);
    }

    #[test]
    fn host_picker_scroll_window() {
        let mut p = HostPicker::new();
        p.matches = (0..20).collect();
        assert!(!p.open);
        assert_eq!(p.marker(), "▾");
        p.open = true;
        assert_eq!(p.marker(), "▴");

        // Walking down past the window pushes the viewport along.
        for _ in 0..10 {
            p.down(5);
        }
        assert_eq!(p.selected, 10);
        assert!(p.scroll <= 10);
        assert!(10 < p.scroll as usize + 5, "selection must stay visible");

        p.home();
        assert_eq!((p.selected, p.scroll), (0, 0));
        p.end();
        assert_eq!(p.selected, 19);
        assert_eq!(p.current(), Some(19));

        // A shrinking list must re-clamp the viewport.
        p.matches.truncate(3);
        p.ensure_visible(5);
        assert!(p.scroll as usize <= p.selected);
    }

    #[test]
    fn host_picker_queries_reset_the_cursor() {
        let mut p = HostPicker::new();
        p.matches = (0..5).collect();
        p.selected = 3;
        p.scroll = 2;
        p.query_char('a');
        assert_eq!((p.selected, p.scroll), (0, 0));
        p.selected = 2;
        p.query_backspace();
        assert_eq!((p.selected, p.scroll), (0, 0));
        assert_eq!(p.query, "");
    }

    #[test]
    fn fuzzy_match_edge_cases() {
        assert!(fuzzy_match("", ""), "empty needle matches empty haystack");
        assert!(fuzzy_match("", "abc"));
        assert!(!fuzzy_match("abc", ""), "non-empty needle cannot match nothing");
        assert!(fuzzy_match("abc", "abc"));
        assert!(fuzzy_match("ac", "abc"), "subsequence, not substring");
        assert!(!fuzzy_match("ca", "abc"), "order matters");
        assert!(fuzzy_match(" ssh ", "my-ssh-host"), "spaces in the needle are ignored");
        assert!(fuzzy_match("SSH", "ssh-box"), "case insensitive");
    }

    #[test]
    fn status_msg_expires() {
        let fresh = StatusMsg::info("hi");
        assert!(!fresh.expired(), "a just-created message is visible");
        let mut old = StatusMsg::ok("hi");
        old.at = Instant::now() - std::time::Duration::from_secs(10);
        assert!(old.expired(), "messages fade out");
        // All constructors carry a level the UI can colour by.
        assert_eq!(StatusMsg::error("e").level, LogLevel::Error);
        assert_eq!(StatusMsg::warn("w").level, LogLevel::Warn);
        assert_eq!(StatusMsg::ok("o").level, LogLevel::Info);
    }

    #[test]
    fn form_field_cycles() {
        let mut f = FormField::SshHost;
        for _ in 0..FormField::ORDER.len() {
            f = f.next();
        }
        assert_eq!(f, FormField::SshHost);
        assert_eq!(FormField::SshHost.prev(), FormField::AutoRestart);
    }
}
