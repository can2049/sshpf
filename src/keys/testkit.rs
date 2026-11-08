//! Shared fixtures for the `keys::*` unit tests. **Test-only, no production code.**
//!
//! Every submodule's `mod tests` imports this, so `app_with` (a temp config dir
//! plus an `App`), `tunnel`, the key constructors (`key`, `ch`, `ctrl`, `mouse`)
//! and the scenario builders (`form_app`, `import_app`,
//! `default_open_import_app`, `three_app`) are defined exactly once. Duplicating
//! them per handler file would make six copies drift apart.
//!
//! The `#[cfg(test)]` on each item is what keeps `tests/safety_guards.rs` honest:
//! it masks these bodies (which `.unwrap()` deliberately — a failed setup must
//! fail loudly) out of the "no panicking constructs in production" scan.

use crate::app::App;
use crate::config::{self, ConfigFile, Paths};
use crate::keys::handle_key;
use crate::model::{ForwardKind, TunnelConfig};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(test)]
/// A key event with no modifiers, as a terminal sends for a plain press.
pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[cfg(test)]
pub(super) fn mouse(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
}

#[cfg(test)]
pub(super) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[cfg(test)]
pub(super) fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

#[cfg(test)]
pub(super) fn tunnel(id: u64, name: &str, port: u16) -> TunnelConfig {
    TunnelConfig {
        id,
        name: name.into(),
        kind: ForwardKind::Local,
        ssh_host: "testhost".into(),
        local_addr: "127.0.0.1".into(),
        local_port: port,
        remote_host: "127.0.0.1".into(),
        remote_port: 22,
        enabled: false,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    }
}

#[cfg(test)]
/// Build an `App` from a temp config. `App::new` spawns nothing.
pub(super) fn app_with(tunnels: Vec<TunnelConfig>) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
    config::save(&paths.config, &cfg).unwrap();
    (dir, App::new(paths))
}

#[cfg(test)]
/// Open a create form on an app with one known host.
pub(super) fn form_app() -> (tempfile::TempDir, App) {
    let (d, mut app) = app_with(vec![]);
    app.hosts = vec![crate::ssh::SshHost {
        alias: "testhost".into(),
        hostname: "127.0.0.1".into(),
        user: Some("u".into()),
        port: 22,
        proxy_jump: Vec::new(),
        identity_file: None,
    }];
    handle_key(&mut app, ch('n'));
    (d, app)
}

#[cfg(test)]
/// A create form whose import pane already holds three forwards.
pub(super) fn import_app() -> (tempfile::TempDir, App) {
    let (d, mut app) = app_with(vec![]);
    app.open_form_create();
    let forwards = (0..3)
        .map(|i| crate::model::SshForward {
            kind: crate::model::ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port: 18001 + i,
            target_host: "10.0.0.1".into(),
            target_port: 80,
            host_alias: "dev-box".into(),
            line: (i as usize) + 1,
            raw: format!("LocalForward {} 10.0.0.1:80", 18001 + i),
        })
        .collect();
    {
        let f = app.form.as_mut().unwrap();
        f.ssh_host.set("dev-box");
        f.set_import_table("dev-box", crate::model::ImportTable::new(forwards));
        f.import_open = true;
    }
    (d, app)
}

#[cfg(test)]
/// A create form with a host and a *default-open* pane holding three rows.
pub(super) fn default_open_import_app() -> (tempfile::TempDir, App) {
    let (d, mut app) = form_app();
    let forwards = (0..3)
        .map(|i| crate::model::SshForward {
            kind: crate::model::ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port: 19001 + i,
            target_host: "10.0.0.1".into(),
            target_port: 80,
            host_alias: "testhost".into(),
            line: (i as usize) + 1,
            raw: format!("LocalForward {} 10.0.0.1:80", 19001 + i),
        })
        .collect();
    {
        let f = app.form.as_mut().unwrap();
        f.ssh_host.set("testhost");
        f.set_import_table("testhost", crate::model::ImportTable::new(forwards));
        f.import_open = true;
    }
    (d, app)
}

#[cfg(test)]
pub(super) fn three_app() -> (tempfile::TempDir, App) {
    app_with(vec![tunnel(1, "a", 18001), tunnel(2, "b", 18002), tunnel(3, "c", 18003)])
}
