//! Test fixtures shared by the `model` unit tests.
//!
//! Test-only: [`super`] declares this module under `#[cfg(test)]`, so nothing
//! here reaches a release build. It exists so a sample `TunnelConfig`, a sample
//! `SshForward` and the four-row import table are built one way for every test
//! module that needs them — the alternative was the same 40 lines of fixture
//! copied into three files, which is how two fixtures drift apart.
//!
//! Not a home for new behaviour: no assertions beyond the ones the tests make.

use super::import::{ImportTable, SshForward};
use super::tunnel::{ForwardKind, TunnelConfig};

/// A `TunnelConfig` with obvious values, varying only by [`ForwardKind`].
pub(crate) fn sample(kind: ForwardKind) -> TunnelConfig {
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

/// A discovered forward on `dev-box`, line 7, addressed on loopback.
pub(crate) fn fwd(kind: ForwardKind, listen: u16, target: (&str, u16)) -> SshForward {
    SshForward {
        kind,
        listen_addr: "127.0.0.1".into(),
        listen_port: listen,
        target_host: target.0.into(),
        target_port: target.1,
        host_alias: "dev-box".into(),
        line: 7,
        raw: format!("{:?}Forward {listen} {}:{}", kind, target.0, target.1),
    }
}

/// The four-row table the import tests navigate: two `-L`, one `-R`, one `-D`.
pub(crate) fn sample_table() -> ImportTable {
    ImportTable::new(vec![
        fwd(ForwardKind::Local, 8080, ("10.0.0.1", 80)),
        fwd(ForwardKind::Local, 9090, ("10.0.0.2", 90)),
        fwd(ForwardKind::Remote, 7000, ("127.0.0.1", 22)),
        fwd(ForwardKind::Dynamic, 1080, ("", 0)),
    ])
}
