//! The create/edit form overlay: field rows, their notes and the text cursor.
//!
//! The form is the single place a tunnel is defined. Its two-stage model (arrows
//! move, `enter` edits) is expressed by three visual states per row — plain,
//! accent-bold focused, and a solid bar while text entry is active — so the stage
//! is never conveyed by colour alone.
//!
//! The import pane is *added* to the form's height rather than borrowed from it
//! (a pane that squeezed the fields would hide the values being checked against
//! the rows). When the pane does not fit, the form says the batch will still be
//! created — silence there would mean creating tunnels the user cannot see.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::model::{FormField, ForwardKind};
use crate::text::{display_width, pad_to_width, truncate_to_width};
use crate::ui::dropdown::draw_host_dropdown;
use crate::ui::frame::{centered, clear_and_block, popup_block};
use crate::ui::hints::{Hint, flow_hints};
use crate::ui::hit::HitMap;
use crate::ui::import;
use crate::ui::layout::{FORM_POPUP_W, form_label_w, form_value_x};
use crate::ui::palette::{Palette, palette};
use crate::ui::text::{fit_pair, fit_pair_spaced, warn_once};

// ---------------------------------------------------------------------------
// Overlay: Form
// ---------------------------------------------------------------------------

/// The text buffer backing a form field, if it is editable.
pub(crate) fn form_field_buf(form: &crate::app::FormState, field: FormField) -> Option<&crate::model::InputBuf> {
    match field {
        FormField::Name => Some(&form.name),
        FormField::SshHost => Some(&form.ssh_host),
        FormField::LocalAddr => Some(&form.local_addr),
        FormField::LocalPort => Some(&form.local_port),
        FormField::RemoteHost => Some(&form.remote_host),
        FormField::RemotePort => Some(&form.remote_port),
        FormField::Kind | FormField::AutoRestart => None,
    }
}

pub(crate) fn draw_form(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(form) = app.form.as_ref() else { return };
    let p = palette();
    // Both languages in the title: this is the first thing that tells a user
    // which of the two form modes they are in.
    let title = if form.is_modifying() {
        format!("编辑转发 · Edit · {}", form.name.value())
    } else {
        "新建转发 · New tunnel".to_string()
    };
    let pane_open = pane_open(app);
    // 110 instead of 74: at 74 the import pane's raw-directive column was clipped
    // (`RemoteForward 9000 12…`), and this is the one screen where a tunnel is
    // defined. `centered` clamps both axes, so a narrow terminal keeps its width.
    let popup = centered(area, FORM_POPUP_W.min(area.width), form_height(pane_open).min(area.height));
    let block = popup_block(&title, p);
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let label_w = form_label_w(inner.width);
    let value_w = inner.width.saturating_sub(form_value_x(inner.width) as u16) as usize;
    let current_idx = FormField::ORDER.iter().position(|f| *f == form.field).unwrap_or(0);

    let lines = form_field_lines(app, form, p, label_w, value_w, current_idx);
    let mut extra = form_notes(form, p, inner.width as usize);
    let (body, pane_area) = split_pane(form, inner, pane_open, lines.len() as u16 + extra.len() as u16, &mut extra, p);

    let used = lines.len() as u16;
    if body.height > used {
        let rest = Rect { x: body.x, y: body.y + used, width: body.width, height: body.height - used };
        frame.render_widget(Paragraph::new(extra), rest);
    }
    frame.render_widget(Paragraph::new(lines), body);

    // Expand the dropdown *inside* the form, anchored under the SSH HOST row.
    // It is confined to the body so it can never cover the import pane.
    if let Some(picker) = app.picker.as_ref().filter(|pk| pk.open) {
        draw_host_dropdown(frame, app, picker, body, current_idx as u16);
    }

    place_form_cursor(frame, form, body, popup, current_idx);

    // Drawn last, and outside `body`: it owns its own cursor (a column filter
    // being typed into), which must win over the form field cursor.
    if let Some(pane) = pane_area {
        import::draw_pane(frame, app, pane, hits);
    }
}

