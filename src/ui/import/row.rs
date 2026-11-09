//! Row geometry for the import table: the columns, their widths, and one line
//! per forward.
//!
//! The width arithmetic is the interesting part. Every column's floor is derived
//! from its own bilingual header (`监听 Listen` plus the `▾` filter marker) instead
//! of a hand-counted number, so a header is never truncated and a filter marker
//! never pushes the English half out; whatever is left over is shared by the
//! elastic columns, weighted towards 监听.
//!
//! All sums are computed in `u32` and narrowed once: a pane can be `u16::MAX`
//! cells wide, where `spare * 4` would overflow a `u16` and panic in a debug
//! build.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::model::{ImportColumn, ImportRow};
use crate::text::{bi, display_width, pad_to_width, truncate_to_width};
use crate::ui::layout::distribute_spare;
use crate::ui::palette::Palette;

/// The checkbox column: ` [x]` / ` [ ]` / ` [–]`, with `▸` marking the cursor
/// row. The cursor keeps a *symbol* as well as the highlight bar so a NO_COLOR
/// run still shows where the selection is.
pub(super) const CHECK_W: u16 = 4;

/// Base minimum width of the type column (its Chinese label plus the `▾` marker).
const MIN_KIND_W: u16 = 5;
const MIN_LISTEN_W: u16 = 9;
const MIN_TARGET_W: u16 = 9;
const MIN_RAW_W: u16 = 9;

// ---------------------------------------------------------------------------
// Row / column geometry
// ---------------------------------------------------------------------------

/// One data column: which model column it is, and how many cells it owns.
#[derive(Debug, Clone, Copy)]
pub(super) struct ColSpec {
    pub(super) col: ImportColumn,
    pub(super) width: u16,
}

/// Minimum width a column needs, derived from its own labels with
/// `display_width` rather than hardcoded numbers.
///
/// The floor is the *bilingual* label (`监听 Listen`) plus one cell for the `▾`
/// filtered marker, so a header is never truncated and a filter marker never
/// pushes the English half out. At this width the data still fits; the larger
/// width a column actually gets is the elastic spare, distributed below.
pub(super) fn min_width(col: ImportColumn) -> u16 {
    let base = match col {
        ImportColumn::Kind => MIN_KIND_W as usize,
        ImportColumn::Listen => MIN_LISTEN_W as usize,
        ImportColumn::Target => MIN_TARGET_W as usize,
        ImportColumn::Raw => MIN_RAW_W as usize,
    };
    (base.max(bilingual_width(col) as usize) + 1) as u16
}

/// The width at which a column's header can show both languages.
///
/// The `▾` filtered marker is *not* counted here: [`header_text`] reserves it
/// separately, so this is exactly the room the two labels need side by side.
pub(super) fn bilingual_width(col: ImportColumn) -> u16 {
    (display_width(col.label()) + 1 + display_width(col.label_en())) as u16
}

/// The columns that fit `width`, left to right.
///
/// Degradation order is 「原始指令」→「目标」: the raw directive is the least
/// needed once the row is recognised, and the listen/target pair is what the
/// user actually filters on.
pub(super) fn columns(width: u16) -> Vec<ColSpec> {
    let mut out: Vec<ColSpec> = Vec::with_capacity(4);
    // checkbox cell + one separating space + the kind column
    let kind_w = min_width(ImportColumn::Kind);
    let mut used = CHECK_W + 1 + kind_w;
    out.push(ColSpec { col: ImportColumn::Kind, width: kind_w });
    for col in [ImportColumn::Listen, ImportColumn::Target, ImportColumn::Raw] {
        let min = min_width(col);
        if width >= used + 1 + min {
            out.push(ColSpec { col, width: min });
            used += 1 + min;
        }
    }

    // A pane narrower than the fixed part still must not overflow: shrink the
    // type column (the only survivor down here) rather than drawing past the edge.
    if used > width {
        let fixed = CHECK_W + 1;
        let room = width.saturating_sub(fixed);
        if let Some(first) = out.first_mut() {
            first.width = room;
        }
        out.truncate(1);
        if room == 0 {
            out.clear();
        }
        return out;
    }

    // Every spare cell goes to the elastic columns, weighted towards 监听: it is
    // the column a user narrows by when the list is long. The distribution
    // itself is shared with the tunnel table (see [`distribute_spare`]) so the
    // two cannot disagree on how a leftover cell is handed out.
    let spare = width.saturating_sub(used);
    let elastic: Vec<usize> =
        out.iter().enumerate().filter(|(_, spec)| spec.col != ImportColumn::Kind).map(|(i, _)| i).collect();
    let mut widths: Vec<u16> = out.iter().map(|s| s.width).collect();
    distribute_spare(&mut widths, spare, &[4, 3, 3], &elastic);
    for (spec, w) in out.iter_mut().zip(widths) {
        spec.width = w;
    }
    out
}

