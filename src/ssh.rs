//! SSH plumbing: host discovery from `~/.ssh/config`, host-key verification,
//! SOCKS5, and the russh client that runs the forwards.
//!
//! Everything here is pure Rust — there is **no** `ssh` subprocess and no
//! `ssh -G` probe any more. That is the whole point of the rewrite: one code
//! path on Linux and Windows, and typed errors instead of parsed stderr.
//!
//! # Layers
//!
//! ```text
//! config       ~/.ssh/config → hosts + effective endpoints (ssh2-config)
//! forwards     ~/.ssh/config → LocalForward/RemoteForward/DynamicForward rows
//! probe        ask the peer which ports are listening (the remote-port picker)
//! known_hosts  server key verification (self-written; russh's has holes)
//! socks5       SOCKS5 server for -D
//! client       connect/auth/ProxyJump + the -L/-R/-D drivers (russh)
//! ```
//!
//! # Public API
//!
//! ```text
//! config::SshHost{alias,hostname,user,port,proxy_jump,identity_file}
//! config::list_hosts() -> Result<Vec<SshHost>>
//! config::resolve_host(alias) -> Result<SshHost>
//! config::Endpoint::resolve(ssh_host) -> Result<Endpoint>   // effective values
//!
//! known_hosts::{verify, learn, HostKeyVerdict, HostKeyPolicy}
//! client::{run, classify, Options}
//! probe::{scan_listen_ports, list_command, interpret, ScanOutcome}
//!
//! fn port_available(addr, port) -> bool
//! fn suggest_free_port(addr, start) -> Option<u16>
//! fn port_owner(addr, port) -> Option<String>
//! ```
//!
//! # Environment overrides
//!
//! * `SSHPF_SSH_CONFIG` — parse only this file instead of `~/.ssh/config`
//!   (tests, and experimentation).
//! * `SSHPF_KNOWN_HOSTS` — use this known_hosts file instead of
//!   `~/.ssh/known_hosts` (tests).

pub mod client;
pub mod config;
pub mod forwards;
pub mod hostkey_conf;
pub mod known_hosts;
pub mod probe;
pub mod socks5;

pub use client::{Options, classify};
pub use config::{Endpoint, SshHost, list_hosts, resolve_host};

/// Ports below this need elevated privileges on Unix; Windows has no such rule.
///
/// This is the named boundary, but the two predicates that implement it —
/// [`crate::model::TunnelConfig::uses_privileged_port`] and
/// [`crate::model::plan_local_port`] — cannot *reference* it: `model` is a leaf
/// layer and `ssh` sits above it, so the dependency would point backwards. They
/// therefore spell `1023` / `1024` inline. Keep the three in step; the off-by-one
/// (`<= 1023` vs `>= 1024`) is exactly what two hand-written copies get wrong.
pub const PRIVILEGED_PORT_MAX: u16 = 1023;

/// Max display width of the short failure phrase shown in the table.
pub const SHORT_MAX_WIDTH: usize = 34;

/// True when `addr:port` can be bound right now.
///
/// Used for the pre-flight check and the form's live hint. Note this is a
/// point-in-time probe: another process can take the port between the check and
/// the bind, which is why the bind itself is still reported as an error.
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

/// First free port at or above `start`, scanning at most 200 candidates.
pub fn suggest_free_port(addr: &str, start: u16) -> Option<u16> {
    let from = start.max(1);
    (from..=u16::MAX).take(200).find(|candidate| port_available(addr, *candidate))
}

/// Best-effort identification of the process holding a listening port.
///
/// Linux-only (`/proc`); returns `None` elsewhere rather than failing.
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

/// Socket inodes listening on `port`, from `/proc/net/tcp{,6}`.
#[cfg(target_os = "linux")]
fn listening_inodes(port: u16) -> Vec<String> {
    let mut out = Vec::new();
    // State 0A is TCP_LISTEN.
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 {
                continue;
            }
            let Some((_, port_hex)) = cols[1].rsplit_once(':') else {
                continue;
            };
            if u16::from_str_radix(port_hex, 16).ok() != Some(port) {
                continue;
            }
            if cols[3] != "0A" {
                continue;
            }
            out.push(cols[9].to_string());
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn listening_inodes(_port: u16) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_zero_is_never_available() {
        assert!(!port_available("127.0.0.1", 0));
    }

    #[test]
    fn a_bound_port_is_not_available() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_available("127.0.0.1", port));
        drop(listener);
        assert!(port_available("127.0.0.1", port));
    }

    #[test]
    fn suggest_free_port_skips_a_taken_port() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = listener.local_addr().unwrap().port();
        let got = suggest_free_port("127.0.0.1", taken).expect("should find a free port above");
        assert_ne!(got, taken);
        assert!(got > taken, "suggested port must not wrap: {got} <= {taken}");
    }

    #[test]
    fn suggest_free_port_returns_the_start_when_free() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let free = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(suggest_free_port("127.0.0.1", free), Some(free));
    }

    #[test]
    fn port_owner_is_none_for_a_closed_port() {
        // Nothing listens on a just-released ephemeral port.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(port_owner("127.0.0.1", port).is_none());
    }
}