/// Whether the import pane is shown.
///
/// Visibility goes through `App::import_pane_open` so the renderer and the key
/// handler cannot disagree. Two extra cases are the renderer's to own, because
/// `import_pane_open` is deliberately just "is there a table worth showing":
///
/// * discovery *failed* → the table is empty, but the reason is exactly what the
///   user needs, so the pane opens to show it;
/// * `import_open` set on an empty table → the state is reachable (a filter or a
///   host switch can empty it), and silently collapsing would look like the key
///   did nothing. `import::draw_empty` has a branch for each.
///
/// (`import_error` on its own deliberately does *not* force the pane open:
/// `App::toggle_import_pane` reports the failure on the status line and leaves
/// the pane closed, and two places claiming to show the same reason would be
/// worse than one.)
fn pane_open(app: &App) -> bool {
    app.import_pane_open() || app.form.as_ref().is_some_and(|f| f.import_open)
}

/// Height the popup asks for.
///
/// Tall enough to hold the fields *and* the expanded host dropdown: title border
/// (2) + one line per field + errors/port hint (2) + blank (1) + key hint (1) +
/// dropdown frame (2) + dropdown rows. The import pane is *added* on top rather
/// than borrowed from the form, so a pane never squeezes the fields whose values
/// the user is checking against the rows they tick.
fn form_height(pane_open: bool) -> u16 {
    let fields_h = FormField::ORDER.len() as u16;
    let base_h = fields_h + 4 + 2 + crate::app::HOST_DROPDOWN_ROWS as u16 + 2;
    base_h + if pane_open { import::PREF_HEIGHT } else { 0 }
}

/// Split the popup body between the field rows and the import pane.
///
/// The pane is reserved out of the bottom, but only after the fields and their
/// notes have their rows: on a short terminal the pane is what shrinks (and
/// eventually disappears) so the form stays usable. When it cannot fit at all,
/// the warning goes into `extra` — see [`invisible_pane_note`].
fn split_pane(
    form: &crate::app::FormState,
    inner: Rect,
    pane_open: bool,
    needed: u16,
    extra: &mut Vec<Line<'static>>,
    p: Palette,
) -> (Rect, Option<Rect>) {
    if !pane_open {
        return (inner, None);
    }
    let room = inner.height.saturating_sub(needed).min(import::PREF_HEIGHT);
    if room >= import::MIN_HEIGHT {
        let body = Rect { height: inner.height - room, ..inner };
        let pane = Rect { x: inner.x, y: inner.y + body.height, width: inner.width, height: room };
        return (body, Some(pane));
    }
    extra.extend(invisible_pane_note(form, inner.width as usize, p));
    (inner, None)
}

/// The warning for the one genuinely dangerous state: the pane is hidden but
/// `import_open` is still true and the submit path keys off it.
///
/// `ctrl+s` would create the ticked rows while the user cannot see a single one
/// of them. Say so instead of silently acting on an invisible table — and point
/// at the key that gets them out of that state.
fn invisible_pane_note(form: &crate::app::FormState, width: usize, p: Palette) -> Option<Line<'static>> {
    if !form.import_open || form.import.is_empty() {
        return None;
    }
    let msg = format!(
        "⚠ 终端太矮，导入面板未显示；ctrl+s 仍会按已勾选的 {} 条创建。i 收起面板可改用手填",
        form.import.checked_count()
    );
    Some(Line::from(Span::styled(truncate_to_width(&msg, width), p.fg(p.pending))))
}

