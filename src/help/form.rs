//! `form` help sections.
//!
//! The create/edit form. Three sections, because the form is three modal states:
//! navigating, typing into a field, and the host dropdown on top of it.
//!
//! Wording is deliberately **not** normalised here: these are the exact strings
//! the overlay draws, and `tests/help_coverage.rs` checks them against the real
//! key dispatch. Add a row in the file whose context it belongs to.

use crate::help::{HelpItem, HelpSection, SectionId};

pub(super) const FORM: HelpSection = HelpSection {
    id: SectionId::Form,
    title_zh: "新建 / 编辑表单",
    title_en: "Create / edit form",
    items: &[
        HelpItem::new("tab / shift+tab", "下一个 / 上一个字段", "Next / previous field"),
        HelpItem::new("↓ / ↑", "下移 / 上移字段高亮", "Move the field highlight down / up"),
        HelpItem::new(
            "enter",
            "执行字段动作：文本进入编辑 · 类型循环 · 自动重连取反 · SSH 主机展开下拉",
            "Act on the field: edit text, cycle type, toggle auto-restart, open host list",
        ),
        HelpItem::new(
            "← / h",
            "类型字段：上一个类型；自动重连字段：取反",
            "On Type: the previous kind; on Auto-restart: toggle",
        ),
        HelpItem::new(
            "→ / l",
            "类型字段：下一个类型（L/R/D）；自动重连字段：取反",
            "On Type: the next kind (L/R/D); on Auto-restart: toggle",
        ),
        HelpItem::new(
            "space",
            "类型字段循环 · 自动重连取反 · SSH 主机展开/收起导入面板",
            "Cycle Type, toggle auto-restart, toggle the import pane on SSH host",
        ),
        HelpItem::new("i", "SSH 主机字段：展开 / 收起导入面板", "On SSH host: toggle the import pane"),
        HelpItem::new("p", "远端端口字段：扫描对端正在监听的端口", "On Remote port: list the peer's listening ports"),
        HelpItem::new("f2 / ctrl+h / ctrl+o", "SSH 主机字段：打开主机下拉", "On SSH host: open the host list"),
        HelpItem::new(
            "ctrl+s / f10",
            "保存（仅导航态生效；新建直接连，编辑先确认，运行中会重建连接）",
            "Save (navigating stage only; create connects at once, edit asks first and rebuilds a live tunnel)",
        ),
        HelpItem::new("esc", "取消（有未保存内容会先确认）", "Cancel (asks first if there are unsaved edits)"),
    ],
};

pub(super) const FORM_EDITING: HelpSection = HelpSection {
    id: SectionId::FormEditing,
    title_zh: "表单编辑态",
    title_en: "Editing a field",
    items: &[
        HelpItem::note("直接输入字符即插入到光标处。", "Typing inserts characters at the cursor."),
        HelpItem::new("backspace", "删除光标前一个字符", "Delete the character before the cursor"),
        HelpItem::new("delete", "删除光标后一个字符", "Delete the character after the cursor"),
        HelpItem::new("← / →", "光标左移 / 右移", "Move the cursor left / right"),
        HelpItem::new("home / end", "光标到行首 / 行尾", "Cursor to the start / end"),
        HelpItem::new("enter / esc", "结束编辑，留在本字段", "Stop editing, stay on this field"),
        HelpItem::new(
            "tab / shift+tab",
            "结束编辑并切到下一 / 上一字段",
            "Stop editing and move to the next / previous field",
        ),
        HelpItem::new("ctrl+s", "保存表单", "Save the form"),
        HelpItem::new("f10", "保存表单（同 ctrl+s）", "Save the form (same as ctrl+s)"),
        HelpItem::note(
            "编辑态与导航态都能保存：编辑态按 ctrl+s / f10 与导航态一致，不会把字符打进字段。",
            "Saving works in both stages: ctrl+s / f10 behave the same while editing and never type into the field.",
        ),
        HelpItem::note(
            "输入远端端口会联动推荐本地端口；手改本地端口后不再被覆盖。",
            "Typing the remote port suggests the local port; a hand-edited local port is never overwritten.",
        ),
    ],
};

pub(super) const HOST_PICKER: HelpSection = HelpSection {
    id: SectionId::HostPicker,
    title_zh: "主机下拉",
    title_en: "Host dropdown",
    items: &[
        HelpItem::new("↓ / j", "下一台主机", "Next host"),
        HelpItem::new("↑ / k", "上一台主机", "Previous host"),
        HelpItem::new("pgdn / pgup", "向下 / 向上翻页", "Page down / up"),
        HelpItem::new("home / end", "第一台 / 最后一台", "First / last host"),
        HelpItem::note(
            "直接输入字符即模糊过滤主机别名；别名全不匹配时再按 user@host:port 匹配。",
            "Typing fuzzy-filters host aliases; only when no alias matches it falls back to user@host:port.",
        ),
        HelpItem::new("backspace", "删除查询字符", "Delete a query character"),
        HelpItem::new("enter", "选定该主机并收起下拉", "Pick this host and collapse the list"),
        HelpItem::new("esc", "收起下拉，不改字段值", "Collapse without changing the field"),
    ],
};
