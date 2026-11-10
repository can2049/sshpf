//! The create/edit form overlay: field rows, their notes and the text cursor.
//!
//! The form is the single place a tunnel is defined. Its two-stage model (arrows
//! move, `enter` edits) is expressed by three visual states per row — plain,
//! accent-bold focused, and a solid bar while text entry is active — so the stage
//! is never conveyed by colour alone.
//!
//! One optional sub-pane sits *below* the fields — the `~/.ssh/config` import
//! table or the peer's listening ports — and it is *added* to the form's height
//! rather than borrowed from it (a pane that squeezed the fields would hide the
//! values being checked against the rows). Which pane claims that slot, and how
//! the body splits for it, is [`pane`]'s job; this file only reserves the room
//! and draws the fields.

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
use crate::ui::layout::{FORM_POPUP_W, form_label_w, form_value_x};
use crate::ui::palette::{Palette, palette};
use crate::ui::text::{fit_pair, fit_pair_spaced, warn_once};

pub mod pane;

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
    // `sub` rather than `pane`: the module of the same name owns the decision.
    let sub = pane::pane(app);
    // 110 instead of 74: at 74 the import pane's raw-directive column was clipped
    // (`RemoteForward 9000 12…`), and this is the one screen where a tunnel is
    // defined. `centered` clamps both axes, so a narrow terminal keeps its width.
    let popup = centered(area, FORM_POPUP_W.min(area.width), pane::height(sub).min(area.height));
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
    let (body, pane_area) = pane::split(app, inner, sub, lines.len() as u16 + extra.len() as u16, &mut extra, p);

    let used = lines.len() as u16;
    if body.height > used {
        let rest = Rect { x: body.x, y: body.y + used, width: body.width, height: body.height - used };
        frame.render_widget(Paragraph::new(extra), rest);
    }
    frame.render_widget(Paragraph::new(lines), body);

    // Expand the dropdown *inside* the form, anchored under the SSH HOST row.
    // It is confined to the body so it can never cover the sub-pane.
    if let Some(picker) = app.picker.as_ref().filter(|pk| pk.open) {
        draw_host_dropdown(frame, app, picker, body, current_idx as u16);
    }

    place_form_cursor(frame, form, body, popup, current_idx);

    // Drawn last, and outside `body`: it owns its own cursor (a column filter
    // being typed into), which must win over the form field cursor.
    if let Some(area) = pane_area {
        pane::draw(frame, app, area, sub, hits);
    }
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
mod tests;
