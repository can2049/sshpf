//! Persistence: tunnel definitions (user intent) and small runtime state.
//!
//! Layout (XDG compliant, override with `SSHPF_CONFIG`):
//!
//! ```text
//! ~/.config/sshpf/config.toml   definitions, 0600
//! ~/.local/state/sshpf/state.toml  last-run bookkeeping, 0600
//! ```

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::model::{ForwardKind, TunnelConfig, TunnelId, now_unix};

pub const CONFIG_VERSION: u32 = 1;

/// Global defaults, editable by hand.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Defaults {
    /// Default local bind address for new tunnels.
    #[serde(default = "Defaults::default_bind")]
    pub bind: String,
    /// Restore `enabled` tunnels on start-up.
    #[serde(default = "Defaults::yes")]
    pub restore_on_start: bool,
    /// Default value of `auto_restart` for new tunnels.
    #[serde(default = "Defaults::yes")]
    pub auto_restart: bool,
    /// First reconnect delay in seconds (doubles up to `retry_max_secs`).
    #[serde(default = "Defaults::default_retry_initial")]
    pub retry_initial_secs: u64,
    /// Reconnect delay ceiling in seconds.
    #[serde(default = "Defaults::default_retry_max")]
    pub retry_max_secs: u64,
}

impl Defaults {
    fn default_bind() -> String {
        "127.0.0.1".to_string()
    }
    fn yes() -> bool {
        true
    }
    fn default_retry_initial() -> u64 {
        2
    }
    fn default_retry_max() -> u64 {
        30
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            bind: Defaults::default_bind(),
            restore_on_start: true,
            auto_restart: true,
            retry_initial_secs: 2,
            retry_max_secs: 30,
        }
    }
}

/// Serialized form of `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConfigFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, rename = "tunnels")]
    pub tunnels: Vec<TunnelConfig>,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

impl Default for ConfigFile {
    fn default() -> Self {
        ConfigFile { version: CONFIG_VERSION, defaults: Defaults::default(), tunnels: Vec::new() }
    }
}

impl ConfigFile {
    /// Highest id in use, so new tunnels never collide.
    pub fn max_id(&self) -> TunnelId {
        self.tunnels.iter().map(|t| t.id).max().unwrap_or(0)
    }

    pub fn find(&self, id: TunnelId) -> Option<&TunnelConfig> {
        self.tunnels.iter().find(|t| t.id == id)
    }

    pub fn find_mut(&mut self, id: TunnelId) -> Option<&mut TunnelConfig> {
        self.tunnels.iter_mut().find(|t| t.id == id)
    }

    /// A name not already used by another tunnel.
    pub fn unique_name(&self, base: &str, skip: Option<TunnelId>) -> String {
        let taken = |n: &str| self.tunnels.iter().any(|t| t.name == n && Some(t.id) != skip);
        if !taken(base) {
            return base.to_string();
        }
        for i in 2..1000 {
            let candidate = format!("{base}-{i}");
            if !taken(&candidate) {
                return candidate;
            }
        }
        // Suffix with a free id rather than `max_id() + 1`: that addition
        // overflows for a hand-edited `id = 18446744073709551615`, and panics in
        // the debug/CI profile. `next_id` never overflows, so neither do we.
        format!("{base}-{}", self.next_id())
    }

    /// First identifier not used.
    ///
    /// Normally `max_id() + 1`, so ids keep growing and never reuse a deleted
    /// tunnel's id. The addition is *saturating* and the result is checked for
    /// collisions: a hand-edited config can legitimately contain `u64::MAX`
    /// (the config is a text file the user is allowed to edit), and a bare `+ 1`
    /// there panics with "attempt to add with overflow" the first time a tunnel
    /// is created. Falling back to the lowest free id keeps allocation total.
    pub fn next_id(&self) -> TunnelId {
        let next = self.max_id().saturating_add(1);
        // `next == 0` only happens for the empty config, where `max_id()` is 0
        // and the answer is 1; the collision check covers the exhausted-counters
        // case where `max_id() == u64::MAX`.
        if next != 0 && !self.tunnels.iter().any(|t| t.id == next) {
            return next;
        }
        self.first_free_id()
    }

