//! Unit tests for `config.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

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
        remote_port: port,
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

/// The same forward as `tmp_cfg(.., target)` but listening elsewhere: exactly
/// what a bumped local port produces, and the shape the duplicate repair has to
/// recognise (`-L`'s local port is not identity).
fn tmp_cfg_bumped(id: TunnelId, name: &str, local_port: u16, target_port: u16) -> TunnelConfig {
    TunnelConfig { local_port, ..tmp_cfg(id, name, target_port) }
}

#[test]
fn loading_folds_entries_that_are_the_same_forward() {
    // The user's `config.toml` already held three entries for `cloud_pc`'s
    // 18789 forward (one per historical import, each on a bumped local port).
    // The create-time guard cannot help them — they are already on disk — so
    // the load repairs the list. Without this, the duplicate rows are
    // permanent: the table shows three, three SSH connections are dialled, and
    // deleting two only holds until the next scan.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = ConfigFile::default();
    cfg.tunnels.push(tmp_cfg_bumped(3, "cloud_pc-L-18789-2", 18789, 18789));
    cfg.tunnels.push(tmp_cfg_bumped(5, "cloud_pc-L-18789", 18790, 18789));
    cfg.tunnels.push(tmp_cfg_bumped(7, "cloud_pc-L-18789-3", 18791, 18789));
    save(&path, &cfg).unwrap();

    let back = load(&path).unwrap();
    assert_eq!(back.tunnels.len(), 1, "one intent, one entry");
    assert_eq!(back.tunnels[0].name, "cloud_pc-L-18789-2", "the first row survives");
    assert_eq!(back.tunnels[0].local_port, 18789, "and keeps its persisted port");
}

#[test]
fn a_clean_config_loads_without_a_repair_report() {
    // The common case must be side-effect free, or every start-up would warn.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = ConfigFile::default();
    cfg.tunnels.push(tmp_cfg(1, "a", 8001));
    cfg.tunnels.push(tmp_cfg(2, "b", 8002));
    save(&path, &cfg).unwrap();
    let (back, report) = load_with_report(&path).unwrap();
    assert_eq!(back.tunnels.len(), 2);
    assert!(report.is_empty());
}

#[test]
fn the_repair_report_names_the_survivor_and_the_count() {
    // The status bar says "merged N duplicates of X", so the report has to
    // carry both halves.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = ConfigFile::default();
    cfg.tunnels.push(tmp_cfg_bumped(1, "kept", 8001, 80));
    cfg.tunnels.push(tmp_cfg_bumped(2, "gone-a", 8002, 80));
    cfg.tunnels.push(tmp_cfg_bumped(3, "gone-b", 8003, 80));
    save(&path, &cfg).unwrap();
    let (back, report) = load_with_report(&path).unwrap();
    assert_eq!(back.tunnels.len(), 1);
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].kept, "kept");
    assert_eq!(report[0].dropped.len(), 2);
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
    // The health check is on by default: a tunnel that shows `●` must
    // actually still work, which a local listener alone cannot tell us.
    assert_eq!(d.health_check_secs, 30);
}

#[test]
fn a_health_check_interval_is_only_corrected_when_it_is_actually_unusable() {
    // `0` means "off" and must stay off: clamping it up would re-enable a
    // feature the user deliberately disabled.
    assert_eq!(normalize_health_check_secs(0), 0);
    // Too frequent floods the session; too rare answers a question nobody
    // asks any more.
    assert_eq!(normalize_health_check_secs(1), MIN_HEALTH_CHECK_SECS);
    assert_eq!(normalize_health_check_secs(4), MIN_HEALTH_CHECK_SECS);
    assert_eq!(normalize_health_check_secs(5), 5);
    assert_eq!(normalize_health_check_secs(30), 30);
    assert_eq!(normalize_health_check_secs(MAX_HEALTH_CHECK_SECS), MAX_HEALTH_CHECK_SECS);
    assert_eq!(normalize_health_check_secs(u64::MAX), MAX_HEALTH_CHECK_SECS);
}

#[test]
fn a_health_check_interval_survives_a_config_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = ConfigFile::default();
    cfg.defaults.health_check_secs = 90;
    save(&path, &cfg).unwrap();
    assert_eq!(load(&path).unwrap().defaults.health_check_secs, 90);
}

#[test]
fn an_old_config_without_a_health_check_interval_still_loads() {
    // Every `config.toml` written before this field existed must keep
    // working: the new setting is additive, not a format change.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "[defaults]\nbind = \"127.0.0.1\"\nauto_restart = false\n").unwrap();
    assert_eq!(load(&path).unwrap().defaults.health_check_secs, 30, "the default must fill in");
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
