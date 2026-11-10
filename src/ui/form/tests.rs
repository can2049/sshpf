//! Unit tests for `form.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::model::InputBuf;
use crate::ui::draw;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::ui::test_support::*;

#[test]
fn renders_form_overlay() {
    let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
    app.open_form_create();
    if let Some(form) = app.form.as_mut() {
        form.ssh_host.set("bastion-prod");
        form.name.set("pg-repo");
        form.local_port.set("5433");
        form.remote_port.set("5432");
        form.port_hint = Some("127.0.0.1:5433 可用".to_string());
    }
    let text = render_to_text(&mut app, 120, 35);
    assert!(text.contains("新建转发"), "form title missing:\n{text}");
    assert!(text.contains("SSH 主机"), "ssh host field missing:\n{text}");
    assert!(text.contains("bastion-prod"), "form value missing:\n{text}");
    assert!(text.contains("类型"), "kind field missing:\n{text}");
    assert!(text.contains("本地转发"), "kind value missing:\n{text}");
    assert!(text.contains("自动重连"), "auto restart field missing:\n{text}");
    assert!(text.contains("可用"), "port hint missing:\n{text}");
    assert!(text.contains("esc 取消"), "form footer missing:\n{text}");
}

/// A cell offset built from typed text must not wrap a `u16`.
///
/// `name`/`ssh_host`/ports come straight out of `config.toml`, so a corrupt
/// or generated file can carry a 64K-character value. Editing such a field
/// put the cursor at `inner.x + FORM_VALUE_X + display_width(prefix)`, which
/// overflows u16 and panics in a debug build. The field value itself renders
/// truncated; only the cursor arithmetic was unguarded.
#[test]
fn a_huge_field_value_cannot_overflow_the_form_cursor() {
    let mut app = app_with_tunnels(vec![sample(1, &"n".repeat(65_530), 8080)]);
    app.open_form_edit();
    if let Some(form) = app.form.as_mut() {
        form.field = FormField::Name;
        form.begin_edit();
    }
    let backend = TestBackend::new(120, 40);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &mut app)).expect("editing a 64K-character name must not panic");
}

#[test]
fn form_shows_the_two_stage_editing_hints() {
    let mut app = app_with_tunnels(vec![]);
    app.open_form_create();

    // Navigating: arrows pick a field, enter starts editing. The legend names
    // both stages' keys, and it must not claim to be editing.
    let text = render_to_text(&mut app, 120, 40);
    assert!(text.contains("enter"), "navigating hint missing:\n{text}");
    assert!(text.contains("编辑"), "navigating hint must mention editing:\n{text}");
    assert!(text.contains("ctrl+s"), "save key must be advertised:\n{text}");
    assert!(!text.contains("编辑中"), "must not look like it is editing:\n{text}");

    // Editing: an explicit worded stage marker appears (never colour alone),
    // and the legend switches to the editing-stage keys.
    app.form.as_mut().unwrap().field = FormField::Name;
    app.form.as_mut().unwrap().begin_edit();
    let text = render_to_text(&mut app, 120, 40);
    assert!(text.contains("编辑中"), "editing hint missing:\n{text}");
    assert!(text.contains("Editing"), "editing hint needs its English:\n{text}");
    assert!(text.contains("结束编辑"), "exit-editing hint missing:\n{text}");
    assert!(text.contains("enter/esc"), "the exit-editing binding must be spelled out:\n{text}");
}

#[test]
fn form_renders_fields_in_the_new_order() {
    let mut app = app_with_tunnels(vec![]);
    app.open_form_create();
    let text = render_to_text(&mut app, 120, 40);
    let pos = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle} missing:\n{text}"));
    // Remote target before the local landing port.
    assert!(pos("远端端口") < pos("本地端口"), "remote port must come before local port:\n{text}");
    assert!(pos("远端地址") < pos("远端端口"), "remote host before remote port:\n{text}");
}

#[test]
fn auto_suggested_local_port_is_labelled_as_editable() {
    let mut app = app_with_tunnels(vec![]);
    app.open_form_create();
    {
        let f = app.form.as_mut().unwrap();
        f.remote_port.set("18077");
    }
    app.refresh_local_port_suggestion();
    let text = render_to_text(&mut app, 120, 40);
    assert!(text.contains("可编辑"), "suggestion must say it can be edited:\n{text}");
}