    /// Lowest id that no tunnel uses (there is always one: ids are unique).
    ///
    /// Bounded by the number of tunnels, so this cannot loop forever.
    fn first_free_id(&self) -> TunnelId {
        let taken = |id: TunnelId| self.tunnels.iter().any(|t| t.id == id);
        // At most `len + 1` candidates and at most `len` of them are taken, so
        // one always matches; the fallback is unreachable but must not panic.
        let limit = self.tunnels.len() as u64;
        let mut id = 1;
        while id <= limit.saturating_add(1) {
            if !taken(id) {
                return id;
            }
            id += 1;
        }
        1
    }

    /// Validate cross-entry invariants. Returns human-readable problems.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for t in &self.tunnels {
            if let Err(e) = validate_tunnel(t) {
                problems.push(format!("{}: {e}", t.name));
            }
        }
        // Duplicate listening endpoints among enabled tunnels.
        for (i, a) in self.tunnels.iter().enumerate() {
            if !a.enabled {
                continue;
            }
            for b in self.tunnels.iter().skip(i + 1) {
                if !b.enabled {
                    continue;
                }
                if same_listen_endpoint(a, b) {
                    problems.push(format!("{} 与 {} 监听同一地址 {}", a.name, b.name, a.local_display()));
                }
            }
        }
        problems
    }
}

/// Address a tunnel listens on: local for `-L`/`-D`, remote for `-R`.
pub fn listen_endpoint(cfg: &TunnelConfig) -> (&str, u16) {
    match cfg.kind {
        ForwardKind::Local | ForwardKind::Dynamic => (&cfg.local_addr, cfg.local_port),
        ForwardKind::Remote => (&cfg.remote_host, cfg.remote_port),
    }
}

fn same_listen_endpoint(a: &TunnelConfig, b: &TunnelConfig) -> bool {
    let (aa, ap) = listen_endpoint(a);
    let (ba, bp) = listen_endpoint(b);
    ap == bp && (aa == ba || aa == "0.0.0.0" || ba == "0.0.0.0")
}

/// Field-level validation used by the form and by `load`.
pub fn validate_tunnel(cfg: &TunnelConfig) -> Result<()> {
    if cfg.name.trim().is_empty() {
        bail!("名称不能为空");
    }
    if cfg.ssh_host.trim().is_empty() {
        bail!("SSH 主机不能为空");
    }
    if cfg.ssh_host.chars().any(|c| c.is_whitespace()) {
        bail!("SSH 主机不能包含空格");
    }
    if cfg.local_port == 0 {
        bail!("本地端口不能为 0");
    }
    if cfg.kind != ForwardKind::Dynamic && cfg.remote_port == 0 {
        bail!("远端端口不能为 0");
    }
    if cfg.kind != ForwardKind::Dynamic && cfg.remote_host.trim().is_empty() {
        bail!("远端地址不能为空");
    }
    if cfg.kind != ForwardKind::Remote && cfg.local_addr.trim().is_empty() {
        bail!("本地地址不能为空");
    }
    if cfg.kind == ForwardKind::Remote && cfg.remote_host.trim().is_empty() {
        bail!("远端监听地址不能为空");
    }
    Ok(())
}

/// Resolved config/state file locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    /// `$SSHPF_CONFIG` wins (used by tests); otherwise XDG dirs.
    pub fn resolve() -> Paths {
        if let Some(p) = std::env::var_os("SSHPF_CONFIG") {
            let config = PathBuf::from(p);
            let state = config.with_file_name("state.toml");
            return Paths { config, state };
        }
        let config_base = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".").join(".config")).join("sshpf");
        let state_base = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| PathBuf::from(".").join(".local/state"))
            .join("sshpf");
        Paths { config: config_base.join("config.toml"), state: state_base.join("state.toml") }
    }
}

/// Reveal a tunnel's absolute config path for the UI footer.
pub fn display_path(p: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(stripped) = p.strip_prefix(&home)
    {
        return format!("~/{}", stripped.display());
    }
    p.display().to_string()
}

