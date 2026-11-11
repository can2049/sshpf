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

    /// A stable, data-only identity for *the forward the user asked for*.
    ///
    /// Two configs with the same identity are the same intent, even when the
    /// environment has since moved one of their ports. That case is why this
    /// function exists: importing `LocalForward 18789 127.0.0.1:18789` while a
    /// tunnel already listens on 18789 makes `plan_local_port` bump the local port
    /// to 18790, and creating that second entry leaves two rows for one forward.
    /// The bump is not even necessarily recent — a bumped port is persisted, so
    /// `config.toml` may hold 18790 for a directive that asked for 18789 and the
    /// original request is gone. Comparing local ports would therefore miss the
    /// duplicate exactly when the bump happened on an earlier run.
    ///
    /// Which ends count as intent depends on the kind:
    ///
    /// * `-L` → `L|ssh_host|local_addr|remote_host:remote_port` — the local
    ///   **port** is deliberately absent: it is a derived value the environment
    ///   may rewrite (and persist rewritten), so including it would make the very
    ///   same forward look new exactly when the port was bumped.
    /// * `-R` → `R|ssh_host|remote_host:remote_port|local_addr:local_port` — both
    ///   ends are intent: the peer-side listen endpoint (what other machines
    ///   dial) and our local target endpoint.
    /// * `-D` → `D|ssh_host|local_addr:local_port` — only the local listen
    ///   endpoint; a SOCKS proxy has no target.
    ///
    /// This is a pure data identity: it does no I/O, never probes a port and only
    /// serialises fields. A *listen-port* conflict is a different question,
    /// answered by [`TunnelConfig::listen_overlap`].
    pub fn forward_identity(&self) -> String {
        identity_of(self.kind, &self.ssh_host, &self.local_addr, self.local_port, &self.remote_host, self.remote_port)
    }

    /// Whether the *listening* end of this forward needs `root` on Unix.
    ///
    /// Ports `0..=1023` are privileged on Unix — binding one needs root or
    /// `CAP_NET_BIND_SERVICE`, which is why the form and the importer steer new
    /// forwards to `>= 1024` (see [`crate::model::plan_local_port`]).
    ///
    /// **Windows has no such rule**: an ordinary user binds port 80 directly, so
    /// this predicate is asking a Unix question on a platform where the answer
    /// never applies. It is kept platform-neutral because the value it feeds —
    /// "would this collide with something a normal user cannot take?" — is still
    /// the right *heuristic* for choosing an alias, and because making it return
    /// `false` on Windows would need a `cfg` in a leaf data type. Callers that
    /// surface it to the user should not claim "needs root" on Windows.
    pub fn uses_privileged_port(&self) -> bool {
        let listen_port = match self.kind {
            ForwardKind::Local | ForwardKind::Dynamic => self.local_port,
            ForwardKind::Remote => self.remote_port,
        };
        listen_port <= 1023
    }

    /// The endpoint this forward *listens* on, per direction.
    ///
    /// `-L`/`-D` bind locally, `-R` binds on the server. Both the pre-flight
    /// bind check and the duplicate-listen guard ask this question, so the
    /// answer lives next to the definition it reads.
    pub fn listen_endpoint(&self) -> (&str, u16) {
        match self.kind {
            ForwardKind::Local | ForwardKind::Dynamic => (&self.local_addr, self.local_port),
            ForwardKind::Remote => (&self.remote_host, self.remote_port),
        }
    }

    /// Whether these two forwards contend for the same listening socket: the
    /// same port, and either the same bind address or a wildcard (`0.0.0.0`)
    /// on one side.
    ///
    /// Three call sites need this same answer — config validation, the form's
    /// live "already in use" hint, and the importer's per-row check — so it is
    /// defined once. A second spelling of the wildcard test is how one of them
    /// silently starts disagreeing about `0.0.0.0`.
    pub fn listen_overlap(&self, other: &TunnelConfig) -> bool {
        let (aa, ap) = self.listen_endpoint();
        let (ba, bp) = other.listen_endpoint();
        ap == bp && (aa == ba || aa == "0.0.0.0" || ba == "0.0.0.0")
    }
}

