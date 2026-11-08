//! `import` help sections.
//!
//! The "import the forwards `~/.ssh/config` already declares" pane, and the
//! column filter that lives on top of it.
//!
//! Wording is deliberately **not** normalised here: these are the exact strings
//! the overlay draws, and `tests/help_coverage.rs` checks them against the real
//! key dispatch. Add a row in the file whose context it belongs to.

use crate::help::{HelpItem, HelpSection, SectionId};

pub(super) const IMPORT_PANE: HelpSection = HelpSection {
    id: SectionId::ImportPane,
    title_zh: "导入面板",
    title_en: "Import pane",
    items: &[
        HelpItem::new("j / ↓", "下一行", "Next row"),
        HelpItem::new("k / ↑", "上一行", "Previous row"),
        HelpItem::new("pgdn / pgup", "向下 / 向上翻页", "Page down / up"),
        HelpItem::new("g / home", "跳到首行", "Jump to the first row"),
        HelpItem::new("G / end", "跳到末行", "Jump to the last row"),
        HelpItem::new("space", "勾选 / 取消当前行", "Tick / untick this row"),
        HelpItem::new("a", "全选可见行", "Tick every visible row"),
        HelpItem::new("U", "全不选可见行", "Untick every visible row"),
        HelpItem::new("I", "反选可见行", "Invert the visible rows"),
        HelpItem::new("f", "切到下一列筛选", "Start filtering the next column"),
        HelpItem::new("← / →", "上一列 / 下一列筛选", "Previous / next filter column"),
        HelpItem::new("F", "清除当前列筛选（无筛选则提示）", "Clear this column's filter (or say there is none)"),
        HelpItem::new("ctrl+r", "清除全部筛选", "Clear every filter"),
        HelpItem::new("z", "撤销上次筛选改动", "Undo the last filter change"),
        HelpItem::new("esc", "先退出筛选输入，再收起面板", "Leave filter input first, then close the pane"),
        HelpItem::new(
            "i",
            "收起面板（筛选输入中先 enter / esc）",
            "Close the pane (enter / esc first while typing a filter)",
        ),
        HelpItem::new("ctrl+s", "保存：勾选 N 行就生成 N 条隧道", "Save: one tunnel per ticked row"),
        HelpItem::note(
            "space 一旦打开面板就归「勾选」，收起面板用 i 或 esc。",
            "Once the pane is open space means tick; close it with i or esc.",
        ),
        HelpItem::note(
            "筛选输入中打不出字面 f：f 始终是切列。",
            "A literal f cannot be typed into a filter: f always switches columns.",
        ),
        HelpItem::note(
            "ctrl+s 实际由表单一侧执行（面板不吞这个键）：保存时按勾选的 N 行生成 N 条隧道。",
            "ctrl+s is handled by the form itself, not the pane: saving turns the N ticked rows into N tunnels.",
        ),
    ],
};

pub(super) const IMPORT_FILTER: HelpSection = HelpSection {
    id: SectionId::ImportFilter,
    title_zh: "导入列筛选",
    title_en: "Import column filter",
    items: &[
        HelpItem::note(
            "直接输入字符即追加到当前列筛选（实时过滤）。",
            "Typing appends to the current column filter and filters live.",
        ),
        HelpItem::new("backspace", "删除一个字符", "Delete a character"),
        HelpItem::new("enter", "结束筛选输入（面板不收起）", "End filter input (the pane stays open)"),
        HelpItem::new("esc", "结束筛选输入", "End filter input"),
        HelpItem::new("← / →", "换到上一列 / 下一列", "Switch to the previous / next column"),
        HelpItem::new("ctrl+r", "清除全部筛选", "Clear every filter"),
        HelpItem::new("ctrl+z", "撤销上次筛选改动", "Undo the last filter change"),
    ],
};
