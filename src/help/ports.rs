//! `ports` help section.
//!
//! The list of ports the peer is listening on, opened with `p` on the 远端端口
//! field. It is the keyboard face of `crate::model::PortPick`: `enter` accepts one
//! port into the field, while ticking rows and saving creates one tunnel per
//! ticked port (see `app::port_batch`).
//!
//! Wording is deliberately **not** normalised here: these are the exact strings
//! the overlay draws, and `tests/help_coverage.rs` checks them against the real
//! key dispatch. Add a row in the file whose context it belongs to.

use crate::help::{HelpItem, HelpSection, SectionId};

pub(super) const PORT_PICKER: HelpSection = HelpSection {
    id: SectionId::PortPicker,
    title_zh: "对端端口列表",
    title_en: "Peer port list",
    items: &[
        HelpItem::new("j / ↓", "下一行", "Next row"),
        HelpItem::new("k / ↑", "上一行", "Previous row"),
        HelpItem::new("pgdn / pgup", "向下 / 向上翻页", "Page down / up"),
        HelpItem::new("g / home", "跳到首行", "Jump to the first row"),
        HelpItem::new("G / end", "跳到末行", "Jump to the last row"),
        HelpItem::new("space", "勾选 / 取消当前行", "Tick / untick this row"),
        HelpItem::new("a", "全选", "Tick every row"),
        HelpItem::new("U", "全不选", "Untick every row"),
        HelpItem::new("I", "反选", "Invert every row"),
        HelpItem::new("enter", "填入当前行端口并收起列表", "Fill in this port and close the list"),
        HelpItem::new("r", "重新扫描（保留勾选）", "Scan again (ticks are kept)"),
        HelpItem::new("esc", "收起列表（不改字段值）", "Close the list without changing the field"),
        HelpItem::new("i", "收起列表（同 esc）", "Close the list (same as esc)"),
        HelpItem::note(
            "勾选多行后保存，每个端口生成一条 -L 转发。",
            "Save after ticking rows and each port becomes its own -L tunnel.",
        ),
        HelpItem::note(
            "只读列表（-R 转发）不能勾选：列出的端口在对端已被占用。",
            "A read-only list (a -R forward) cannot be ticked: those ports are already taken on the peer.",
        ),
    ],
};
