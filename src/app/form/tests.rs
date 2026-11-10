//! Unit tests for `form.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::model::InputBuf;

fn create_form() -> FormState {
    let mut f = FormState::create("127.0.0.1");
    f.ssh_host.set("host");
    f.name.set("demo");
    f
}

/// A form whose `kind_idx` was written past the end of `ForwardKind::all()`.
///
/// The field is public, so this is not a hypothetical: the UI and tests both
/// assign it directly.
fn form_with_kind_idx(idx: usize) -> FormState {
    let mut f = create_form();
    f.kind_idx = idx;
    f
}

#[test]
fn an_out_of_range_kind_index_degrades_to_the_default_kind() {
    // Regression: this used to be `ForwardKind::all()[self.kind_idx.min(2)]`
    // — a hard-coded `2` that only happened to match the array length. Both
    // `3` (past the end) and a reordered array would have indexed out of
    // bounds, which panics.
    for idx in [3usize, 4, 99, usize::MAX] {
        let f = form_with_kind_idx(idx);
        assert_eq!(f.kind(), ForwardKind::default(), "kind_idx={idx} must degrade, not panic");
    }
    // Valid indices still work.
    assert_eq!(form_with_kind_idx(0).kind(), ForwardKind::Local);
    assert_eq!(form_with_kind_idx(1).kind(), ForwardKind::Remote);
    assert_eq!(form_with_kind_idx(2).kind(), ForwardKind::Dynamic);
}

#[test]
fn cycling_a_kind_from_an_out_of_range_index_comes_back_in_range() {
    // `(i + n - 1) % n` on an out-of-range `i` stays out of range; the cycle
    // must normalise first, so one keypress repairs the form.
    let mut f = form_with_kind_idx(99);
    f.cycle_kind(true);
    assert!(f.kind_idx < ForwardKind::all().len(), "kind_idx still out of range: {}", f.kind_idx);
    assert_eq!(f.kind(), ForwardKind::all()[f.kind_idx]);

    let mut f = form_with_kind_idx(99);
    f.cycle_kind(false);
    assert!(f.kind_idx < ForwardKind::all().len(), "kind_idx still out of range: {}", f.kind_idx);
}

#[test]
fn cycling_the_kind_wraps_in_both_directions() {
    let mut f = form_with_kind_idx(0);
    for expected in [1usize, 2, 0, 1] {
        f.cycle_kind(true);
        assert_eq!(f.kind_idx, expected);
    }
    let mut f = form_with_kind_idx(0);
    for expected in [2usize, 1, 0, 2] {
        f.cycle_kind(false);
        assert_eq!(f.kind_idx, expected);
    }
}

#[test]
fn a_form_with_a_bogus_kind_still_validates_and_saves() {
    // The degraded kind must not smuggle an invalid config through: the
    // default kind needs a remote port, so the validation result is
    // whatever the default kind demands — but it must not panic.
    let mut f = form_with_kind_idx(usize::MAX);
    f.remote_port.set("22");
    f.local_port.set("8080");
    let cfg = f.to_config(1, None).expect("the degraded kind is a real kind");
    assert_eq!(cfg.kind, ForwardKind::default());
    assert_eq!(cfg.remote_port, 22);
}

#[test]
fn editing_a_forward_keeps_a_valid_kind_index() {
    let mut cfg = crate::model::TunnelConfig {
        id: 1,
        name: "x".into(),
        kind: ForwardKind::Dynamic,
        ssh_host: "h".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 1080,
        remote_host: String::new(),
        remote_port: 0,
        enabled: false,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    };
    for kind in ForwardKind::all() {
        cfg.kind = kind;
        let f = FormState::edit(&cfg);
        assert!(f.kind_idx < ForwardKind::all().len());
        assert_eq!(f.kind(), kind);
    }
}

#[test]
fn a_tunnel_with_no_remote_target_does_not_break_the_edit_form() {
    // A `-D` row has empty remote fields; opening it for edit must not
    // produce an index or parse panic, just a valid form.
    let cfg = crate::model::TunnelConfig {
        id: 4,
        name: "socks".into(),
        kind: ForwardKind::Dynamic,
        ssh_host: "h".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 1080,
        remote_host: String::new(),
        remote_port: 0,
        enabled: true,
        auto_restart: true,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    };
    let mut f = FormState::edit(&cfg);
    f.revalidate();
    assert!(f.to_config(4, Some(&cfg)).is_ok());
}

#[test]
fn malformed_port_input_never_reaches_a_config() {
    // Every non-numeric / out-of-range string must be a validation error,
    // not a parse panic or a config with a zero port.
    for bad in ["", " ", "abc", "-1", "0", "65536", "99999999999", "１２３", "1e5"] {
        let mut f = create_form();
        f.local_port.set(bad);
        f.remote_port.set(bad);
        let result = f.to_config(1, None);
        if bad.trim().parse::<u16>().map(|p| p != 0).unwrap_or(false) {
            // Only strings that are genuinely a valid non-zero u16 may pass
            // — none of the samples above are, so this branch is a guard.
            continue;
        }
        assert!(result.is_err(), "input {bad:?} must be rejected, got {result:?}");
    }
}

#[test]
fn port_zero_is_rejected_for_both_ends() {
    let mut f = create_form();
    f.local_port.set("0");
    f.remote_port.set("22");
    assert!(f.to_config(1, None).is_err(), "local port 0 cannot be listened on");

    let mut f = create_form();
    f.local_port.set("8080");
    f.remote_port.set("0");
    assert!(f.to_config(1, None).is_err(), "remote port 0 is not a target");
}

#[test]
fn a_dynamic_forward_tolerates_an_empty_remote_side() {
    let mut f = create_form();
    f.kind_idx = 2;
    f.local_port.set("1080");
    f.remote_port.set("");
    f.remote_host.set("");
    let cfg = f.to_config(1, None).expect("a SOCKS forward needs no remote side");
    assert_eq!(cfg.remote_port, 0);
    assert!(cfg.remote_host.is_empty());
}

#[test]
fn suggesting_a_local_port_terminates_for_every_remote_port() {
    // `plan_local_port` has a bounded scan; this pins that the *form* path
    // (which also probes real sockets) cannot hang or panic for any input.
    for remote in ["0", "1", "22", "1023", "1024", "8080", "65535", "abc"] {
        let mut f = create_form();
        f.remote_port.set(remote);
        f.suggest_local_port(&[]);
        // Whatever it decided, the buffer must stay parseable-or-empty.
        let value = f.local_port.value().to_string();
        assert!(value.is_empty() || value.parse::<u16>().is_ok(), "remote {remote} produced {value:?}");
    }
}

#[test]
fn a_form_with_an_empty_import_table_is_not_mistaken_for_a_batch() {
    let f = create_form();
    assert!(f.import.is_empty());
    assert!(matches!(f.mode, FormMode::Create));
}

#[test]
fn input_buffers_survive_a_degenerate_cursor() {
    // `cursor` is a public field on `InputBuf`; a stale value past the end
    // must clamp rather than slice-panic.
    let mut buf = InputBuf::new("abc");
    buf.cursor = usize::MAX;
    let mut f = create_form();
    f.name = buf;
    f.revalidate();
    // The buffer is not edited here, but `to_config` reads it.
    let _ = f.to_config(1, None);
}
