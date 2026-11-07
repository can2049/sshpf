//! SSH plumbing: `~/.ssh/config` discovery, `ssh` command construction, a
//! synthetic one-shot config and stderr classification.
//!
//! This layer is deliberately *pure plumbing*: it never spawns a long-lived
//! process (that is `tunnel`'s job) and never touches ratatui. The only short
//! lived processes it starts are `ssh -G -T …` probes used to resolve a host
//! definition (with a hard 3 s timeout) — see `docs/research/tech-options.md`
//! §2.1 for the measured evidence behind that choice.
//!
//! # Public API
//!
//! ```text
//! struct SshHost { alias, hostname, user, port, proxy_jump, identity_file }
//!     SshHost::display(&self) -> String
//!
//! fn default_config_paths() -> Vec<PathBuf>
//! fn list_hosts() -> Result<Vec<SshHost>>
//! fn resolve_host(alias: &str) -> Result<SshHost>
//! fn build_ssh_args(cfg: &TunnelConfig, synth_conf: Option<&Path>) -> Vec<String>
//! fn write_synth_config(cfg: &TunnelConfig, dir: &Path) -> Result<PathBuf>
//! fn classify_failure(line: &str) -> Option<FailureInfo>
//! fn is_ready_line(kind: ForwardKind, line: &str) -> bool
//! fn is_local_listening_line(line: &str) -> bool
//! fn display_width(s: &str) -> usize
//! fn ssh_binary() -> PathBuf
//! fn ssh_available() -> bool
//! fn port_available(addr: &str, port: u16) -> bool
//! fn suggest_free_port(addr: &str, start: u16) -> Option<u16>
//! fn port_owner(addr: &str, port: u16) -> Option<String>
//! ```
//!
//! # Design notes / measured facts (all verified on OpenSSH_8.2p1)
//!
//! * **Synthetic config, not `ClearAllForwardings`.** `-o ClearAllForwardings=yes`
//!   also clears *our* `-L` (§2.5), so a tunnel gets a one-shot `-F` file that
//!   re-states the resolved host verbatim. Baking the *full* `ssh -G` output is
//!   lossless: 73–74 keys round-trip `ssh -G -F synth <host>` byte-identically
//!   (verified against the machine's real 13-block config, including
//!   `identityfile` paths containing spaces, which must be re-quoted or ssh
//!   silently drops the line).
//! * `localforward`/`remoteforward`/`dynamicforward`/`clearallforwardings`/
//!   `proxycommand` are **excluded**: the first three would duplicate or
//!   re-bind the user's config forwards, `clearallforwardings` kills ours, and
//!   a `ProxyCommand` cannot be safely re-stated as a config line. Structural
//!   directives (`host`/`match`/`include`) are excluded too: emitting one
//!   inside a block would open a new `Host` context and drop every following
//!   key.
//! * `ProxyJump` needs more than the target's own block: ssh forks
//!   `ssh -F synth -W '[%h]:%p' <jump>` and that child re-reads the *synthetic*
//!   file, so every host on the jump chain is baked too (verified: end-to-end
//!   `Authenticated to … (via proxy)` through a baked two-hop chain).
//! * `-v` is enough to get `debug1:` markers; a `LogLevel` baked into the
//!   config does **not** override it (measured: baked `LogLevel ERROR` still
//!   printed `debug1:` lines), and adding `-o LogLevel=…` on the command line
//!   is not needed — so we only ever pass `-v`.
//! * Host enumeration cannot be done with `ssh -G '*'` (§2.1): the `Host`
//!   lines are parsed from the config files and each alias is then *evaluated*
//!   with `ssh -G -T`. The alias list itself is the source of truth for "does
//!   this host exist"; the extra `ssh -G -T -v | grep 'Applying options for'`
//!   check is deliberately skipped to avoid doubling the number of processes
//!   (documented trade-off, §2.1 measured both to agree on 12/12 hosts).
//! * A destination of the shape `user@host:port` is **not** understood by ssh
//!   as a port (`ssh user@h:2222` tries to resolve the literal `h:2222`). It is
//!   resolved locally here, and the synthetic config carries
//!   `Host <host>:<port>` + explicit `hostname`/`port`, which is exactly the
//!   pattern ssh matches for that destination (measured).
//!
//! # Environment overrides
//!
//! * `SSHPF_SSH_CONFIG` — parse only this file instead of `~/.ssh/config`
//!   (tests, and `--config`-style experimentation). Relative `Include`s then
//!   resolve against that file's directory instead of `~/.ssh`.
//! * `SSHPF_SSH_BIN` — use this executable instead of searching `PATH`
//!   (end-to-end tests with a fake `ssh` script).

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::model::{ForwardKind, TunnelConfig};