/// `中文 English` for the reason a row cannot be imported.
///
/// Both halves come from `model.rs` (`unsupported_reason` /
/// `unsupported_reason_en`), so adding a third un-importable case is a
/// compile-visible edit there and the two languages cannot drift apart — no
/// local translation table that could silently fall back to a generic wording.
///
/// The fallback below is for the case the model does *not* describe: it is only
/// reached for a row that is genuinely un-importable (`importable()` false), yet
/// both accessors returned `None`, which would otherwise print a bare `⚠`.
pub(super) fn unsupported_label(row: &ImportRow) -> String {
    match (row.forward.unsupported_reason(), row.forward.unsupported_reason_en()) {
        (Some(zh), Some(en)) => bi(zh, en),
        _ => bi("无法导入", "Cannot import"),
    }
}

/// One row, padded to the full width so the cursor bar spans the whole line.
pub(super) fn row_line(row: &ImportRow, is_cursor: bool, cols: &[ColSpec], width: u16, p: Palette) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols.len() + 2);
    spans.push(check_cell(row, is_cursor, p));
    spans.push(Span::raw(" "));
    spans.extend(value_cells(row, cols, p));
    pad_to_line(&mut spans, cols, width);
    let line = Line::from(spans);
    if is_cursor { line.style(p.focused()) } else { line }
}

/// The leading cell: cursor mark, then one of four box glyphs.
///
/// Symbol *and* colour, so a `NO_COLOR` run loses no information:
///
/// | glyph | meaning |
/// |---|---|
/// | `[x]` | ticked — saving creates this forward |
/// | `[ ]` | not ticked |
/// | `[✓]` | already saved as a tunnel (dimmed, never tickable) |
/// | `[–]` | cannot be imported at all (dimmed, reason in the last column) |
///
/// `[✓]` rather than a second `[–]`: "you already have this" and "this cannot be
/// expressed" are different sentences, and the pane has to say both. The tick
/// box is the one place where the user decides, so the distinction belongs here
/// as well as in the last column's label.
fn check_cell(row: &ImportRow, is_cursor: bool, p: Palette) -> Span<'static> {
    let lead = if is_cursor { "▸" } else { " " };
    let (check, style) = if !row.importable() {
        ("[–]", p.fg(p.dim))
    } else if row.existing.is_some() {
        ("[✓]", p.fg(p.dim))
    } else if row.checked {
        ("[x]", p.fg(p.up))
    } else {
        ("[ ]", p.fg(p.dim))
    };
    Span::styled(format!("{lead}{check}"), style)
}

/// `已存在：cloud_pc-L-18789 / exists as cloud_pc-L-18789`.
///
/// Chinese first and verbatim: the pane test asserts on 「已存在」, the same way the
/// port-in-use hint is asserted by 「占用」. The English half is composed here rather
/// than stored, so the two cannot drift apart.
pub(super) fn existing_label(name: &str) -> String {
    bi(&format!("已存在：{name}"), &format!("exists as {name}"))
}

/// The data cells, in column order.
fn value_cells(row: &ImportRow, cols: &[ColSpec], p: Palette) -> Vec<Span<'static>> {
    let importable = row.importable();
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols.len());
    for (i, spec) in cols.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let last = i + 1 == cols.len();
        // For a row we cannot import, the reason is the only thing the user needs
        // from it, so it takes *all* the width the remaining columns would have
        // used instead of being squeezed into the last one. It is the label on
        // the row, not a column value, hence the indent kept below the headers.
        if !importable && last {
            // The bilingual reason comes from `unsupported_label`, so the two
            // languages are paired at the source instead of drifting here.
            let reason = format!("⚠ {}", unsupported_label(row));
            spans.push(cell_span(&reason, rest_width(cols, i).max(spec.width), p.fg(p.failed)));
            continue;
        }
        // An already-saved forward explains itself in the same place, for the same
        // reason: the user's question is "why can't I tick this?", and the answer
        // is a sentence, not a column value. It names the tunnel so the row is
        // traceable to the table the user is about to look at.
        if let Some(existing) = row.existing.as_deref()
            && last
        {
            let label = existing_label(existing);
            spans.push(cell_span(&label, rest_width(cols, i).max(spec.width), p.fg(p.dim)));
            continue;
        }
        let style = if !importable {
            p.fg(p.dim)
        } else if spec.col == ImportColumn::Kind {
            p.fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            p.fg(p.text)
        };
        spans.push(cell_span(&spec.col.cell(&row.forward), spec.width, style));
    }
    spans
}

