//! Shared fixtures for the UI layer's unit tests.
//!
//! Every renderer test needs the same three things: a real [`App`] with a temp
//! config file, a `TestBackend` frame flattened back to text, and a couple of
//! sample tunnels/forwards. They live here once rather than in each module's
//! `mod tests`, so a new panel's tests start from the same fixtures and a change
//! to `TunnelConfig` only has to be chased in one place.
//!
//! This file is compiled **only** under `#[cfg(test)]` (see the `mod` gate in
//! [`crate::ui`]), which is also why the helpers may `unwrap`: a failed fixture is
//! a broken test, not a runtime failure.

#[cfg(test)]
pub(crate) use fixtures::*;

#[cfg(test)]
mod fixtures {
    use crate::app::App;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{ForwardKind, ImportTable, SshForward, TunnelConfig, TunnelId};
    use crate::text::display_width;
    use crate::ui::draw;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    pub(crate) fn test_paths(dir: &std::path::Path) -> Paths {
        Paths { config: dir.join("config.toml"), state: dir.join("state.toml") }
    }

    pub(crate) fn sample(id: TunnelId, name: &str, port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "dev-box".into(),
            local_addr: "127.0.0.1".into(),
            local_port: port,
            remote_host: "127.0.0.1".into(),
            remote_port: port,
            enabled: false,
            auto_restart: true,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1_700_000_000,
        }
    }

    pub(crate) fn app_with_tunnels(tunnels: Vec<TunnelConfig>) -> App {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
        crate::config::save(&paths.config, &cfg).unwrap();
        // `App::new` never spawns a process, so this is safe in a unit test.
        App::new(paths)
    }

    /// Render one frame and flatten the buffer to text.
    ///
    /// ratatui stores a double-width glyph in its leading cell only and resets
    /// the following cell (which then reads back as a plain space); skipping
    /// those continuation cells keeps CJK words contiguous, so
    /// `contains("已连通")` means what it looks like.
    pub(crate) fn render_to_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut skip = 0u16;
            for x in 0..buf.area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let symbol = buf[(x, y)].symbol();
                out.push_str(symbol);
                skip = (display_width(symbol) as u16).saturating_sub(1);
            }
            out.push('\n');
        }
        out
    }

    pub(crate) fn fwd(kind: ForwardKind, listen: u16, target: &str, target_port: u16, raw: &str) -> SshForward {
        SshForward {
            kind,
            listen_addr: String::new(),
            listen_port: listen,
            target_host: target.to_string(),
            target_port,
            host_alias: "dev-box".to_string(),
            line: 12,
            raw: raw.to_string(),
        }
    }

    /// An app with a create form whose SSH HOST is set and whose import table
    /// holds `rows`, pane open.
    pub(crate) fn app_with_import(rows: Vec<SshForward>) -> App {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        app.hosts = vec![crate::ssh::SshHost {
            alias: "dev-box".into(),
            hostname: "10.0.0.9".into(),
            user: None,
            port: 22,
            proxy_jump: Vec::new(),
            identity_file: None,
        }];
        if let Some(f) = app.form.as_mut() {
            f.ssh_host.set("dev-box");
            f.set_import_table("dev-box", ImportTable::new(rows));
            f.import_open = true;
        }
        app
    }

    pub(crate) fn sample_forwards() -> Vec<SshForward> {
        vec![
            fwd(ForwardKind::Local, 8080, "127.0.0.1", 80, "LocalForward 8080 127.0.0.1:80"),
            fwd(ForwardKind::Remote, 9000, "127.0.0.1", 9001, "RemoteForward 9000 127.0.0.1:9001"),
            // Un-importable: listen port could not be parsed.
            fwd(ForwardKind::Local, 0, "127.0.0.1", 5432, "LocalForward abc 127.0.0.1:5432"),
        ]
    }
}