/// Load `config.toml`, creating an empty default if it does not exist.
///
/// A malformed file is a hard error: we never silently overwrite a config the
/// user may have hand-edited.
pub fn load(path: &Path) -> Result<ConfigFile> {
    if !path.exists() {
        return Ok(ConfigFile::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("读取配置失败: {}", path.display()))?;
    let mut cfg: ConfigFile = toml::from_str(&text).with_context(|| format!("解析配置失败: {}", path.display()))?;
    if cfg.version > CONFIG_VERSION {
        bail!("配置版本 {} 高于本程序支持的 {}，请升级 sshpf", cfg.version, CONFIG_VERSION);
    }
    cfg.version = CONFIG_VERSION;
    // Repair ids/names that collide due to hand editing.
    let mut seen = Vec::new();
    let mut next = 1;
    let mut repaired = 0usize;
    for t in &mut cfg.tunnels {
        let before = t.id;
        while t.id == 0 || seen.contains(&t.id) {
            t.id = next;
            next += 1;
        }
        if t.id != before {
            repaired += 1;
        }
        seen.push(t.id);
        if t.created_at == 0 {
            t.created_at = now_unix();
            repaired += 1;
        }
    }
    if repaired > 0 {
        // Silent repair is a trap: the user's ids are not what they wrote, and
        // any id mentioned in a bug report would otherwise look wrong.
        crate::logging::warn("config.repaired", &[("path", &path.display().to_string()), ("entries", &repaired)]);
    }
    Ok(cfg)
}

/// Atomic save: write a temp file in the same directory, fsync, rename.
pub fn save(path: &Path, cfg: &ConfigFile) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_dir(dir)?;
    let text = toml::to_string_pretty(cfg).context("序列化配置失败")?;
    let tmp = dir.join(format!(".{}.tmp", path.file_name().and_then(|s| s.to_str()).unwrap_or("config")));
    {
        let mut f =
            crate::sys::create_private_file(&tmp).with_context(|| format!("写入配置失败: {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Tighten in case the temp file pre-existed with wider permissions.
    let _ = crate::sys::restrict_file(&tmp);
    fs::rename(&tmp, path).with_context(|| format!("替换配置失败: {}", path.display()))?;
    Ok(())
}

/// Ensure a directory exists with `0700`.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
    }
    let _ = crate::sys::restrict_dir(dir);
    Ok(())
}

/// Warn when the config file is readable by others.
pub fn perms_too_open(path: &Path) -> bool {
    crate::sys::perms_too_open(path)
}

/// Force `0600` on an existing config file.
pub fn tighten_perms(path: &Path) -> Result<()> {
    crate::sys::restrict_file(path).with_context(|| format!("修复权限失败: {}", path.display()))
}

/// Runtime bookkeeping kept out of the user-editable config.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StateFile {
    #[serde(default)]
    pub last_run: i64,
    /// tunnel id -> last observed status key ("up", "failed", ...)
    #[serde(default)]
    pub last_state: std::collections::BTreeMap<String, String>,
}