/// One `Line` per form field, styled for the three highlight states.
///
/// Three visual states make the two-stage model obvious:
///   • highlighted + editing → solid selection bar (text entry active)
///   • highlighted only      → bold accent, no bar (arrows just move)
///   • neither               → plain
pub(crate) fn form_field_lines(
    app: &App,
    form: &crate::app::FormState,
    p: Palette,
    label_w: usize,
    value_w: usize,
    current_idx: usize,
) -> Vec<Line<'static>> {
    let kind = form.kind();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(FormField::ORDER.len());
    for (i, field) in FormField::ORDER.iter().enumerate() {
        let focused = i == current_idx;
        let editing_here = focused && form.is_editing_field();
        let label = Span::styled(
            pad_to_width(&fit_pair(field.label(), field.label_en(), label_w), label_w),
            label_style(editing_here, focused, p),
        );

        if *field == FormField::SshHost {
            lines.push(host_field_line(app, form, p, value_w, editing_here, label));
            continue;
        }
        let value = field_value(form, *field, kind, value_w, focused, editing_here, p);
        lines.push(Line::from(vec![label, Span::raw(" "), value]));
    }
    lines
}

/// The label's style for the row's three states.
fn label_style(editing_here: bool, focused: bool, p: Palette) -> ratatui::style::Style {
    if editing_here {
        p.focused().add_modifier(Modifier::BOLD)
    } else if focused {
        p.fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        p.fg(p.dim)
    }
}

/// The rendered value of one field, padded to the value column.
fn field_value(
    form: &crate::app::FormState,
    field: FormField,
    kind: ForwardKind,
    value_w: usize,
    focused: bool,
    editing_here: bool,
    p: Palette,
) -> Span<'static> {
    let value_style = if editing_here {
        p.focused()
    } else if focused {
        p.fg(p.accent)
    } else {
        p.fg(p.text)
    };
    match field {
        FormField::Kind => Span::styled(
            pad_to_width(
                &format!("< {} >", fit_pair_spaced(kind.label(), kind.label_en(), value_w.saturating_sub(4))),
                value_w,
            ),
            if editing_here { p.focused() } else { p.fg(p.accent) },
        ),
        FormField::AutoRestart => {
            let mark = if form.auto_restart { "[x]" } else { "[ ]" };
            let text = fit_pair("网络类失败时自动退避重试", "Retry network failures", value_w.saturating_sub(4));
            Span::styled(pad_to_width(&format!("{mark} {text}"), value_w), value_style)
        }
        FormField::RemoteHost | FormField::RemotePort if kind == ForwardKind::Dynamic => {
            let text = fit_pair("(SOCKS5 不需要)", "(not for SOCKS5)", value_w);
            Span::styled(pad_to_width(&text, value_w), p.fg(p.dim))
        }
        _ => {
            let text = form_field_buf(form, field).map(|b| truncate_to_width(b.value(), value_w)).unwrap_or_default();
            Span::styled(pad_to_width(&text, value_w), value_style)
        }
    }
}

/// The SSH HOST row: value on the left, a ▾/▴ affordance on the right.
///
/// The `(N台可选)` hint is deliberately suppressed once a host is chosen — it
/// would push the actual alias out of view, which is what the user is checking.
pub(crate) fn host_field_line(
    app: &App,
    form: &crate::app::FormState,
    p: Palette,
    value_w: usize,
    editing_here: bool,
    label: Span<'static>,
) -> Line<'static> {
    let typed = form.ssh_host.value();
    let open = app.picker.as_ref().is_some_and(|pk| pk.open);
    let marker = if open { "▴" } else { "▾" };
    let drop_style = if editing_here { p.focused() } else { p.fg(p.accent) };
    let marker_w = display_width(marker);
    let avail = value_w.saturating_sub(marker_w + 1);
    let hint = if !typed.trim().is_empty() {
        String::new()
    } else if !app.hosts.is_empty() {
        fit_pair_spaced(&format!("({}台可选)", app.hosts.len()), &format!("({} available)", app.hosts.len()), avail)
    } else if app.hosts_error.is_some() {
        fit_pair_spaced("(读取失败)", "(read failed)", avail)
    } else {
        fit_pair_spaced("(无主机)", "(no hosts)", avail)
    };
    let shown = truncate_to_width(&format!(" {typed}{hint}"), avail);
    let value_style = if editing_here { p.focused() } else { p.fg(p.text) };
    Line::from(vec![
        label,
        Span::raw(" "),
        Span::styled(pad_to_width(&shown, avail), value_style),
        Span::styled(marker, drop_style.add_modifier(Modifier::BOLD)),
    ])
}

