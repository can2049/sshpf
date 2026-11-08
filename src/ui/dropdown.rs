//! The inline `~/.ssh/config` host list, expanded under the SSH HOST field.
//!
//! Deliberately an in-form panel rather than a separate centred modal: choosing
//! from the hosts the config file already declares should be the obvious action,
//! and it should stay visible next to the field it fills. It is confined to the
//! form's body so it can never cover the import pane.
//!
//! Rows are `▸ alias   endpoint`; the filter echo, the empty/error states and the
//! key hint each degrade to a single truncated line rather than overflowing the
//! panel.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::App;
use crate::model::HostPicker;
use crate::text::{pad_to_width, truncate_to_width};
use crate::ui::palette::palette;
use crate::ui::text::fit_pair;

// ---------------------------------------------------------------------------
// Inline SSH-host dropdown (part of the form)
// ---------------------------------------------------------------------------

/// Draw the expanded host list directly beneath the SSH HOST field.
///
/// This is deliberately an in-form panel rather than a separate centred modal:
/// the whole point of the change is that choosing from `~/.ssh/config` should
/// be the obvious, visible action when adding a forward.
pub(crate) fn draw_host_dropdown(frame: &mut Frame, app: &App, picker: &HostPicker, inner: Rect, field_row: u16) {
    let area = match dropdown_area(inner, field_row) {
        Some(area) => area,
        // Not enough room for a usable list: keep the field marker only.
        None => return,
    };
    let p = palette();
    frame.render_widget(Clear, area);
    let block =
        Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(p.fg(p.accent)).title(
            Line::from(Span::styled(
                " 从 ~/.ssh/config 选择主机 / Pick host ",
                p.fg(p.accent).add_modifier(Modifier::BOLD),
            )),
        );
    let body = block.inner(area);
    frame.render_widget(block, area);
    if body.height == 0 {
        return;
    }

    if app.hosts.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(no_hosts_message(app), p.fg(p.failed)))),
            Rect { height: 1, ..body },
        );
        return;
    }

    draw_filter_echo(frame, picker, body, p);
    let list_area = Rect { y: body.y + 1, height: body.height.saturating_sub(2), ..body };
    if picker.matches.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("没有匹配的主机 / No matching host", p.fg(p.dim)))),
            Rect { height: 1, ..list_area },
        );
    } else {
        draw_list(frame, app, picker, list_area, p);
    }
    draw_dropdown_hint(frame, body, p);
}

/// Where the panel goes, or `None` when it cannot hold a usable list.
///
/// The panel starts right under the SSH HOST row and may use every row left in
/// the popup. It is *overlaid* on the remaining fields (normal dropdown
/// behaviour), so the height is capped by the space below the field: frame (2) +
/// filter line (1) + hint line (1) => at least 1 list row.
fn dropdown_area(inner: Rect, field_row: u16) -> Option<Rect> {
    let below = inner.height.saturating_sub(field_row + 1);
    let max_height = 2 + 1 + 1 + crate::app::HOST_DROPDOWN_ROWS as u16;
    let height = max_height.min(below);
    if height < 5 {
        return None;
    }
    Some(Rect { x: inner.x, y: inner.y + field_row + 1, width: inner.width, height })
}

/// The reason there is nothing to list: the discovery error when there is one,
/// otherwise the "no hosts" sentence.
fn no_hosts_message(app: &App) -> String {
    app.hosts_error.clone().unwrap_or_else(|| "未在 ~/.ssh/config 中找到主机 / No hosts in ~/.ssh/config".to_string())
}

/// Row 0 of the panel body: the filter echo, or the "type to filter" hint while
/// the query is empty.
fn draw_filter_echo(frame: &mut Frame, picker: &HostPicker, body: Rect, p: crate::ui::palette::Palette) {
    let width = body.width as usize;
    let filter = if picker.query.is_empty() {
        Span::styled(fit_pair("输入以过滤", "Type to filter", width.saturating_sub(10)), p.fg(p.dim))
    } else {
        Span::styled(truncate_to_width(&picker.query, width.saturating_sub(10)), p.fg(p.text))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("过滤/Filter: ", p.fg(p.dim)), filter])),
        Rect { y: body.y, height: 1, ..body },
    );
}