/// Width still available to the columns from `i` on, separators included.
///
/// Widened before summing: the widths come from a pane that can be up to
/// `u16::MAX` cells wide, and the running total plus the separator cells can
/// pass that. `u16::MAX` on overflow simply means "no spare width".
fn rest_width(cols: &[ColSpec], i: usize) -> u16 {
    let rest: u32 = cols[i..].iter().map(|c| u32::from(c.width)).sum::<u32>() + (cols.len() - i - 1) as u32;
    u16::try_from(rest).unwrap_or(u16::MAX)
}

/// Fill the rest of the line.
///
/// The cursor highlight is a bar, and a bar that stops mid-row reads as a text
/// selection instead of a focused row. The reason span can already have absorbed
/// the spare width, so this only ever pads — it never truncates. The total is
/// computed wide and only compared against `width`: an over-wide sum simply
/// means "no padding needed".
fn pad_to_line(spans: &mut Vec<Span<'static>>, cols: &[ColSpec], width: u16) {
    let mut used = u32::from(CHECK_W) + 1;
    for (i, spec) in cols.iter().enumerate() {
        used += u32::from(spec.width) + u32::from(i > 0);
    }
    if used < u32::from(width) {
        spans.push(Span::raw(" ".repeat((u32::from(width) - used) as usize)));
    }
}