#[test]
fn chosen_host_is_visible_and_not_overwritten_by_the_hint() {
    // Regression: appending "(N台可选)" unconditionally pushed the picked
    // alias out of the field, so the user could not confirm the choice.
    let mut app = app_with_tunnels(vec![]);
    app.hosts = (0..12)
        .map(|i| crate::ssh::SshHost {
            alias: format!("host-{i:02}"),
            hostname: "1.1.1.1".into(),
            user: None,
            port: 22,
            proxy_jump: Vec::new(),
            identity_file: None,
        })
        .collect();
    app.open_form_create();

    // Empty field: advertise the list.
    let text = render_to_text(&mut app, 120, 40);
    assert!(text.contains("12台可选"), "empty field should hint at the list:\n{text}");

    // Chosen host: the alias must win over the hint.
    app.open_host_picker();
    app.picker_accept();
    let text = render_to_text(&mut app, 120, 40);
    assert!(text.contains("host-00"), "picked alias missing:\n{text}");
    assert!(!text.contains("12台可选"), "hint must not crowd out the value:\n{text}");
}

#[test]
fn renders_form_errors_and_dynamic_kind() {
    let mut app = app_with_tunnels(vec![]);
    app.open_form_create();
    if let Some(form) = app.form.as_mut() {
        form.kind_idx = 2; // Dynamic
        form.errors = vec!["名称不能为空".to_string()];
        form.field = FormField::RemoteHost;
    }
    let text = render_to_text(&mut app, 120, 35);
    assert!(text.contains("动态转发"), "dynamic kind missing:\n{text}");
    assert!(text.contains("SOCKS5"), "dynamic placeholder missing:\n{text}");
    assert!(text.contains("名称不能为空"), "form error missing:\n{text}");
}

#[test]
fn a_terminal_too_short_for_the_pane_says_the_batch_will_still_be_created() {
    // The pane shrinks away on a short terminal, but `import_open` stays true
    // and `submit_form` keys off it: `ctrl+s` creates the ticked rows even
    // though not one of them is on screen. Silence there means creating
    // tunnels the user cannot see, so the form has to say it.
    let mut app = app_with_import(sample_forwards());
    let text = render_to_text(&mut app, 100, 14);
    assert!(!text.contains("从 ~/.ssh/config 导入转发"), "precondition: the pane does not fit:\n{text}");
    assert!(text.contains("导入面板未显示"), "the invisible-batch state must be announced:\n{text}");
    assert!(text.contains("ctrl+s"), "and it must say what ctrl+s will do:\n{text}");

    // Collapsing the pane (which is what the message suggests) makes the
    // warning moot: the form is back in charge and nothing is hidden.
    app.close_import_pane();
    let text = render_to_text(&mut app, 100, 14);
    assert!(!text.contains("导入面板未显示"), "a collapsed pane has nothing hidden:\n{text}");
}

/// The form must show a whole value, not a clipped one.
///
/// This is the user-visible half of the width change, asserted on rendered
/// text rather than on the geometry: the widest thing a user actually types
/// is a remote hostname and a `~/.ssh/config` directive, and at the old fixed
/// 74 columns both were drawn with a trailing `…`. A form that silently
/// truncates the value you are checking is worse than a wider popup.
#[test]
fn the_form_is_wide_enough_to_show_complete_values() {
    let host = "internal-db-prod-01.example.internal";
    let raw = "RemoteForward 9000 127.0.0.1:9001";
    let mut app = app_with_import(vec![fwd(crate::model::ForwardKind::Remote, 9000, "127.0.0.1", 9001, raw)]);
    if let Some(f) = app.form.as_mut() {
        f.ssh_host.set("dev-box");
        f.remote_host.set(host);
        f.remote_port.set("5432");
    }

    // The default terminal: the whole hostname and the whole directive.
    let text = render_to_text(&mut app, 120, 45);
    assert!(text.contains(host), "remote host must not be clipped:\n{text}");
    assert!(text.contains(raw), "the raw directive must not be clipped:\n{text}");

    // A terminal narrower than the popup still wins: the form clamps to the
    // frame and the values truncate, because an overflowing popup is worse.
    let narrow = render_to_text(&mut app, 60, 40);
    assert!(!narrow.contains(raw), "a narrow frame must clamp the popup:\n{narrow}");
    assert!(narrow.contains("新建转发"), "a narrow frame must still show the form:\n{narrow}");
}

#[test]
fn the_import_pane_is_reachable_only_through_the_form() {
    // The pane must never leak into the normal table view.
    let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
    let text = render_to_text(&mut app, 140, 26);
    assert!(!text.contains("导入"), "the pane is a form sub-pane, not table chrome:\n{text}");
}

#[test]
fn form_field_buf_matches_form_state() {
    let mut app = app_with_tunnels(vec![]);
    app.open_form_create();
    let form = app.form.as_ref().unwrap();
    assert_eq!(form_field_buf(form, FormField::Name).map(InputBuf::value), Some(""));
    assert!(form_field_buf(form, FormField::Kind).is_none());
    assert!(form_field_buf(form, FormField::AutoRestart).is_none());
    assert_eq!(form_field_buf(form, FormField::LocalAddr).map(InputBuf::value), Some("127.0.0.1"));
}

// -- import pane --------------------------------------------------------