pub fn load_state(path: &Path) -> StateFile {
    let Ok(text) = fs::read_to_string(path) else {
        return StateFile::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

pub fn save_state(path: &Path, state: &StateFile) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_dir(dir)?;
    let text = toml::to_string_pretty(state).context("序列化状态失败")?;
    let tmp = dir.join(".state.toml.tmp");
    {
        let mut f = crate::sys::create_private_file(&tmp)?;
        f.write_all(text.as_bytes())?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardKind;

    fn tmp_cfg(id: TunnelId, name: &str, port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "host".into(),
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

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = ConfigFile::default();
        cfg.tunnels.push(tmp_cfg(1, "a", 8080));
        cfg.tunnels.push(tmp_cfg(2, "b", 8081));
        save(&path, &cfg).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.tunnels.len(), 2);
        assert_eq!(back.tunnels[1].name, "b");
        assert_eq!(back.defaults.bind, "127.0.0.1");
    }

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(&dir.path().join("nope.toml")).unwrap();
        assert!(cfg.tunnels.is_empty());
    }

    #[test]
    fn malformed_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is not toml [").unwrap();
        assert!(load(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &ConfigFile::default()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!perms_too_open(&path));
    }

    #[test]
    fn duplicate_ids_repaired_and_names_unique() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = ConfigFile::default();
        cfg.tunnels.push(tmp_cfg(0, "a", 8080));
        cfg.tunnels.push(tmp_cfg(0, "a", 8081));
        save(&path, &cfg).unwrap();
        let back = load(&path).unwrap();
        assert_ne!(back.tunnels[0].id, back.tunnels[1].id);
        assert_eq!(back.unique_name("a", None), "a-2");
        assert_eq!(back.next_id(), back.max_id() + 1);
    }

    #[test]
    fn validate_reports_conflicts() {
        let mut cfg = ConfigFile::default();
        let mut a = tmp_cfg(1, "a", 9000);
        let mut b = tmp_cfg(2, "b", 9000);
        a.enabled = true;
        b.enabled = true;
        cfg.tunnels = vec![a, b];
        let problems = cfg.validate();
        assert_eq!(problems.len(), 1, "{problems:?}");

        let mut bad = tmp_cfg(3, "", 1);
        bad.ssh_host = String::new();
        assert!(validate_tunnel(&bad).is_err());
    }

    fn cfg_with(tunnels: Vec<TunnelConfig>) -> ConfigFile {
        ConfigFile { tunnels, ..ConfigFile::default() }
    }

    #[test]
    fn defaults_are_conservative() {
        let d = Defaults::default();
        // Never expose a forwarded port to the network by default.
        assert_eq!(d.bind, "127.0.0.1");
        assert!(d.restore_on_start);
        assert!(d.auto_restart);
        // Backoff starts small and is capped, so one bad host cannot spin.
        assert!(d.retry_initial_secs >= 1);
        assert!(d.retry_max_secs >= d.retry_initial_secs);
    }

    #[test]
    fn missing_optional_fields_get_defaults() {
        // A hand-written config with only the essentials must still load.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[[tunnels]]\nid = 1\nname = \"a\"\nssh_host = \"h\"\nlocal_port = 8080\nremote_port = 22\n")
            .unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.tunnels[0].kind, ForwardKind::Local, "kind defaults to local");
        assert_eq!(cfg.tunnels[0].local_addr, "127.0.0.1", "bind defaults to loopback");
        assert!(!cfg.tunnels[0].enabled);
    }

    #[test]
    fn future_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, format!("version = {}\n", CONFIG_VERSION + 1)).unwrap();
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("版本"), "error should explain the version clash: {err}");
    }

    #[test]
    fn save_round_trips_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut t = tmp_cfg(1, "full", 9000);
        t.kind = ForwardKind::Remote;
        t.remote_host = "0.0.0.0".into();
        t.remote_port = 2200;
        t.enabled = true;
        t.auto_restart = true;
        t.ssh_options = vec!["Compression=yes".into()];
        t.note = "hand written".into();
        save(&path, &cfg_with(vec![t.clone()])).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.tunnels[0], t);
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("nested").join("config.toml");
        save(&path, &ConfigFile::default()).unwrap();
        assert!(path.exists());
        // Directory is tightened to 0700, file to 0600.
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn saves_are_atomic_and_leave_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for i in 0..5 {
            save(&path, &cfg_with(vec![tmp_cfg(i + 1, "t", 9000 + i as u16)])).unwrap();
        }
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
        assert_eq!(load(&path).unwrap().tunnels.len(), 1, "last write wins");
    }

    #[cfg(unix)]
    #[test]
    fn open_permissions_are_detected_and_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &ConfigFile::default()).unwrap();
        assert!(!perms_too_open(&path));

        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(perms_too_open(&path), "world-readable config must be flagged");
        tighten_perms(&path).unwrap();
        assert!(!perms_too_open(&path), "tighten_perms must fix it");
    }

    #[test]
    fn perms_of_a_missing_file_are_not_reported_as_open() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!perms_too_open(&dir.path().join("nope.toml")));
    }

    #[test]
    fn listen_endpoint_tracks_the_forward_direction() {
        // -L and -D bind locally; -R binds on the server.
        let mut l = tmp_cfg(1, "l", 8080);
        l.local_addr = "127.0.0.1".into();
        assert_eq!(listen_endpoint(&l), ("127.0.0.1", 8080));

        let mut d = tmp_cfg(2, "d", 1080);
        d.kind = ForwardKind::Dynamic;
        assert_eq!(listen_endpoint(&d), ("127.0.0.1", 1080));

        let mut r = tmp_cfg(3, "r", 8080);
        r.kind = ForwardKind::Remote;
        r.remote_host = "0.0.0.0".into();
        r.remote_port = 2222;
        assert_eq!(listen_endpoint(&r), ("0.0.0.0", 2222));
    }

    #[test]
    fn validate_tunnel_rejects_each_bad_field() {
        // Each case breaks exactly one field and must be rejected.
        type Break = fn(&mut TunnelConfig);
        let cases: [(&str, Break); 8] = [
            ("empty name", |t: &mut TunnelConfig| t.name = String::new()),
            ("blank name", |t: &mut TunnelConfig| t.name = "   ".into()),
            ("empty host", |t: &mut TunnelConfig| t.ssh_host = String::new()),
            ("host with space", |t: &mut TunnelConfig| t.ssh_host = "a b".into()),
            ("zero local port", |t: &mut TunnelConfig| t.local_port = 0),
            ("zero remote port", |t: &mut TunnelConfig| t.remote_port = 0),
            ("empty remote host", |t: &mut TunnelConfig| t.remote_host = String::new()),
            ("empty local addr", |t: &mut TunnelConfig| t.local_addr = String::new()),
        ];
        for (label, mutate) in cases {
            let mut t = tmp_cfg(1, "ok", 8080);
            mutate(&mut t);
            assert!(validate_tunnel(&t).is_err(), "{label} should be rejected");
        }
        // The untouched baseline must pass.
        assert!(validate_tunnel(&tmp_cfg(1, "ok", 8080)).is_ok());
    }

    #[test]
    fn dynamic_forwards_need_no_remote_target() {
        let mut t = tmp_cfg(1, "socks", 1080);
        t.kind = ForwardKind::Dynamic;
        t.remote_host = String::new();
        t.remote_port = 0;
        assert!(validate_tunnel(&t).is_ok(), "a SOCKS forward has no remote target");
    }

    #[test]
    fn validate_catches_duplicate_listeners_among_enabled() {
        let mut a = tmp_cfg(1, "a", 9000);
        let mut b = tmp_cfg(2, "b", 9000);
        a.enabled = true;
        b.enabled = true;
        let problems = cfg_with(vec![a.clone(), b.clone()]).validate();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains('a') && problems[0].contains('b'));

        // Disabled entries cannot collide, because they do not listen.
        b.enabled = false;
        assert!(cfg_with(vec![a, b]).validate().is_empty());
    }

    #[test]
    fn validate_catches_a_wildcard_binding_overlap() {
        let mut a = tmp_cfg(1, "a", 9000);
        let mut b = tmp_cfg(2, "b", 9000);
        a.enabled = true;
        b.enabled = true;
        a.local_addr = "0.0.0.0".into();
        b.local_addr = "127.0.0.1".into();
        // 0.0.0.0 covers every interface, so these two do collide.
        assert_eq!(cfg_with(vec![a, b]).validate().len(), 1);

        // Different ports never collide.
        let mut c = tmp_cfg(3, "c", 9001);
        c.enabled = true;
        let mut d = tmp_cfg(4, "d", 9002);
        d.enabled = true;
        assert!(cfg_with(vec![c, d]).validate().is_empty());
    }

    #[test]
    fn unique_name_skips_taken_names() {
        let cfg = cfg_with(vec![tmp_cfg(1, "web", 9000), tmp_cfg(2, "web-2", 9001)]);
        assert_eq!(cfg.unique_name("db", None), "db");
        assert_eq!(cfg.unique_name("web", None), "web-3", "must skip both taken names");
        // When updating a tunnel, its own name is not a conflict.
        assert_eq!(cfg.unique_name("web", Some(1)), "web");
    }

    #[test]
    fn find_and_max_id() {
        let mut cfg = cfg_with(vec![tmp_cfg(3, "a", 9000), tmp_cfg(7, "b", 9001)]);
        assert_eq!(cfg.max_id(), 7);
        assert_eq!(cfg.next_id(), 8);
        assert_eq!(cfg.find(7).map(|t| t.name.as_str()), Some("b"));
        assert!(cfg.find(99).is_none());
        cfg.find_mut(3).unwrap().name = "renamed".into();
        assert_eq!(cfg.find(3).map(|t| t.name.as_str()), Some("renamed"));
    }

    #[test]
    fn empty_config_has_no_ids() {
        let cfg = ConfigFile::default();
        assert_eq!(cfg.max_id(), 0);
        assert_eq!(cfg.next_id(), 1, "the first tunnel gets id 1");
    }

    /// A hand-edited `id = 18446744073709551615` used to make `next_id()` panic
    /// with "attempt to add with overflow" in the debug/CI profile, i.e. the
    /// first tunnel the user created crashed the TUI.
    #[test]
    fn next_id_saturates_instead_of_panicking_on_max_id() {
        let mut cfg = cfg_with(vec![tmp_cfg(u64::MAX, "maxed", 9000)]);
        // Must not panic, and must hand back an id nothing else is using.
        let id = cfg.next_id();
        assert_ne!(id, u64::MAX, "the taken id must not be reissued");
        assert!(cfg.find(id).is_none(), "next_id returned the taken id {id}");
        // The config is still usable: allocate a few more and let the invariant
        // (all distinct) carry the weight.
        cfg.tunnels.push(tmp_cfg(id, "new", 9001));
        let id2 = cfg.next_id();
        assert!(id2 != id && cfg.find(id2).is_none(), "second allocation collided: {id2}");
    }

    /// `unique_name` called `max_id() + 1` directly, so it panicked on the same
    /// config once a name was exhausted.
    #[test]
    fn unique_name_does_not_overflow_on_max_id() {
        let mut cfg = cfg_with(vec![tmp_cfg(u64::MAX, "web", 9000)]);
        // Exhaust the `base-2..base-999` candidates so the suffix path is taken.
        for i in 2..1000u64 {
            cfg.tunnels.push(tmp_cfg(i, &format!("web-{i}"), 9000 + (i as u16 % 1000)));
        }
        let name = cfg.unique_name("web", None);
        assert!(name.starts_with("web-"), "unexpected fallback name {name:?}");
        assert!(!cfg.tunnels.iter().any(|t| t.name == name), "fallback name {name:?} already taken");
    }

    #[test]
    fn state_round_trip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        // A missing state file is not an error: it just means "first run".
        let fresh = load_state(&path);
        assert_eq!(fresh.last_run, 0);
        assert!(fresh.last_state.is_empty());

        let st = StateFile { last_run: 1234, last_state: [("3".to_string(), "failed".to_string())].into() };
        save_state(&path, &st).unwrap();
        let back = load_state(&path);
        assert_eq!(back.last_run, 1234);
        assert_eq!(back.last_state.get("3").map(String::as_str), Some("failed"));
    }

    #[test]
    fn corrupt_state_file_degrades_gracefully() {
        // State is diagnostic only; a corrupt one must never block start-up.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        fs::write(&path, "not toml [[[").unwrap();
        let st = load_state(&path);
        assert_eq!(st.last_run, 0);
    }

    #[test]
    fn display_path_shortens_home() {
        if let Some(home) = dirs::home_dir() {
            let p = home.join(".config").join("sshpf").join("config.toml");
            let shown = display_path(&p);
            assert!(shown.starts_with("~/"), "expected ~ shorthand, got {shown}");
        }
        // Paths outside HOME are shown verbatim.
        assert_eq!(display_path(Path::new("/var/empty/config.toml")), "/var/empty/config.toml");
    }

    #[test]
    fn env_override_paths() {
        // Not parallel-safe with other env-mutating tests, so just check shape.
        let p = Paths::resolve();
        assert!(p.config.ends_with("config.toml"));
        assert!(p.state.ends_with("state.toml"));
    }

    #[test]
    fn state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let s = StateFile { last_run: 42, last_state: [("1".to_string(), "up".to_string())].into() };
        save_state(&path, &s).unwrap();
        let back = load_state(&path);
        assert_eq!(back.last_run, 42);
        assert_eq!(back.last_state.get("1").map(String::as_str), Some("up"));
    }
}