/// `text` padded or truncated to exactly `width` cells.
pub(super) fn cell_span(text: &str, width: u16, style: Style) -> Span<'static> {
    let cells = width as usize;
    Span::styled(pad_to_width(&truncate_to_width(text, cells), cells), style)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::{ForwardKind, ImportTable, SshForward};
    use crate::ui::import::header::header_text;
    use crate::ui::palette::palette_with;

    fn forward(kind: ForwardKind, listen_port: u16, raw: &str) -> SshForward {
        SshForward {
            kind,
            listen_addr: String::new(),
            listen_port,
            target_host: "127.0.0.1".to_string(),
            target_port: 3000,
            host_alias: "dev-box".to_string(),
            line: 7,
            raw: raw.to_string(),
        }
    }

    #[test]
    fn columns_fit_the_width_and_degrade_from_the_right() {
        // Wide: all four data columns, and the footprint equals the budget.
        let wide = columns(90);
        assert_eq!(wide.len(), 4);
        let footprint: u16 =
            CHECK_W + 1 + wide.iter().map(|c| c.width).sum::<u16>() + wide.len().saturating_sub(1) as u16;
        assert_eq!(footprint, 90, "spare width must be fully distributed");

        // Narrow: 原始指令 goes first, then 目标. 类型 always survives.
        for w in 8u16..=120 {
            let cols = columns(w);
            assert!(!cols.is_empty(), "width {w} produced no columns");
            let used: u16 =
                CHECK_W + 1 + cols.iter().map(|c| c.width).sum::<u16>() + cols.len().saturating_sub(1) as u16;
            assert!(used <= w, "width {w} overflows: {cols:?}");
        }
        // Even 20 columns still fits a few columns; 12 leaves only the type.
        assert_eq!(columns(10).iter().map(|c| c.col).collect::<Vec<_>>(), vec![ImportColumn::Kind]);
    }

    /// The elastic-space pass must not overflow on a very wide pane.
    ///
    /// `share = spare * weight / 10` is computed in u16 upstream; `spare` can be
    /// almost `u16::MAX` (a pane spanning a very wide terminal), so `spare * 4`
    /// overflows and panics in a debug build. The widths it then hands out also
    /// have to stay within the pane.
    #[test]
    fn columns_survive_a_very_wide_pane_without_overflowing() {
        for w in [4_000u16, 20_000, u16::MAX - 1, u16::MAX] {
            let cols = columns(w);
            assert!(!cols.is_empty(), "width {w} produced no columns");
            let used = u32::from(CHECK_W)
                + 1
                + cols.iter().map(|c| u32::from(c.width)).sum::<u32>()
                + cols.len().saturating_sub(1) as u32;
            assert!(used <= u32::from(w), "width {w} overflows: {cols:?}");
        }
    }

    /// A row whose reason absorbs the remaining columns sums those widths in
    /// u32, so the same wide-pane case cannot wrap into a tiny span.
    #[test]
    fn an_unimportable_row_on_a_wide_pane_keeps_its_reason_readable() {
        let p = palette_with(true);
        let bad = ImportRow {
            forward: forward(ForwardKind::Local, 0, "LocalForward abc 127.0.0.1:3000"),
            checked: false,
            existing: None,
        };
        for w in [90u16, 4_000, u16::MAX] {
            let cols = columns(w);
            let line = row_line(&bad, false, &cols, w, p);
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.contains("⚠ 端口无法解析"), "width {w}: {text}");
        }
    }

    #[test]
    fn column_minimums_are_derived_from_the_header_label() {
        // The floor must hold the Chinese label plus the `▾` marker, so a header
        // is never truncated into `监…`. The English half is offered on top when
        // the elastic space allows it (see `bilingual_width`).
        for col in ImportColumn::ALL {
            let min = min_width(col) as usize;
            assert!(min > display_width(col.label()), "{col:?} too narrow for its Chinese header");
            // The floor reserves the bilingual labels *and* the `▾` marker.
            let needed = usize::from(bilingual_width(col));
            assert!(min > needed, "{col:?}: floor leaves no room for the English half plus the filter marker");
        }
        // Every column that survives at its minimum renders a full Chinese header.
        for w in [40u16, 60, 90, 120] {
            for spec in columns(w) {
                let table = ImportTable::new(vec![]);
                let text = header_text(spec.col, &table, spec.width);
                assert!(display_width(&text) <= spec.width as usize, "{w}/{col:?}: {text:?}", col = spec.col);
                if spec.width >= min_width(spec.col) {
                    assert!(
                        text.starts_with(spec.col.label()),
                        "{w}/{col:?}: header lost its Chinese label: {text:?}",
                        col = spec.col
                    );
                }
            }
        }
    }

    #[test]
    fn unimportable_rows_explain_themselves_in_both_languages() {
        // Built through real `ImportRow`s rather than by passing strings, so this
        // exercises the actual path `row_line` takes (and would break the day the
        // model's two accessors disagree about whether a row is importable).
        let row = |forward: SshForward| ImportRow { forward, checked: false, existing: None };

        // `listen_port == 0`: the port could not be parsed.
        let bad_port = row(forward(ForwardKind::Local, 0, "LocalForward abc 127.0.0.1:3000"));
        assert!(!bad_port.importable(), "the fixture must really be un-importable");
        assert_eq!(unsupported_label(&bad_port), "端口无法解析 Unparseable port");

        // Non-Dynamic with no target: the destination could not be parsed.
        let mut no_target = forward(ForwardKind::Local, 8080, "LocalForward 8080");
        no_target.target_host = String::new();
        no_target.target_port = 0;
        let bad_target = row(no_target);
        assert!(!bad_target.importable(), "the fixture must really be un-importable");
        assert_eq!(unsupported_label(&bad_target), "目标地址无法解析 Unparseable target");

        // A `-D` forward needs no target, so it is importable and must never be
        // labelled with a failure reason.
        let dynamic = row(forward(ForwardKind::Dynamic, 1080, "DynamicForward 1080"));
        assert!(dynamic.importable(), "a DynamicForward without a target is valid");
        assert_eq!(unsupported_label(&dynamic), "无法导入 Cannot import");
    }

    #[test]
    fn unimportable_rows_render_the_reason_not_the_directive() {
        let p = palette_with(true);
        let cols = columns(90);
        let bad = ImportRow {
            forward: forward(ForwardKind::Local, 0, "LocalForward abc 127.0.0.1:3000"),
            checked: false,
            existing: None,
        };
        let line = row_line(&bad, false, &cols, 90, p);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("⚠ 端口无法解析"), "{text}");
        assert!(text.contains("[–]"), "un-importable rows must not look tickable: {text}");
        assert!(!text.contains("[x]"), "{text}");
    }

    #[test]
    fn cursor_rows_are_flagged_with_a_symbol_too() {
        let p = palette_with(true);
        let cols = columns(90);
        let good = ImportRow {
            forward: forward(ForwardKind::Local, 8080, "LocalForward 8080 127.0.0.1:3000"),
            checked: true,
            existing: None,
        };
        let text = |l: Line<'static>| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text(row_line(&good, true, &cols, 90, p)).starts_with("▸[x]"));
        assert!(text(row_line(&good, false, &cols, 90, p)).starts_with(" [x]"));
    }
}
