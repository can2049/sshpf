//! `main` help sections.
//!
//! Main screen and selection mode: the two contexts the dashboard itself
//! offers, in the order the user meets them.
//!
//! Wording is deliberately **not** normalised here: these are the exact strings
//! the overlay draws, and `tests/help_coverage.rs` checks them against the real
//! key dispatch. Add a row in the file whose context it belongs to.

use crate::help::{HelpItem, HelpSection, SectionId};

pub(super) const MAIN: HelpSection = HelpSection {
    id: SectionId::Main,
    title_zh: "主界面",
    title_en: "Main screen",
    items: &[
        HelpItem::new("j / ↓", "向下移动光标", "Move the cursor down"),
        HelpItem::new("k / ↑", "向上移动光标", "Move the cursor up"),
        HelpItem::new("g / home", "跳到首行", "Jump to the first row"),
        HelpItem::new("G / end", "跳到末行", "Jump to the last row"),
        HelpItem::new("space", "启用 / 停用当前行（运行中先确认）", "Enable / disable the row (asks if running)"),
        HelpItem::new(
            "r",
            "重启当前行（运行中先确认，未运行直接起）",
            "Restart the row (asks if running, starts if not)",
        ),
        HelpItem::new("n", "新建转发", "Create a tunnel"),
        HelpItem::new("e / E", "编辑当前行", "Edit the selected row"),
        HelpItem::new("d", "删除当前行（先确认）", "Delete the selected row (asks first)"),
        HelpItem::new("enter / l", "打开详情 / 日志", "Open detail / log"),
        HelpItem::new("/", "按名称 / SSH 主机过滤", "Filter by name or SSH host"),
        HelpItem::new("a", "全部启用（先确认）", "Enable all (asks first)"),
        HelpItem::new("x", "全部停用（先确认）", "Disable all (asks first)"),
        HelpItem::new("?", "打开 / 关闭本帮助", "Open / close this help"),
        HelpItem::new("v / V", "进入 / 退出多选模式", "Enter / leave selection mode"),
        HelpItem::new("esc", "清除过滤", "Clear the filter"),
        HelpItem::new("q", "退出 sshpf（先确认）", "Quit sshpf (asks first)"),
        HelpItem::note(
            "没有未启用条目时 a 只给提示；没有运行中条目时 x 只给提示。",
            "With nothing off, a only reports it; with nothing running, x only reports it.",
        ),
    ],
};

pub(super) const SELECTION: HelpSection = HelpSection {
    id: SectionId::Selection,
    title_zh: "多选模式",
    title_en: "Selection mode",
    items: &[
        HelpItem::new("v / V / esc", "退出多选模式", "Leave selection mode"),
        HelpItem::new("j / ↓ / k / ↑", "移动光标（已勾选的标记不变）", "Move the cursor (marks stay)"),
        HelpItem::new("g / G / home / end", "跳到首行 / 末行", "Jump to the first / last row"),
        HelpItem::new("J / K / shift+↓ / shift+↑", "从锚点按行扩选标记", "Extend the marks from the anchor"),
        HelpItem::new("space", "勾选 / 取消当前行", "Mark / unmark this row"),
        HelpItem::new("a", "全选切换（只作用于筛选后可见行）", "Toggle all (visible rows only)"),
        HelpItem::new("s", "批量启用（先确认）", "Batch start (asks first)"),
        HelpItem::new("x", "批量停用（先确认）", "Batch stop (asks first)"),
        HelpItem::new("R", "批量重启（先确认）", "Batch restart (asks first)"),
        HelpItem::new("D", "批量删除（先确认）", "Batch delete (asks first)"),
        HelpItem::note(
            "本模式下 space 只勾选、不会启停隧道；r / d / n / e 在此不生效。",
            "Here space only marks, it never starts or stops a tunnel; r / d / n / e do nothing.",
        ),
        HelpItem::note(
            "shift+↓ / shift+↑ 与 J / K 等效：从锚点扩选。",
            "shift+↓ / shift+↑ behave like J / K: extend the mark from the anchor.",
        ),
        HelpItem::note(
            "批量确认框里的条数是实际会变化的条数，不是已勾选条数。",
            "The batch prompt counts the rows that will actually change, not all marked rows.",
        ),
    ],
};