pub mod config_parse;
pub mod failure;
#[cfg(test)]
pub(crate) mod testutil;

pub use config_parse::{list_hosts, resolve_host};
pub use failure::{classify_failure, is_local_listening_line, is_ready_line};

use config_parse::{HostSpec, first_value, ssh_g};

pub const CONFIG_ENV: &str = "SSHPF_SSH_CONFIG";
/// Env var that overrides the `ssh` binary used by this module.
pub const SSH_BIN_ENV: &str = "SSHPF_SSH_BIN";

/// Hard timeout for one `ssh -G` probe. Measured: ~5 ms typical, 5 ms for
/// unknown hosts; 3 s is a generous guard against a pathological config
/// (e.g. a FIFO or NFS-hung include).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Recursion guard for `Include` chains.
const MAX_INCLUDE_DEPTH: usize = 8;
/// Recursion guard for `ProxyJump` chains.
const MAX_JUMP_DEPTH: usize = 4;

/// Maximum display width of a [`FailureInfo::short`] phrase, in terminal cells.
pub const SHORT_MAX_WIDTH: usize = 34;

// ---------------------------------------------------------------------------
// Host discovery
// ---------------------------------------------------------------------------

/// One `Host` entry of `~/.ssh/config`, resolved through `ssh -G`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: u16,
    pub proxy_jump: Option<String>,
    pub identity_file: Option<String>,
}

impl SshHost {
    /// `alias (user@hostname:port)`, omitting the user when there is none and
    /// the port when it is the default 22.
    pub fn display(&self) -> String {
        let mut who = String::new();
        if let Some(user) = &self.user {
            who.push_str(user);
            who.push('@');
        }
        who.push_str(&self.hostname);
        if self.port != 22 {
            who.push(':');
            who.push_str(&self.port.to_string());
        }
        format!("{} ({})", self.alias, who)
    }

    /// `user@hostname:port` — handy for the detail panel / copy-paste.
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
}

/// Config files to parse by default: `~/.ssh/config` when it exists.
pub fn default_config_paths() -> Vec<PathBuf> {
    match dirs::home_dir() {
        Some(home) => {
            let p = home.join(".ssh").join("config");
            if p.is_file() { vec![p] } else { Vec::new() }
        }
        None => Vec::new(),
    }
}

