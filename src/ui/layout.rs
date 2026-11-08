//! Table column selection: which columns to show, and how wide.
//!
//! Pure arithmetic, no ratatui widgets — which is why it is a separate module
//! and can be tested exhaustively at every terminal width.

// ---------------------------------------------------------------------------

/// One logical column of the tunnel table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Name,
    Kind,
    /// `127.0.0.1:8080`
    Local,
    /// `127.0.0.1:3000` / `(SOCKS5)`
    Remote,
    /// `8080→3000` — the merged fallback when the terminal is very narrow.
    LocalRemote,
    SshHost,
    Status,
    LastError,
    Uptime,
}

/// Space reserved for the `▶ ` selection symbol (always allocated so the table
/// never shifts when the selection moves).
const SELECTION_WIDTH: u16 = 2;
/// `Table::column_spacing`.
pub(crate) const COL_SPACING: u16 = 1;
/// Widest the LAST ERROR column may become (prior-art: `短句 ≤ 34 列`).
const MAX_LAST_ERROR: u16 = 34;
/// Narrowest LAST ERROR column we accept before dropping columns instead.
const MIN_LAST_ERROR: u16 = 4;
/// Narrow label column of the form: widest Chinese label (`SSH 主机` = 8 cells).
pub(crate) const FORM_LABEL_W: usize = 8;
/// Wide label column: widest bilingual label, `远端端口 Remote port` = 20 cells.
pub(crate) const FORM_LABEL_W_BI: usize = 20;
/// How many log lines the detail overlay keeps on screen.
pub(crate) const DETAIL_LOG_TAIL: usize = 15;

/// Width of the form's label column inside a popup of `inner_width` cells.
///
/// Every label carries both languages (`名称 Name`), which is worth a much wider
/// column on a normal terminal but would squeeze the values to nothing on a
/// small one. So the bilingual column is only paid for when the popup can spare
/// it; below the threshold the labels degrade to Chinese, which [`crate::text::fit`]
/// then renders on one line.
pub(crate) fn form_label_w(inner_width: u16) -> usize {
    // 20 cells of label + 1 of gap + 25 of value, the narrowest readable form.
    if inner_width >= (FORM_LABEL_W_BI + 26) as u16 { FORM_LABEL_W_BI } else { FORM_LABEL_W }
}

/// First cell of the form's value column inside a popup of `inner_width` cells.
pub(crate) fn form_value_x(inner_width: u16) -> usize {
    form_label_w(inner_width) + 1
}

/// Minimum width for a column when squeezing.
fn min_width(col: Col) -> u16 {
    match col {
        Col::Name => 6,
        Col::Kind => 1,
        Col::Local | Col::Remote => 10,
        Col::LocalRemote => 10,
        Col::SshHost => 6,
        Col::Status => 8,
        Col::LastError => MIN_LAST_ERROR,
        Col::Uptime => 10,
    }
}

/// Preferred width for a column (before shrinking).
fn ideal_width(col: Col) -> u16 {
    match col {
        Col::Name => 14,
        Col::Kind => 1,
        Col::Local | Col::Remote => 17,
        Col::LocalRemote => 20,
        Col::SshHost => 14,
        Col::Status => 11,
        // Overwritten by the "take the remaining space" pass.
        Col::LastError => MAX_LAST_ERROR,
        Col::Uptime => 10,
    }
}

/// Which columns survive at a given terminal width.
///
/// Sacrifice order mandated by the spec: ① 时长 → ② SSH 主机 → ③ merge 本地/远端.
fn select_columns(width: u16) -> Vec<Col> {
    if width >= 110 {
        vec![Col::Name, Col::Kind, Col::Local, Col::Remote, Col::SshHost, Col::Status, Col::LastError, Col::Uptime]
    } else if width >= 88 {
        vec![Col::Name, Col::Kind, Col::Local, Col::Remote, Col::SshHost, Col::Status, Col::LastError]
    } else if width >= 64 {
        vec![Col::Name, Col::Kind, Col::Local, Col::Remote, Col::Status, Col::LastError]
    } else {
        vec![Col::Name, Col::Kind, Col::LocalRemote, Col::Status, Col::LastError]
    }
}

