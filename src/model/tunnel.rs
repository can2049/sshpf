//! `ForwardKind` and `TunnelConfig`: what the user asked for, as persisted.
//!
//! This is the definition side of a tunnel — the part that goes to disk and is
//! identical on every run. Live state (status, retries, logs) is `status.rs`;
//! discovery data (`~/.ssh/config`) is `import.rs`.
//!
//! Belongs here: a field of the definition, a per-kind display/spec rule.
//! Does not belong: anything that needs a socket, a clock or a renderer.

use super::TunnelId;

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

    /// English half of [`ForwardKind::label`], for the bilingual UI.
    pub fn label_en(self) -> &'static str {
        match self {
            ForwardKind::Local => "Local -L",
            ForwardKind::Remote => "Remote -R",
            ForwardKind::Dynamic => "Dynamic -D",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fixtures::sample;

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
}
