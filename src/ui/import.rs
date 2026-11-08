//! The "import the forwards `~/.ssh/config` already declares" pane.
//!
//! Rendered *inside* the create form (see [`crate::ui::form::draw_form`]): the
//! form stays the single place where a new tunnel is defined, and this pane is
//! the shortcut for a host that already describes what should be forwarded, so
//! the user does not retype ports that sit in the config file next to it.
//!
//! Pure rendering like the rest of `ui`: it reads `form.import`, draws, and
//! records what it drew into the mouse [`crate::ui::hit::HitMap`]. Nothing here
//! mutates state — the pane's scroll/window correction is computed locally for
//! the same reason.
//!
//! # Module map
//!
//! | file | what it draws |
//! |---|---|
//! | this file | the pane frame, its title, and the empty/error states |
//! | [`body`] | the visible rows and the scroll window that selects them |
//! | [`header`] | column headers, filter chips, select/filter buttons |
//! | [`footer`] | the selection summary and the wrapped key hints |
//! | [`row`] | column geometry and one rendered row |
//! | [`cursor`] | the span builder that keeps hits aligned with drawn text |
//!
//! Adding a column: [`row`] (width) + [`header`] (label) + [`crate::model::ImportColumn`].
//! Adding a key: [`footer`]'s `HINTS` and `crate::help::import`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::App;
use crate::text::{fit, truncate_to_width};
use crate::ui::hit::HitMap;
use crate::ui::palette::{Palette, palette};

pub mod body;
pub mod cursor;
pub mod footer;
pub mod header;
pub mod row;

use body::draw_body;

/// Preferred height of the pane. `ui::draw_form` adds this to the popup it would
/// draw anyway, so the form grows instead of squeezing the fields.
///
/// Sized for `IMPORT_ROWS_VISIBLE` rows *and* the bilingual footer: at the
/// form's default width the hints need five rows in `中文 / English` form, and a
/// footer that falls back to Chinese-only on a normal terminal is not bilingual.
pub const PREF_HEIGHT: u16 = 19;