/// Compute the table layout for `area_width`.
///
/// The LAST ERROR column is the elastic one: it absorbs (or yields) the slack
/// first, which reproduces the "错误列缩到 18 列" rule from the wireframe.
/// Whatever happens, `sum(widths) <= area_width` and the list is never empty.
pub fn column_layout(area_width: u16) -> Vec<(Col, u16)> {
    let mut cols = select_columns(area_width);
    let mut widths: Vec<u16> = cols.iter().map(|c| ideal_width(*c)).collect();
    let n = cols.len();

    // Budget for the column widths themselves (selection gutter + spacing).
    let overhead = SELECTION_WIDTH + COL_SPACING * (n.saturating_sub(1) as u16);
    let avail = area_width.saturating_sub(overhead);

    // 1. LAST ERROR takes whatever is left, clamped to a sane range.
    if let Some(err_idx) = cols.iter().position(|c| *c == Col::LastError) {
        let others: u16 = widths.iter().enumerate().filter(|(i, _)| *i != err_idx).map(|(_, w)| *w).sum();
        widths[err_idx] = avail.saturating_sub(others).clamp(MIN_LAST_ERROR, MAX_LAST_ERROR);
    }

    // 2. Shrink the widest elastic column one cell at a time until it fits.
    let mut total: u16 = widths.iter().sum();
    while total > avail {
        let mut best: Option<usize> = None;
        for (i, col) in cols.iter().enumerate() {
            if widths[i] <= min_width(*col) {
                continue;
            }
            if best.is_none_or(|b| widths[b] <= widths[i]) {
                best = Some(i);
            }
        }
        match best {
            Some(i) => {
                widths[i] -= 1;
                total -= 1;
            }
            None => break,
        }
    }

    // 3. Absolute guarantee for absurd terminals: drop trailing columns, then
    //    cap the survivor. Never panic, never overflow the area.
    while widths.iter().sum::<u16>() > area_width {
        if cols.len() > 1 {
            cols.pop();
            widths.pop();
        } else {
            widths[0] = widths[0].min(area_width);
            if widths[0] == 0 {
                break;
            }
        }
    }

    cols.into_iter().zip(widths).collect()
}

/// Spread `spare` cells across `widths` by `weights`, never overflowing.
///
/// The one piece of column arithmetic both tables share: once a row of columns
/// fits, whatever is left over is handed to the *elastic* ones in proportion to
/// their weights, and the last elastic column absorbs the rounding remainder so
/// the spare budget is spent exactly.
///
/// The two tables differ in *which* columns are elastic and in how they give
/// space back (the tunnel table shrinks the widest column one cell at a time), so
/// they keep their own strategies — this function is deliberately only the "add
/// spare" half, which is identical in both.
///
/// All arithmetic runs in `u32`: a pane can be `u16::MAX` cells wide, where
/// `spare * 4` overflows a `u16` and panics in a debug build (release would wrap
/// and hand out a nonsensical width). Every share is provably `<= spare`, so
/// narrowing it back cannot clip.
pub(crate) fn distribute_spare(widths: &mut [u16], spare: u16, weights: &[u32], elastic: &[usize]) {
    if elastic.is_empty() {
        return;
    }
    let mut left = u32::from(spare);
    let total = u32::from(spare);
    for (k, &i) in elastic.iter().enumerate() {
        let Some(slot) = widths.get_mut(i) else { continue };
        let weight = weights.get(k.min(weights.len().saturating_sub(1))).copied().unwrap_or(0);
        let share = if k + 1 == elastic.len() { left } else { (total * weight / 10).min(left) };
        let share = u16::try_from(share).unwrap_or(u16::MAX);
        *slot = slot.saturating_add(share);
        left = left.saturating_sub(u32::from(share));
    }
}

/// Footprint including the selection gutter and inter-column spacing.
#[cfg(test)]
fn layout_footprint(layout: &[(Col, u16)]) -> u16 {
    let n = layout.len() as u16;
    column_sum(layout) + SELECTION_WIDTH + COL_SPACING * n.saturating_sub(1)
}

/// Sum of the column widths (without the gutter/spacing overhead).
#[cfg(test)]
fn column_sum(layout: &[(Col, u16)]) -> u16 {
    layout.iter().map(|(_, w)| *w).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_layout_fits_and_degrades() {
        let wide = column_layout(120);
        assert_eq!(wide.len(), 8, "120 columns must show all eight columns");
        assert!(layout_footprint(&wide) <= 120);
        assert!(wide.iter().any(|(c, _)| *c == Col::Uptime));
        assert!(wide.iter().any(|(c, _)| *c == Col::SshHost));

        let mid = column_layout(100);
        assert_eq!(mid.len(), 7, "100 columns drops 时长 first");
        assert!(layout_footprint(&mid) <= 100);
        assert!(!mid.iter().any(|(c, _)| *c == Col::Uptime));
        assert!(mid.iter().any(|(c, _)| *c == Col::SshHost));

        let narrow = column_layout(80);
        assert_eq!(narrow.len(), 6, "80 columns also drops SSH 主机");
        assert!(layout_footprint(&narrow) <= 80);
        assert!(!narrow.iter().any(|(c, _)| *c == Col::SshHost));
        assert!(narrow.iter().any(|(c, _)| *c == Col::Local));
        assert!(narrow.iter().any(|(c, _)| *c == Col::Remote));

        let tiny = column_layout(60);
        assert_eq!(tiny.len(), 5);
        assert!(tiny.iter().any(|(c, _)| *c == Col::LocalRemote));
        assert!(layout_footprint(&tiny) <= 60);

        // Never overflows, never panics, for every width from absurd to huge.
        for w in 1u16..=240 {
            let layout = column_layout(w);
            assert!(!layout.is_empty(), "width {w} returned no columns");
            assert!(column_sum(&layout) <= w, "width {w} overflows: {layout:?}");
        }
    }

    #[test]
    fn column_layout_takes_the_slack_in_last_error() {
        let err_w = |w: u16| {
            column_layout(w)
                .into_iter()
                .find(|(c, _)| *c == Col::LastError)
                .map(|(_, w)| w)
                .expect("LAST ERROR column must exist")
        };
        assert!(err_w(120) >= err_w(100));
        assert!(err_w(120) <= MAX_LAST_ERROR);
    }
}