/// The one identity recipe, usable on inputs that are not a `TunnelConfig` yet.
///
/// Two production callers, and they must agree or the duplicate guard is
/// meaningless:
///
/// * [`TunnelConfig::forward_identity`] — a saved or freshly built definition;
/// * `model::import::SshForward::forward_identity` — a *discovered* directive,
///   which must answer "is this already mine?" **without** planning a local port.
///   Planning probes sockets and may bump the number, so two runs could answer
///   differently and the answer would depend on what happens to be listening.
///
/// A detector that disagrees with the duplicate *definition* is worse than none,
/// which is why the formatting lives here rather than being written twice.
///
/// Trimmed: `~/.ssh/config` values are whitespace-sensitive in parsing but not in
/// meaning, and a hand-edited `config.toml` can carry a trailing space.
pub fn identity_of(
    kind: ForwardKind,
    ssh_host: &str,
    local_addr: &str,
    local_port: u16,
    remote_host: &str,
    remote_port: u16,
) -> String {
    let ssh_host = ssh_host.trim();
    let local_addr = local_addr.trim();
    let remote_host = remote_host.trim();
    match kind {
        ForwardKind::Local => format!("L|{ssh_host}|{local_addr}|{remote_host}:{remote_port}"),
        ForwardKind::Remote => format!("R|{ssh_host}|{remote_host}:{remote_port}|{local_addr}:{local_port}"),
        ForwardKind::Dynamic => format!("D|{ssh_host}|{local_addr}:{local_port}"),
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
    fn local_identity_ignores_the_bumpable_local_port() {
        // The real-world case: the same `LocalForward` re-imported while the local
        // port is taken gets a bumped local port. Same intent, same identity.
        let a = sample(ForwardKind::Local);
        let mut b = a.clone();
        b.id = 2;
        b.name = "another-name".into();
        b.local_port = a.local_port + 1;
        assert_eq!(a.forward_identity(), b.forward_identity());

        // The remote side *is* intent: a different remote port is another forward.
        let mut c = a.clone();
        c.remote_port = a.remote_port + 1;
        assert_ne!(a.forward_identity(), c.forward_identity());

        // So is the SSH host, and the local address the user binds.
        let mut d = a.clone();
        d.ssh_host = "other-host".into();
        assert_ne!(a.forward_identity(), d.forward_identity());
        let mut e = a.clone();
        e.local_addr = "0.0.0.0".into();
        assert_ne!(a.forward_identity(), e.forward_identity());
    }

    #[test]
    fn dynamic_identity_is_the_local_listener_only() {
        let a = sample(ForwardKind::Dynamic);
        let mut b = a.clone();
        // `-D` has no target, so the remote fields are noise.
        b.remote_host = "10.9.9.9".into();
        b.remote_port = 1234;
        assert_eq!(a.forward_identity(), b.forward_identity());

        let mut c = a.clone();
        c.local_port = a.local_port + 1;
        assert_ne!(a.forward_identity(), c.forward_identity(), "the SOCKS listener is the intent");
    }

    #[test]
    fn remote_identity_counts_both_ends() {
        let a = sample(ForwardKind::Remote);
        // The peer-side listener is intent …
        let mut b = a.clone();
        b.remote_port = a.remote_port + 1;
        assert_ne!(a.forward_identity(), b.forward_identity());
        // … and so is the local target we forward back to.
        let mut c = a.clone();
        c.local_port = a.local_port + 1;
        assert_ne!(a.forward_identity(), c.forward_identity());
    }

    #[test]
    fn identity_trims_whitespace_and_ignores_names_and_ids() {
        let a = sample(ForwardKind::Local);
        let mut b = a.clone();
        b.id = 42;
        b.name = "renamed".into();
        b.note = "note".into();
        b.enabled = true;
        let mut padded = b.clone();
        padded.ssh_host = format!("  {}  ", b.ssh_host);
        padded.local_addr = format!(" {} ", b.local_addr);
        padded.remote_host = format!("\t{}\n", b.remote_host);
        assert_eq!(a.forward_identity(), padded.forward_identity());
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
    fn listen_endpoint_tracks_the_forward_direction() {
        // -L and -D bind locally; -R binds on the server.
        let mut local = sample(ForwardKind::Local);
        local.local_addr = "127.0.0.1".into();
        assert_eq!(local.listen_endpoint(), ("127.0.0.1", local.local_port));

        let dynamic = sample(ForwardKind::Dynamic);
        assert_eq!(dynamic.listen_endpoint(), (&*dynamic.local_addr, dynamic.local_port));

        let mut remote = sample(ForwardKind::Remote);
        remote.remote_host = "0.0.0.0".into();
        remote.remote_port = 2222;
        assert_eq!(remote.listen_endpoint(), ("0.0.0.0", 2222));
    }

    #[test]
    fn listen_overlap_needs_the_same_port_and_a_shared_or_wildcard_address() {
        let mut a = sample(ForwardKind::Local);
        a.local_addr = "127.0.0.1".into();
        a.local_port = 8080;

        // Same address + same port contend.
        let mut same = a.clone();
        assert!(a.listen_overlap(&same));

        // A different address on the same port does not.
        same.local_addr = "127.0.0.1".into();
        same.local_port = 8081;
        assert!(!a.listen_overlap(&same));

        // A wildcard on either side does, which is the case a naive
        // `addr == addr` comparison gets wrong.
        let mut wildcard = a.clone();
        wildcard.local_addr = "0.0.0.0".into();
        assert!(a.listen_overlap(&wildcard));
        assert!(wildcard.listen_overlap(&a));

        let mut other_wildcard = a.clone();
        other_wildcard.local_port = 8081;
        other_wildcard.local_addr = "0.0.0.0".into();
        assert!(!a.listen_overlap(&other_wildcard), "a different port never contends");
    }
}