/// Validation errors, the auto-suggested port note, and the key hint.
///
/// The hint line is the form's own legend: both stages are spelled out, because
/// `enter`/`esc` mean different things in them and a table can only show one.
/// `ctrl+s`/`f10` save from either stage (the handler treats them identically),
/// so the hint does not have to change with it.
pub(crate) fn form_notes(form: &crate::app::FormState, p: Palette, width: usize) -> Vec<Line<'static>> {
    let mut extra = Vec::new();
    for err in form.errors.iter().take(3) {
        extra.push(Line::from(Span::styled(truncate_to_width(&format!("✗ {err}"), width), p.fg(p.failed))));
    }
    if let Some(note) = &form.local_port_note {
        // The auto-derived local port is a *suggestion*; say so plainly.
        let text = format!("· {}（可编辑） / (editable)", note);
        extra.push(Line::from(Span::styled(truncate_to_width(&text, width), p.fg(p.pending))));
    }
    if let Some(hint) = &form.port_hint {
        // The Chinese keyword check is deliberate: `port_hint` is built in
        // `src/app/form.rs` and the Chinese prefix is part of that contract.
        let style = if hint.contains("占用") { p.fg(p.pending) } else { p.fg(p.up) };
        extra.push(Line::from(Span::styled(truncate_to_width(&format!("· {hint}"), width), style)));
    }
    extra.push(Line::from(""));
    // The legend flows like the main footer instead of being one long string:
    // the popup is wide but a narrow terminal can still clamp it, and a single
    // line would then be cut mid-token (`esc 取…`), which is worse than useless
    // for the keys that teach the two-stage editing model. Flowing drops a whole
    // hint instead, and on a normal terminal every hint fits in both languages.
    let hints: &[Hint] = if form.is_editing_field() {
        // Say which stage the form is in, in words: the highlight colour is the
        // other cue, and colour alone is not allowed to carry meaning.
        extra.push(Line::from(Span::styled("编辑中 / Editing:", p.fg(p.pending))));
        &[
            ("字符", "输入", "Type"),
            ("enter/esc", "结束编辑", "Done"),
            ("tab", "下一字段", "Next"),
            ("ctrl+s", "保存", "Save"),
        ]
    } else {
        &[
            ("↑↓", "选字段", "Field"),
            ("enter", "编辑", "Edit"),
            ("tab", "切换", "Next"),
            ("ctrl+s", "保存", "Save"),
            ("f10", "保存", "Save"),
            ("esc", "取消", "Cancel"),
        ]
    };
    extra.extend(flow_hints(hints, width, 3, p));
    extra
}

/// Show a text cursor only while a field is actively being edited.
pub(crate) fn place_form_cursor(
    frame: &mut Frame,
    form: &crate::app::FormState,
    inner: Rect,
    popup: Rect,
    current_idx: usize,
) {
    if !form.is_editing_field() {
        return;
    }
    let Some(buf) = form_field_buf(form, form.field) else { return };
    let prefix: String = buf.text.chars().take(buf.cursor).collect();
    // A field value is unbounded typed text (or a config file someone edited by
    // hand), so the cell offset is added in `usize`: a `u16` sum wraps around
    // and would park the cursor in the middle of the popup.
    let x = usize::from(inner.x) + form_value_x(inner.width) + display_width(&prefix);
    let y = usize::from(inner.y) + current_idx;
    if x < usize::from(popup.right().saturating_sub(1)) && y < usize::from(inner.bottom()) {
        match (u16::try_from(x), u16::try_from(y)) {
            (Ok(x), Ok(y)) => frame.set_cursor_position((x, y)),
            _ => {
                static SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                warn_once(
                    &SEEN,
                    "ui.render_degraded",
                    &[("what", &"form_cursor"), ("reason", &"cursor_overflow"), ("x", &x), ("y", &y)],
                );
            }
        }
    }
}
#[cfg(test)]
mod tests {
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
}