pub(crate) fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub(crate) fn config_override() -> Option<PathBuf> {
    std::env::var_os(CONFIG_ENV).map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Command construction
// ---------------------------------------------------------------------------

/// Options every tunnel client gets. `-v` is required for the readiness
/// markers; a `LogLevel` must **not** be added (measured: the config's
/// `LogLevel` does not defeat `-v`, but adding one on the command line is
/// pointless and easy to get wrong — §2.3).
const BASE_ARGS: &[&str] = &[
    "-N",
    "-v",
    "-o",
    "ExitOnForwardFailure=yes",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
];

/// Build the `ssh` argument vector for one tunnel. Arguments stay separated —
/// never a shell string.
pub fn build_ssh_args(cfg: &TunnelConfig, synth_conf: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = BASE_ARGS.iter().map(|s| (*s).to_string()).collect();

    if let Some(conf) = synth_conf {
        args.push("-F".to_string());
        args.push(conf.display().to_string());
    }
    for opt in &cfg.ssh_options {
        let opt = opt.trim();
        if opt.is_empty() {
            continue;
        }
        args.push("-o".to_string());
        args.push(opt.to_string());
    }

    match cfg.kind {
        ForwardKind::Local => {
            args.push("-L".to_string());
            args.push(cfg.forward_spec());
        }
        ForwardKind::Remote => {
            args.push("-R".to_string());
            args.push(cfg.forward_spec());
        }
        ForwardKind::Dynamic => {
            args.push("-D".to_string());
            args.push(format!("{}:{}", cfg.local_addr, cfg.local_port));
        }
    }

    args.push(cfg.ssh_host.clone());
    args
}

// ---------------------------------------------------------------------------
// Synthetic one-shot config
// ---------------------------------------------------------------------------

/// Keys of `ssh -G` output that must never be re-stated in a synthetic config
/// (`docs/research/tech-options.md` §2.5, §2.7).
const SYNTH_EXCLUDED: &[&str] = &[
    // re-binding the user's own forwards would duplicate / steal their ports
    "localforward",
    "remoteforward",
    "dynamicforward",
    // would cancel the `-L`/`-R`/`-D` we pass on the command line
    "clearallforwardings",
    // cannot be re-stated as a config line
    "proxycommand",
    // structural directives, never options: emitting one inside a block would
    // open a new `Host` context and silently drop every following key
    "host",
    "match",
    "include",
];

/// Keys whose value is a whitespace separated *list*: quoting them breaks ssh.
const MULTI_VALUE_KEYS: &[&str] = &[
    "rekeylimit",
    "ipqos",
    "userknownhostsfile",
    "globalknownhostsfile",
    "sendenv",
    "ciphers",
    "macs",
    "kexalgorithms",
    "hostkeyalgorithms",
    "hostbasedkeytypes",
    "pubkeyacceptedkeytypes",
    "casignaturealgorithms",
    "canonicaldomains",
    "proxyjump",
    "localforward",
    "remoteforward",
    "dynamicforward",
];

/// Write a one-shot ssh config for `cfg` into `dir` and return its path.
///
/// The file re-states the *resolved* configuration of `cfg.ssh_host` (plus its
/// whole `ProxyJump` chain) so that `ClearAllForwardings` tricks are
/// unnecessary and the user's own `LocalForward`s are not duplicated.
pub fn write_synth_config(cfg: &TunnelConfig, dir: &Path) -> Result<PathBuf> {
    write_synth_config_with(&ssh_binary(), cfg, dir, config_override().as_deref())
}

fn write_synth_config_with(
    bin: &Path,
    cfg: &TunnelConfig,
    dir: &Path,
    conf_override: Option<&Path>,
) -> Result<PathBuf> {
    let root = HostSpec::parse(&cfg.ssh_host);
    if root.host.is_empty() {
        bail!("SSH 主机为空");
    }

    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let mut visited: Vec<String> = Vec::new();
    bake_sections(bin, &root, conf_override, &mut sections, &mut visited, 0)?;
    if sections.is_empty() {
        bail!("无法解析 {} 的 ssh 配置", cfg.ssh_host);
    }

    let mut text = String::new();
    text.push_str("# sshpf generated config — do not edit, regenerated per start.\n");
    text.push_str(&format!("# tunnel: {} ({})  host: {}\n", cfg.id, cfg.name, cfg.ssh_host));
    for (key, lines) in &sections {
        text.push('\n');
        text.push_str(&format!("Host {key}\n"));
        for line in lines {
            text.push_str("    ");
            text.push_str(line);
            text.push('\n');
        }
    }

    std::fs::create_dir_all(dir).with_context(|| format!("无法创建目录 {}", dir.display()))?;
    let path = dir.join(format!("sshpf-{}.conf", cfg.id));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("无法写入 {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    // `mode(0o600)` only applies on creation; force it if the file existed.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

fn bake_sections(
    bin: &Path,
    spec: &HostSpec,
    conf_override: Option<&Path>,
    sections: &mut Vec<(String, Vec<String>)>,
    visited: &mut Vec<String>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_JUMP_DEPTH {
        bail!("ProxyJump 链过深");
    }
    let key = spec.match_key();
    if visited.contains(&key) {
        return Ok(());
    }
    visited.push(key.clone());

    let pairs = ssh_g(bin, spec, conf_override).with_context(|| format!("无法解析主机 {} 的配置", spec.match_key()))?;

    // A ProxyCommand cannot be re-stated reliably in a synthetic config; let
    // the caller fall back to the user's real config instead of silently
    // producing a broken tunnel.
    if let Some((_, pc)) = pairs.iter().find(|(k, v)| k == "proxycommand" && !v.is_empty()) {
        bail!("主机 {} 使用 ProxyCommand，无法生成合成配置（{}）", spec.match_key(), pc);
    }

    let mut lines: Vec<String> = Vec::new();
    for (k, v) in &pairs {
        if SYNTH_EXCLUDED.contains(&k.as_str()) {
            continue;
        }
        if k == "hostname" && v == "none" {
            continue;
        }
        if k == "user" && v.is_empty() {
            continue;
        }
        lines.push(format_baked_line(k, v));
    }

    // The baked block must pin the resolved endpoint explicitly: `Host` line
    // matching alone would leave `hostname` at the literal destination string.
    if let Some(hostname) = first_value(&pairs, "hostname").filter(|h| h != "none") {
        ensure_line(&mut lines, "hostname", &format_baked_line("hostname", &hostname));
    }
    if let Some(port) = first_value(&pairs, "port") {
        ensure_line(&mut lines, "port", &format_baked_line("port", &port));
    }
    if let Some(user) = &spec.user {
        ensure_line(&mut lines, "user", &format_baked_line("user", user));
    }

    sections.push((key, lines));

    // `ProxyJump` makes ssh fork a child that re-reads *our* `-F` file, so the
    // whole chain has to be contained in it.
    if let Some(jump) = first_value(&pairs, "proxyjump").filter(|j| j != "none") {
        for hop in jump.split(',') {
            let hop = hop.trim();
            if hop.is_empty() {
                continue;
            }
            let hop_spec = HostSpec::parse(hop);
            bake_sections(bin, &hop_spec, conf_override, sections, visited, depth + 1)?;
        }
    }
    Ok(())
}

fn ensure_line(lines: &mut Vec<String>, key: &str, line: &str) {
    if lines.iter().any(|l| l.split_whitespace().next() == Some(key)) {
        return;
    }
    lines.push(line.to_string());
}

fn format_baked_line(key: &str, value: &str) -> String {
    if value.is_empty() {
        return key.to_string();
    }
    let needs_quote = value.chars().any(char::is_whitespace) && !MULTI_VALUE_KEYS.contains(&key);
    if needs_quote {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{key} \"{escaped}\"")
    } else {
        format!("{key} {value}")
    }
}

// ---------------------------------------------------------------------------
// stderr classification
// ---------------------------------------------------------------------------

/// Terminal cell width of a string (CJK = 2, everything else = 1).
///
/// `unicode-width` is not a dependency and must not be added.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Binary discovery
// ---------------------------------------------------------------------------

/// The `ssh` executable: `$SSHPF_SSH_BIN` when it points at an executable
/// file, otherwise the first `ssh` on `$PATH`, otherwise `/usr/bin/ssh`.
pub fn ssh_binary() -> PathBuf {
    pick_binary(std::env::var_os(SSH_BIN_ENV).as_deref())
}

pub(crate) fn pick_binary(override_path: Option<&OsStr>) -> PathBuf {
    if let Some(path) = override_path {
        let p = PathBuf::from(path);
        if is_executable(&p) {
            return p;
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("ssh");
            if is_executable(&candidate) {
                return candidate;
            }
        }
    }
    PathBuf::from("/usr/bin/ssh")
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Whether `ssh` can actually be run.
pub fn ssh_available() -> bool {
    is_executable(&ssh_binary())
}

// ---------------------------------------------------------------------------
// Local port probing
// ---------------------------------------------------------------------------

/// Can we bind `addr:port` right now? IPv4/IPv6 are tested exactly as given:
/// `127.0.0.1` is not silently widened to the wildcard address.
pub fn port_available(addr: &str, port: u16) -> bool {
    if port == 0 {
        return false;
    }
    match std::net::TcpListener::bind((addr, port)) {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(_) => false,
    }
}

/// First free port at or above `start` (scanning at most 200 candidates).
pub fn suggest_free_port(addr: &str, start: u16) -> Option<u16> {
    let from = start.max(1);
    (from..=u16::MAX).take(200).find(|candidate| port_available(addr, *candidate))
}

/// Best-effort "who is using this port": `pid/comm` from `/proc`.
///
/// Returns `None` when the owner cannot be determined (permissions, kernel
/// thread, socket owned by another user's fd we may not inspect, …). This is a
/// diagnostic nicety, never a correctness dependency.
pub fn port_owner(addr: &str, port: u16) -> Option<String> {
    let _ = addr; // matched by port only: /proc/net/tcp has no name resolution
    let inodes = listening_inodes(port);
    if inodes.is_empty() {
        return None;
    }
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(target) = target.to_str() else {
                continue;
            };
            let Some(rest) = target.strip_prefix("socket:[") else {
                continue;
            };
            let Some(inode) = rest.strip_suffix(']') else {
                continue;
            };
            if inodes.iter().any(|i| i == inode) {
                let comm = std::fs::read_to_string(entry.path().join("comm"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| "?".to_string());
                return Some(format!("{pid}/{comm}"));
            }
        }
    }
    None
}

/// Socket inodes of listening sockets bound to `port`, from `/proc/net/tcp{,6}`.
fn listening_inodes(port: u16) -> Vec<String> {
    let mut out = Vec::new();
    let target = format!("{port:04X}");
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            let Some((_, local_port)) = fields[1].rsplit_once(':') else {
                continue;
            };
            if !local_port.eq_ignore_ascii_case(&target) {
                continue;
            }
            // Prefer LISTEN sockets (state 0A) but accept all: a connected
            // socket on the same local port still points at a culprit.
            let inode = fields[9];
            if inode != "0" {
                out.push(inode.to_string());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FailureKind;
    use crate::ssh::failure::fit_short;
    use crate::ssh::testutil::{FAKE_SSH, write_script};
    use std::path::Path;

    fn tunnel(kind: ForwardKind) -> TunnelConfig {
        TunnelConfig {
            id: 7,
            name: "demo".into(),
            kind,
            ssh_host: "cloud_pc".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 18080,
            remote_host: "127.0.0.1".into(),
            remote_port: 3000,
            enabled: false,
            auto_restart: true,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        }
    }

    // -- classify_failure ---------------------------------------------------

    #[test]
    fn classify_real_stderr_lines() {
        // ① port already in use (both families)
        let f = classify_failure("bind [127.0.0.1]:18081: Address already in use").unwrap();
        assert_eq!(f.kind, FailureKind::PortInUse);
        assert!(f.short.contains("18081"), "{}", f.short);
        assert_eq!(classify_failure("bind [::1]:18081: Address already in use").unwrap().kind, FailureKind::PortInUse);
        // On its own this line is ambiguous (it follows both the in-use and the
        // permission case), so it is `Unknown` and keeps the raw text as `short`.
        let f = classify_failure("channel_setup_fwd_listener_tcpip: cannot listen to port: 18081").unwrap();
        assert_eq!(f.kind, FailureKind::Unknown);
        // `short` keeps the raw text (truncated to the display budget).
        assert!(f.short.starts_with("channel_setup_fwd_listener_tcpip"), "{}", f.short);
        let f = classify_failure("Could not request local forwarding.").unwrap();
        assert_eq!(f.kind, FailureKind::Unknown);
        assert!(f.short.starts_with("Could not request local"), "{}", f.short);
        assert!(display_width(&f.short) <= SHORT_MAX_WIDTH, "{}", display_width(&f.short));

        // ② privileged port
        let f = classify_failure("bind [127.0.0.1]:80: Permission denied").unwrap();
        assert_eq!(f.kind, FailureKind::PermissionDenied);
        assert!(f.short.contains("80"), "{}", f.short);

        // ③ auth failure
        let f = classify_failure("alice@127.0.0.1: Permission denied (publickey).").unwrap();
        assert_eq!(f.kind, FailureKind::AuthFailed);

        // ④ dns / refused
        let f = classify_failure(
            "ssh: Could not resolve hostname no-such-host.invalid: Temporary failure in name resolution",
        )
        .unwrap();
        assert_eq!(f.kind, FailureKind::DnsFailure);
        assert!(f.short.contains("no-such-host.invalid"), "{}", f.short);
        let f = classify_failure("ssh: connect to host 127.0.0.1 port 1: Connection refused").unwrap();
        assert_eq!(f.kind, FailureKind::ConnectionRefused);

        // ⑤ host key
        assert_eq!(
            classify_failure(
                "No ED25519 host key is known for [127.0.0.1]:2222 and you have requested strict checking."
            )
            .unwrap()
            .kind,
            FailureKind::HostKeyMismatch
        );
        assert_eq!(classify_failure("Host key verification failed.").unwrap().kind, FailureKind::HostKeyMismatch);
        assert_eq!(
            classify_failure("@@@@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@@@@").unwrap().kind,
            FailureKind::HostKeyMismatch
        );

        // ⑥ remote forward
        let f = classify_failure("remote forward failure for: listen 22, connect 127.0.0.1:22").unwrap();
        assert_eq!(f.kind, FailureKind::RemoteForwardFailed);
        assert_eq!(
            classify_failure("Error: remote port forwarding failed for listen port 22").unwrap().kind,
            FailureKind::RemoteForwardFailed
        );

        // ⑦ lazy target failure
        let f = classify_failure("channel 2: open failed: connect failed: Connection refused").unwrap();
        assert_eq!(f.kind, FailureKind::TargetUnreachable);

        // ⑧ timeout
        let f = classify_failure("Timeout, server 203.0.113.10 not responding.").unwrap();
        assert_eq!(f.kind, FailureKind::Timeout);
        assert_eq!(
            classify_failure("ssh: connect to host 10.0.0.1 port 22: Connection timed out").unwrap().kind,
            FailureKind::Timeout
        );
        assert_eq!(
            classify_failure("ssh: connect to host h port 22: Operation timed out").unwrap().kind,
            FailureKind::Timeout
        );

        // noise / benign lines are not failures
        assert!(classify_failure("debug1: Entering interactive session.").is_none());
        assert!(classify_failure("Authenticated to 10.0.0.1 ([10.0.0.1]:22).").is_none());
        assert!(classify_failure("").is_none());
    }

    #[test]
    fn benign_agent_socket_refusal_is_not_a_failure() {
        // Measured on OpenSSH 8.2 with no ssh-agent running: this line appears
        // on a healthy session that goes on to forward successfully. Treating
        // it as a failure used to mark (and kill) working tunnels.
        assert!(
            classify_failure("debug1: pubkey_prepare: ssh_get_authentication_socket: Connection refused").is_none()
        );
        assert!(classify_failure("debug1: identity agent: Connection refused").is_none());
        // Genuine refusals must still be classified.
        assert!(classify_failure("ssh: connect to host 127.0.0.1 port 1: Connection refused").is_some());
        assert!(classify_failure("debug1: channel 2: open failed: connect failed: Connection refused").is_some());
    }

    #[test]
    fn classify_short_phrases_fit_width() {
        let cases = [
            "bind [127.0.0.1]:65535: Address already in use",
            "bind [127.0.0.1]:80: Permission denied",
            "ssh: Could not resolve hostname a-very-long-hostname-that-goes-on-and-on.invalid: Name or service not known",
            "channel_setup_fwd_listener_tcpip: cannot listen to port: 65535",
        ];
        for line in cases {
            let info = classify_failure(line).unwrap();
            assert!(
                display_width(&info.short) <= SHORT_MAX_WIDTH,
                "{} => {} ({} cells)",
                line,
                info.short,
                display_width(&info.short)
            );
            assert!(!info.short.contains('\n'));
            assert_eq!(info.detail, line.trim());
        }
    }

    #[test]
    fn display_width_counts_cjk_as_two() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("本地端口"), 8);
        assert_eq!(display_width("a中b"), 4);
        let long = fit_short("端口".repeat(40));
        assert!(display_width(&long) <= SHORT_MAX_WIDTH, "{}", display_width(&long));
        assert!(long.ends_with('…'));
    }

    // -- is_ready_line ------------------------------------------------------

    #[test]
    fn ready_markers_per_kind() {
        assert!(is_ready_line(ForwardKind::Local, "debug1: Entering interactive session."));
        assert!(is_ready_line(ForwardKind::Dynamic, "debug1: Entering interactive session."));
        assert!(!is_ready_line(ForwardKind::Local, "debug1: Authenticated to h ([1.2.3.4]:22)."));
        assert!(is_ready_line(
            ForwardKind::Remote,
            "debug1: remote forward success for: listen 8080, connect 127.0.0.1:80"
        ));
        assert!(is_ready_line(ForwardKind::Remote, "debug1: Entering interactive session."));
        assert!(!is_ready_line(ForwardKind::Remote, "remote forward failure for: listen 22, connect 127.0.0.1:22"));
    }

    #[test]
    fn local_listening_line_is_staged_marker() {
        assert!(is_local_listening_line("debug1: Local forwarding listening on 127.0.0.1 port 18500."));
        assert!(!is_local_listening_line("debug1: Entering interactive session."));
        // Measured with ProxyJump: the forked `ssh -W` child reaches
        // "Entering interactive session." before the target authenticated, so
        // the listener line has to be required first.
        let child_line = "debug1: Entering interactive session.";
        assert!(is_ready_line(crate::model::ForwardKind::Local, child_line));
        assert!(!is_local_listening_line(child_line));
    }

    // -- build_ssh_args -----------------------------------------------------

    #[test]
    fn args_local_forward() {
        let args = build_ssh_args(&tunnel(ForwardKind::Local), None);
        assert!(args.windows(2).any(|w| w == ["-L", "127.0.0.1:18080:127.0.0.1:3000"]));
        assert_eq!(args.last().unwrap(), "cloud_pc");
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"-v".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("LogLevel")), "must not emit LogLevel");
        assert!(args.windows(2).any(|w| w == ["-o", "ExitOnForwardFailure=yes"]));
        assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ConnectTimeout=10"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ServerAliveInterval=15"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ServerAliveCountMax=3"]));
    }

    #[test]
    fn args_remote_and_dynamic() {
        let mut cfg = tunnel(ForwardKind::Remote);
        cfg.remote_host = "0.0.0.0".into();
        cfg.remote_port = 8080;
        let args = build_ssh_args(&cfg, None);
        assert!(args.windows(2).any(|w| w == ["-R", "0.0.0.0:8080:127.0.0.1:18080"]));

        let cfg = tunnel(ForwardKind::Dynamic);
        let args = build_ssh_args(&cfg, None);
        assert!(args.windows(2).any(|w| w == ["-D", "127.0.0.1:18080"]));
        assert!(!args.iter().any(|a| a == "-L" || a == "-R"));
    }

    #[test]
    fn args_synth_config_and_user_options() {
        let mut cfg = tunnel(ForwardKind::Local);
        cfg.ssh_options = vec!["StrictHostKeyChecking=no".into(), "  ".into(), "UserKnownHostsFile=/dev/null".into()];
        let synth = Path::new("/tmp/sshpf-x/sshpf-7.conf");
        let args = build_ssh_args(&cfg, Some(synth));
        let pos = args.iter().position(|a| a == "-F").expect("-F present");
        assert_eq!(args[pos + 1], "/tmp/sshpf-x/sshpf-7.conf");
        assert!(args.windows(2).any(|w| w == ["-o", "StrictHostKeyChecking=no"]));
        assert!(args.windows(2).any(|w| w == ["-o", "UserKnownHostsFile=/dev/null"]));
        // empty option is skipped, no dangling `-o`
        let dangling = args.windows(2).filter(|w| w[0] == "-o").count();
        assert_eq!(dangling, 2 + 5, "5 base + 2 user options");
        assert_eq!(args.last().unwrap(), "cloud_pc");
    }

    // -- host spec parsing --------------------------------------------------

    // -- config parsing / list_hosts ---------------------------------------

    // -- synth config -------------------------------------------------------

    #[test]
    fn synth_config_excludes_forwards_and_bakes_jump_chain() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        let out_dir = dir.path().join("nested").join("synth");
        assert!(!out_dir.exists(), "caller dir may not exist yet");

        let cfg = tunnel(ForwardKind::Local);
        let path = write_synth_config_with(&bin, &cfg, &out_dir, None).unwrap();
        assert_eq!(path.file_name().unwrap(), "sshpf-7.conf");
        assert_eq!(path.parent().unwrap(), out_dir.as_path());

        let text = std::fs::read_to_string(&path).unwrap();
        // resolved target block
        assert!(text.contains("Host cloud_pc"), "{text}");
        assert!(text.contains("hostname 10.0.0.1"), "{text}");
        assert!(text.contains("port 2222"), "{text}");
        assert!(text.contains("user alice"), "{text}");
        // jump chain must be present too, otherwise the forked child cannot
        // resolve `jumpbox` once `-F` replaces the user's config.
        assert!(text.contains("Host jumpbox"), "{text}");
        assert!(text.contains("hostname 10.9.9.9"), "{text}");
        // excluded keys
        assert!(!text.contains("localforward"), "{text}");
        assert!(!text.contains("remoteforward"), "{text}");
        assert!(!text.contains("dynamicforward"), "{text}");
        assert!(!text.contains("clearallforwardings"), "{text}");
        assert!(!text.contains("proxycommand "), "{text}");
        // multi-value line must stay unquoted, single path values are kept
        assert!(text.contains("userknownhostsfile ~/.ssh/known_hosts"), "{text}");

        // 0600
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn synth_config_quotes_paths_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "space-ssh.sh",
            "#!/bin/sh\ncase \"$*\" in\n  *) echo 'hostname 1.2.3.4'; echo 'user u';\
             echo 'port 22'; echo 'identityfile ~/my keys/key with space';;\nesac\n",
        );
        let cfg = tunnel(ForwardKind::Local);
        let path = write_synth_config_with(&script, &cfg, dir.path(), None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("identityfile \"~/my keys/key with space\""), "{text}");
    }

    #[test]
    fn synth_config_never_emits_structural_directives() {
        // A hostile/odd `-G` dump (the project's fake ssh prints a `host …`
        // line) must not leak `Host`/`Match`/`Include` into the baked block:
        // that would open a new host context and drop every following key.
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "weird-ssh.sh",
            "#!/bin/sh\necho 'host -G -T tgt'\necho 'match all'\necho 'include /tmp/x.conf'\n             echo 'hostname 1.2.3.4'\necho 'user u'\necho 'port 22'\n",
        );
        let cfg = tunnel(ForwardKind::Local);
        let path = write_synth_config_with(&script, &cfg, dir.path(), None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let body: Vec<&str> =
            text.lines().filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty()).collect();
        // Exactly one `Host` line — the one we wrote for the block itself.
        assert_eq!(body.iter().filter(|l| l.starts_with("Host ")).count(), 1, "{text}");
        assert!(!text.contains("match all"), "{text}");
        assert!(!text.contains("include /tmp/x.conf"), "{text}");
        assert!(text.contains("hostname 1.2.3.4"), "{text}");
    }

    #[test]
    fn synth_config_rejects_proxycommand_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "pc-ssh.sh",
            "#!/bin/sh\necho 'hostname 5.5.5.5'\necho 'user u'\necho 'port 22'\necho 'proxycommand ssh -W %h:%p jump'\n",
        );
        let cfg = tunnel(ForwardKind::Local);
        let err = write_synth_config_with(&script, &cfg, dir.path(), None).unwrap_err();
        assert!(err.to_string().contains("ProxyCommand"), "{err}");
    }

    #[test]
    fn synth_config_host_port_destination_writes_match_key() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        let mut cfg = tunnel(ForwardKind::Local);
        cfg.ssh_host = "alice@alpha:2222".into();
        let path = write_synth_config_with(&bin, &cfg, dir.path(), None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // ssh matches `user@host:port` destinations against `Host host:port`.
        assert!(text.contains("Host alpha:2222"), "{text}");
        assert!(text.contains("hostname 10.0.0.1"), "{text}");
        assert!(text.contains("port 2222"), "{text}");
    }

    // -- binary discovery ---------------------------------------------------

    #[test]
    fn pick_binary_prefers_override_then_path() {
        let dir = tempfile::tempdir().unwrap();
        let fake = write_script(dir.path(), "my-ssh", "#!/bin/sh\nexit 0\n");
        assert_eq!(pick_binary(Some(fake.as_os_str())), fake);
        // Non-executable override falls back to PATH resolution.
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "not executable").unwrap();
        let picked = pick_binary(Some(plain.as_os_str()));
        assert_ne!(picked, plain);
        assert!(picked.ends_with("ssh") || picked == Path::new("/usr/bin/ssh"));
    }

    // Env-var overrides are process-global, so these two tests are merged into
    // one serial test to stay correct under `cargo test`'s thread pool.

    #[test]
    fn ssh_available_is_true_on_this_machine() {
        // The dev/CI box for this project is Linux with OpenSSH installed.
        assert!(ssh_available(), "ssh should be available: {:?}", ssh_binary());
    }

    // -- ports --------------------------------------------------------------

    #[test]
    fn probe_and_suggest_free_port() {
        // Grab a port from the OS, then make sure it is reported busy and that
        // a free replacement is suggested *above* it. The listener stays bound
        // for the whole test so the assertions cannot race other tests.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        assert!(!port_available("127.0.0.1", busy));

        // `suggest_free_port` scans and then releases, so another test binding
        // the same ephemeral port in between is a legal race: verify by
        // grabbing the suggestion ourselves, retrying the whole scan if we
        // lost it.
        let mut verified = false;
        for _ in 0..10 {
            let free = suggest_free_port("127.0.0.1", busy).expect("some free port above a busy one");
            assert!(free > busy, "suggestion {free} must be above busy {busy}");
            if let Ok(held) = std::net::TcpListener::bind(("127.0.0.1", free)) {
                assert_eq!(held.local_addr().unwrap().port(), free);
                verified = true;
                break;
            }
        }
        assert!(verified, "never managed to hold a suggested port");

        // Port 0 is never a valid forwarding port.
        assert!(!port_available("127.0.0.1", 0));
        // Scanning past the top of the range terminates instead of wrapping or
        // overflowing. This must not depend on whether 65535 happens to be
        // free at this instant (other tests bind ports concurrently), only on
        // the scan not wrapping around to a low port.
        match suggest_free_port("127.0.0.1", u16::MAX) {
            None => {}
            Some(p) => assert_eq!(p, u16::MAX, "scan wrapped around instead of stopping at the top"),
        }
        // Starting at 0 must be clamped to 1, never scan port 0.
        assert_ne!(suggest_free_port("127.0.0.1", 0), Some(0));
    }

    #[test]
    fn port_owner_does_not_panic() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        // Our own process should be identifiable on Linux; tolerate None on
        // exotic setups but never panic.
        if let Some(owner) = port_owner("127.0.0.1", busy) {
            assert!(owner.contains('/'), "{owner}");
        }
        assert_eq!(port_owner("127.0.0.1", 1).is_some(), port_owner("127.0.0.1", 1).is_some());
    }

    // -- misc ---------------------------------------------------------------
}
