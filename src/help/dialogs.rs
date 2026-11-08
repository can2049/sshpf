//! `dialogs` help sections.
//!
//! The small modals: the confirmation gate, the detail/log panel, the inline
//! filter input, and the help overlay itself.
//!
//! Wording is deliberately **not** normalised here: these are the exact strings
//! the overlay draws, and `tests/help_coverage.rs` checks them against the real
//! key dispatch. Add a row in the file whose context it belongs to.

use crate::help::{HelpItem, HelpSection, SectionId};

pub(super) const CONFIRM: HelpSection = HelpSection {
    id: SectionId::Confirm,
    title_zh: "确认框",
    title_en: "Confirmation",
    items: &[
        HelpItem::new("y / Y / enter", "执行该操作", "Run the action"),
        HelpItem::new("n / N / esc", "取消，什么都不做", "Cancel and change nothing"),
        HelpItem::note("确认框打开时，其它按键不生效。", "While the dialog is open no other key does anything."),
    ],
};

pub(super) const DETAIL: HelpSection = HelpSection {
    id: SectionId::Detail,
    title_zh: "详情 / 日志",
    title_en: "Detail / log",
    items: &[
        HelpItem::new("j / ↓", "向下滚动", "Scroll down"),
        HelpItem::new("k / ↑", "向上滚动", "Scroll up"),
        HelpItem::new("pgdn / pgup", "向下 / 向上翻页", "Page down / up"),
        HelpItem::new("q / esc / enter / l", "关闭，回到主界面", "Close and go back"),
        HelpItem::note(
            "失败原因与原始 ssh stderr 在这里完整保留。",
            "The failure reason and the raw ssh stderr are kept here in full.",
        ),
    ],
};

pub(super) const FILTER_BAR: HelpSection = HelpSection {
    id: SectionId::FilterBar,
    title_zh: "过滤输入",
    title_en: "Filter input",
    items: &[
        HelpItem::note(
            "直接输入字符即时过滤（名称 / SSH 主机模糊匹配）。",
            "Typing filters live by fuzzy match on name and SSH host.",
        ),
        HelpItem::new("backspace", "删除一个字符", "Delete a character"),
        HelpItem::new("enter", "保留过滤并关闭输入", "Keep the filter and close the input"),
        HelpItem::new("esc", "清除过滤并关闭输入", "Clear the filter and close the input"),
    ],
};

pub(super) const HELP_OVERLAY: HelpSection = HelpSection {
    id: SectionId::HelpOverlay,
    title_zh: "本帮助",
    title_en: "This help",
    items: &[
        HelpItem::new("j / ↓", "向下滚动", "Scroll down"),
        HelpItem::new("k / ↑", "向上滚动", "Scroll up"),
        HelpItem::new("pgdn / pgup", "向下 / 向上翻页", "Page down / up"),
        HelpItem::new("? / esc / q / enter", "关闭帮助", "Close the help"),
        HelpItem::note(
            "帮助按上下文分区，与真实键位一致；按键说明全部中英双语。",
            "Help is split by context and matches the real key map, in Chinese and English.",
        ),
    ],
};
