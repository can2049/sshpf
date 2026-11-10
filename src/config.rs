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

/// Shortest `defaults.health_check_secs` that is actually useful.
///
/// A probe is one round trip for the whole session, so even one second would
/// not hurt locally — but on a loaded link a sub-second-triggered flood of
/// probes is indistinguishable from a broken tunnel. Five seconds is well below
/// any human-noticeable recovery time and above any plausible probe latency.
pub const MIN_HEALTH_CHECK_SECS: u64 = 5;

/// Longest `defaults.health_check_secs` the app will honour (one day).
///
/// Beyond this the check no longer answers "did the session die?", because a
/// resumed laptop has already been unusable for hours.
pub const MAX_HEALTH_CHECK_SECS: u64 = 24 * 60 * 60;

/// Coerce a hand-written `health_check_secs` into the range the app operates in.
///
/// `0` is a meaningful value — "the user turned the check off" — so it passes
/// through untouched rather than being clamped up to [`MIN_HEALTH_CHECK_SECS`],
/// which would silently re-enable a feature the user disabled.
pub fn normalize_health_check_secs(secs: u64) -> u64 {
    if secs == 0 { 0 } else { secs.clamp(MIN_HEALTH_CHECK_SECS, MAX_HEALTH_CHECK_SECS) }
}

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
    /// How often an `Up` tunnel probes the SSH session for liveness, in
    /// seconds. `0` disables the check.
    ///
    /// A local `-L`/`-D` listener never learns that the SSH session died — the
    /// `accept()` loop keeps returning connections that then fail, so the
    /// status stays `●` while nothing forwards. That is what a Windows machine
    /// looks like after sleep/resume, and it is why the probe exists at all.
    #[serde(default = "Defaults::default_health_check")]
    pub health_check_secs: u64,
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
    fn default_health_check() -> u64 {
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
            health_check_secs: Defaults::default_health_check(),
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
    load_with_report(path).map(|(cfg, _)| cfg)
}

/// [`load`], plus what the duplicate repair did to the file.
///
/// The report exists because a folded entry is a *visible* change to the user's
/// data: their table is shorter than what `config.toml` said. The log records it
/// either way, but a start-up that quietly loses two rows is the kind of silence
/// that gets read as a bug in the other direction ("sshpf forgot my forwards"), so
/// the caller can put it on the status bar too.
pub fn load_with_report(path: &Path) -> Result<(ConfigFile, Vec<crate::model::Folded>)> {
    if !path.exists() {
        return Ok((ConfigFile::default(), Vec::new()));
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
    // Entries that describe the *same forward* fold into one. Done here, on the
    // parsed list, because this is the layer that owns `config.toml`: the table the
    // user sees, the tunnels that get dialled and the duplicate guard all read
    // through here, so one repair fixes every consumer at once. See `model::dedup`
    // for which entry survives.
    let (tunnels, folded) = crate::model::fold_duplicates(cfg.tunnels);
    cfg.tunnels = tunnels;
    for group in &folded {
        crate::eventlog::config_deduped(&group.kept, group.dropped.len());
    }
    Ok((cfg, folded))
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
mod tests;