/// Below this the pane is not worth drawing: two borders + a header + one row +
/// a (clipped) footer only just fit. Shorter than this and the form keeps the
/// space for its own fields (a pane with no row in it is pure noise).
pub const MIN_HEIGHT: u16 = 8;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The pane, with its bilingual title (the host name stays last in both).
pub fn draw_pane(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(form) = app.form.as_ref() else { return };
    if area.width < 8 || area.height < 3 {
        return;
    }
    let p = palette();
    let host = form.ssh_host.value();
    let title = fit(
        &format!(" 从 ~/.ssh/config 导入转发 · {host} "),
        &format!(" Import forwards from ~/.ssh/config · {host} "),
        " ",
        area.width.saturating_sub(2) as usize,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(p.fg(p.accent))
        .title(Line::from(Span::styled(title, p.fg(p.accent).add_modifier(Modifier::BOLD))));
    let inner = block.inner(area);
    // `Clear` because the pane sits on top of the form's own background.
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if form.import.is_empty() {
        draw_empty(frame, app, inner, p);
        return;
    }
    draw_body(frame, &form.import, inner, p, hits);
}

/// No rows at all: say *why* ("~/.ssh/config has nothing for this host" vs. the
/// discovery error), because an empty table with no explanation looks broken.
fn draw_empty(frame: &mut Frame, app: &App, inner: Rect, p: Palette) {
    let width = inner.width as usize;
    let (zh, en, style) = match app.import_error.as_deref() {
        Some(err) => (format!("✗ {err}"), None, p.fg(p.failed)),
        None => (
            "~/.ssh/config 中该主机没有端口转发配置".to_string(),
            Some("No port forwards for this host in ~/.ssh/config"),
            p.fg(p.dim),
        ),
    };
    let first = match en {
        Some(en) => fit(&zh, en, " ", width),
        None => truncate_to_width(&zh, width),
    };
    let lines = vec![
        Line::from(Span::styled(first, style)),
        Line::from(Span::styled(
            fit("esc 收起 · 也可以直接在表单里手填端口", "esc Close · Or type the ports in the form", " ", width),
            p.fg(p.dim),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use crate::model::{FormField, ImportColumn, TunnelConfig};
    use crate::ui::draw;
    use crate::ui::hit::HitTarget;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::App;
    use crate::model::{ForwardKind, ImportTable, SshForward};
    use crate::ui::test_support::*;

    fn fwd(kind: ForwardKind, listen: u16, target: &str, target_port: u16, raw: &str) -> SshForward {
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
    fn app_with_import(rows: Vec<SshForward>) -> App {
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

    fn sample_forwards() -> Vec<SshForward> {
        vec![
            fwd(ForwardKind::Local, 8080, "127.0.0.1", 80, "LocalForward 8080 127.0.0.1:80"),
            fwd(ForwardKind::Remote, 9000, "127.0.0.1", 9001, "RemoteForward 9000 127.0.0.1:9001"),
            // Un-importable: listen port could not be parsed.
            fwd(ForwardKind::Local, 0, "127.0.0.1", 5432, "LocalForward abc 127.0.0.1:5432"),
        ]
    }

    #[test]
    fn import_pane_renders_rows_checkboxes_and_summary() {
        let mut app = app_with_import(sample_forwards());
        let text = render_to_text(&mut app, 120, 45);
        assert!(text.contains("从 ~/.ssh/config 导入转发"), "pane title missing:\n{text}");
        assert!(text.contains("localhost:8080"), "listen cell missing:\n{text}");
        assert!(text.contains("(SOCKS5)") || text.contains("127.0.0.1:9001"), "target cell missing:\n{text}");
        // The 原始指令 column is the elastic one, so a long directive is shown
        // truncated with an ellipsis rather than being allowed to overflow.
        assert!(text.contains("LocalForward 8080"), "raw directive missing:\n{text}");
        // Two importable rows are pre-ticked, the un-importable one is not.
        assert!(text.contains("[x]"), "checked box missing:\n{text}");
        assert!(text.contains("[–]"), "un-importable box should be the striped form:\n{text}");
        assert!(text.contains("2/2 条勾选"), "summary must count importable rows only:\n{text}");
        assert!(text.contains("space 勾选"), "pane footer hints missing:\n{text}");
    }

    #[test]
    fn unimportable_import_row_shows_its_reason_and_cannot_be_ticked() {
        let mut app =
            app_with_import(vec![fwd(ForwardKind::Local, 0, "127.0.0.1", 5432, "LocalForward abc 127.0.0.1:5432")]);
        let text = render_to_text(&mut app, 120, 45);
        let row =
            text.lines().find(|l| l.contains("端口无法解析")).unwrap_or_else(|| panic!("reason missing:\n{text}"));
        assert!(!row.contains("[x]"), "a broken row must not look ticked: {row:?}");
        assert!(row.contains("[–]"), "it should look untickable, not merely unticked: {row:?}");
        assert!(text.contains("0/0 条勾选"), "summary should show nothing importable:\n{text}");
        // Ticking it is a no-op, at the model level too.
        let form = app.form.as_mut().unwrap();
        form.import.toggle_cursor();
        assert_eq!(form.import.checked_count(), 0, "un-importable rows must not become checked");
    }

    #[test]
    fn import_pane_shows_filtered_header_chips_and_buttons() {
        let mut app = app_with_import(sample_forwards());
        if let Some(f) = app.form.as_mut() {
            f.import.set_filter(ImportColumn::Kind, "R");
        }
        let text = render_to_text(&mut app, 120, 45);
        // The filtered column header is marked distinctly (`▾`) ...
        assert!(text.contains("类型▾"), "filtered header marker missing:\n{text}");
        // ... and the chip line names the active filter with a delete affordance.
        assert!(text.contains("筛选: 类型=R ×"), "filter chip missing:\n{text}");
        assert!(text.contains("清除全部筛选"), "clear-all affordance missing:\n{text}");
        assert!(text.contains("撤销筛选"), "undo affordance missing:\n{text}");
        assert!(text.contains("全选"), "select-all button missing:\n{text}");
        // Only the R row survives the filter.
        assert!(text.contains("RemoteForward 900"), "filtered row missing:\n{text}");
        assert!(!text.contains("LocalForward 8080"), "filter did not narrow:\n{text}");
    }

    #[test]
    fn import_pane_shows_the_editing_column_with_a_cursor() {
        let mut app = app_with_import(sample_forwards());
        if let Some(f) = app.form.as_mut() {
            f.import.begin_filter(ImportColumn::Listen);
            f.import.filter_char(ImportColumn::Listen, '8');
        }
        let text = render_to_text(&mut app, 120, 45);
        assert!(text.contains("监听:8"), "the column being typed into must echo the input:\n{text}");
    }

    #[test]
    fn import_pane_renders_at_small_sizes_without_panicking() {
        // The pane must degrade, never panic: it is opened by a key press, and a
        // panic would leave the terminal in raw mode.
        for rows in [0usize, 1, 3, 40] {
            let mut app = app_with_import(
                (0..rows)
                    .map(|i| fwd(ForwardKind::Local, 8000 + i as u16, "127.0.0.1", 80, "LocalForward x"))
                    .collect(),
            );
            if rows == 0 {
                app.form.as_mut().unwrap().import_open = true;
            }
            for (w, h) in [(1u16, 1u16), (8, 6), (20, 8), (40, 12), (74, 24), (120, 45)] {
                let backend = TestBackend::new(w, h);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("import pane at {w}x{h}: {e}"));
            }
        }
    }

    #[test]
    fn import_pane_empty_state_says_there_are_no_forwards() {
        // `import_open` on an empty table with no error: reachable when a host
        // switch empties the list. It must not collapse silently — the pane says
        // why it is empty, and the manual form fields all stay on screen.
        let mut app = app_with_import(vec![]);
        app.form.as_mut().unwrap().import_open = true;
        app.import_error = None;
        let text = render_to_text(&mut app, 120, 45);
        assert!(text.contains("没有端口转发配置"), "empty state missing:\n{text}");
        assert!(text.contains("从 ~/.ssh/config 导入转发"), "pane title missing:\n{text}");
        for field in FormField::ORDER {
            assert!(text.contains(field.label()), "form field {field:?} pushed out by the pane:\n{text}");
        }
    }

    #[test]
    fn import_pane_empty_state_surfaces_a_discovery_error() {
        let mut app = app_with_import(vec![]);
        app.form.as_mut().unwrap().import_open = true;
        app.import_error = Some("读取 ~/.ssh/config 转发配置失败: permission denied".to_string());
        let text = render_to_text(&mut app, 120, 45);
        assert!(text.contains("permission denied"), "the reason must be readable:\n{text}");
        assert!(!text.contains("没有端口转发配置"), "a failure is not the same as 'no forwards':\n{text}");
    }

    #[test]
    fn hits_are_not_pushed_for_rows_outside_the_list_area() {
        // 30 tunnels into a 14-row terminal: most rows are below the table area,
        // and a click down there must not resolve to a row that is not drawn.
        let tunnels: Vec<TunnelConfig> = (1..=30).map(|i| sample(i, &format!("t-{i:02}"), 8000 + i as u16)).collect();
        let mut app = app_with_tunnels(tunnels);
        let _ = render_to_text(&mut app, 100, 14);

        let list = app.hits.rect_of(&HitTarget::TunnelList).expect("table area must be recorded");
        let pushed: Vec<u16> =
            (1..=30u64).filter_map(|i| app.hits.rect_of(&HitTarget::TunnelRow(i)).map(|r| r.y)).collect();
        assert!(!pushed.is_empty(), "some rows must be clickable");
        assert!(pushed.iter().all(|y| *y < list.bottom()), "a row hit escaped the table area: {pushed:?}");
        assert!(pushed.len() < 30, "off-screen rows must not be clickable: {pushed:?}");
    }

    #[test]
    fn import_pane_hits_resolve_to_the_checkbox_and_row() {
        let mut app = app_with_import(sample_forwards());
        let _ = render_to_text(&mut app, 120, 45);

        // The first row's checkbox cell: the hit map must resolve to the *raw
        // row index*, not the visible position.
        let cb = app.hits.rect_of(&HitTarget::ImportCheckbox(0)).expect("first checkbox must be clickable");
        assert_eq!(app.hits.at(cb.x, cb.y), Some(&HitTarget::ImportCheckbox(0)));

        // Somewhere else on the same row: selects rather than toggles.
        let row = app.hits.rect_of(&HitTarget::ImportRow(0)).expect("first row must be clickable");
        let far_x = row.right().saturating_sub(2).max(row.x);
        assert_eq!(app.hits.at(far_x, row.y), Some(&HitTarget::ImportRow(0)), "wide part of the row selects");

        // Column headers and the action buttons are recorded too.
        assert!(app.hits.rect_of(&HitTarget::ImportColumnHeader(ImportColumn::Listen)).is_some());
        assert!(app.hits.rect_of(&HitTarget::ImportCheckAll).is_some());
        assert!(app.hits.rect_of(&HitTarget::ImportInvert).is_some());
        assert!(app.hits.rect_of(&HitTarget::ImportUncheckAll).is_some());
        assert!(app.hits.rect_of(&HitTarget::ImportList).is_some());
    }

    #[test]
    fn import_pane_hits_are_rebuilt_every_frame() {
        let mut app = app_with_import(sample_forwards());
        let _ = render_to_text(&mut app, 120, 45);
        let first = app.hits.len();
        assert!(first > 0);
        // Closing the pane must not leave its rects behind, or a click would hit
        // a widget that is no longer drawn.
        app.form.as_mut().unwrap().import_open = false;
        let _ = render_to_text(&mut app, 120, 45);
        assert!(app.hits.rect_of(&HitTarget::ImportCheckbox(0)).is_none(), "stale import hits survived");
    }

    // -- selection mode -----------------------------------------------------
}