/// The scrollable host rows: `▸ alias   endpoint`, one line each.
fn draw_list(frame: &mut Frame, app: &App, picker: &HostPicker, list_area: Rect, p: crate::ui::palette::Palette) {
    let width = list_area.width as usize;
    let list_h = list_area.height as usize;
    let mut out: Vec<Line<'static>> = Vec::with_capacity(list_h);
    let start = picker.scroll as usize;
    for (offset, pos) in (start..picker.matches.len()).take(list_h).enumerate() {
        let Some(host) = picker.matches.get(pos).and_then(|i| app.hosts.get(*i)) else {
            continue;
        };
        let sel = start + offset == picker.selected;
        let style = if sel { p.focused().add_modifier(Modifier::BOLD) } else { p.fg(p.text) };
        let alias_w = 24usize.min(width.saturating_sub(2));
        let alias = truncate_to_width(&format!("{}{}", if sel { "▸ " } else { "  " }, host.alias), alias_w);
        out.push(Line::from(vec![
            Span::styled(pad_to_width(&alias, alias_w), style),
            Span::styled(
                truncate_to_width(&host.endpoint(), width.saturating_sub(alias_w)),
                if sel { style } else { p.fg(p.dim) },
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(out), list_area);
}

/// The key hint pinned to the panel's last row.
fn draw_dropdown_hint(frame: &mut Frame, body: Rect, p: crate::ui::palette::Palette) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_to_width(
                "↑↓ k/j 选择 · enter 确定 · esc 收起 · tab 下一字段 / Select · Confirm · Close · Next",
                body.width as usize,
            ),
            p.fg(p.dim),
        ))),
        Rect { y: body.bottom().saturating_sub(1), height: 1, ..body },
    );
}

#[cfg(test)]
mod tests {
    use crate::app::{ConfirmAction, Overlay};

    use crate::ui::test_support::*;

    #[test]
    fn renders_host_dropdown_and_confirm() {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        app.hosts = vec![
            crate::ssh::SshHost {
                alias: "bastion-prod".into(),
                hostname: "10.0.4.9".into(),
                user: Some("deploy".into()),
                port: 22,
                proxy_jump: Vec::new(),
                identity_file: None,
            },
            crate::ssh::SshHost {
                alias: "dev-box".into(),
                hostname: "192.168.1.7".into(),
                user: None,
                port: 2222,
                proxy_jump: Vec::new(),
                identity_file: None,
            },
        ];

        // Collapsed: the field advertises that a list exists.
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("2台可选"), "host field should hint at the list:\n{text}");
        assert!(text.contains('▾'), "collapsed field should show a ▾ marker:\n{text}");

        // Expanded: the list is inline, under the SSH HOST row.
        app.open_host_picker();
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("从 ~/.ssh/config 选择主机"), "dropdown title missing:\n{text}");
        assert!(text.contains("bastion-prod"), "host alias missing:\n{text}");
        assert!(text.contains("deploy@10.0.4.9:22"), "host endpoint missing:\n{text}");
        assert!(text.contains("enter 确定"), "dropdown hint missing:\n{text}");
        assert!(text.contains('▴'), "expanded field should show a ▴ marker:\n{text}");

        // Typing filters the list.
        app.picker_query_char('d');
        app.picker_query_char('e');
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("dev-box"), "filtered host missing:\n{text}");
        assert!(!text.contains("bastion-prod"), "filter should drop non-matches:\n{text}");

        // Accepting fills the field and collapses the list.
        app.picker_accept();
        assert_eq!(app.form.as_ref().unwrap().ssh_host.value(), "dev-box");
        assert!(!app.host_picker_open(), "accept should collapse the dropdown");
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("dev-box"));

        app.close_overlay();
        app.overlay = Overlay::Confirm {
            action: ConfirmAction::Delete(1),
            prompt: "确认删除「api-dev」？(y/N)".to_string(),
        };
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("确认删除"), "confirm prompt missing:\n{text}");
        assert!(text.contains("y "), "confirm key hint missing:\n{text}");
    }
}
