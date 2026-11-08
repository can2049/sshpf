//! `notes` help sections.
//!
//! Cross-cutting prose: mouse gestures, what the status glyphs mean, and the
//! safety rules (what asks first, colour, logs).
//!
//! Wording is deliberately **not** normalised here: these are the exact strings
//! the overlay draws, and `tests/help_coverage.rs` checks them against the real
//! key dispatch. Add a row in the file whose context it belongs to.

use crate::help::{HelpItem, HelpSection, SectionId};

pub(super) const MOUSE: HelpSection = HelpSection {
    id: SectionId::Mouse,
    title_zh: "鼠标操作",
    title_en: "Mouse",
    items: &[
        HelpItem::note(
            "单击表格行：移动光标（多选模式下改为切换该行勾选）。",
            "Click a table row: move the cursor (in selection mode it toggles that row's mark).",
        ),
        HelpItem::note(
            "shift+单击表格行：从锚点扩选到该行。",
            "shift+click a table row: extend the marks from the anchor to that row.",
        ),
        HelpItem::note(
            "单击表格列表空白处：滚动用，不改变选择。",
            "Click the table list area: it only serves the wheel and changes no selection.",
        ),
        HelpItem::note("单击导入行：移动导入表光标。", "Click an import row: move the import cursor."),
        HelpItem::note("单击导入复选框：勾选 / 取消该行。", "Click an import checkbox: tick / untick that row."),
        HelpItem::note("单击导入列头：开始筛选该列。", "Click an import column header: start filtering that column."),
        HelpItem::note("单击筛选 chip 的 ×：删除该列筛选。", "Click a filter chip's ×: drop that column's filter."),
        HelpItem::note(
            "单击「清除全部筛选」/「撤销筛选」：清空或撤销筛选；无内容时变暗且不可点。",
            "Click Clear all filters / Undo filter; both dim out and are not clickable when empty.",
        ),
        HelpItem::note(
            "单击「全选 / 反选 / 全不选」：对应批量勾选，只作用于可见行。",
            "Click Select all / Invert / Select none: the matching bulk tick, visible rows only.",
        ),
        HelpItem::note(
            "单击导入列表空白处：滚轮在该列表内滚动。",
            "Click the import list area: the wheel scrolls within that list.",
        ),
        HelpItem::note(
            "单击多选动作条按钮：对应批量操作，仍要确认。",
            "Click a batch action button: the same batch operation, still behind the confirmation gate.",
        ),
        HelpItem::note("单击「退出选择模式」：退出多选模式。", "Click Exit selection mode: leave selection mode."),
        HelpItem::note(
            "单击多选模式「全选」：全选可见行，再点取消全选。",
            "Click Select all in selection mode: mark every visible row, click again to clear.",
        ),
        HelpItem::note(
            "单击过滤输入行：只吞掉这次点击，不穿透到下面的表格。",
            "Click the filter line: the click is swallowed so it cannot reach the table below.",
        ),
        HelpItem::note(
            "单击过滤行的「清除」按钮（显示为 × 清除）：清空过滤并回到首行。",
            "Click the filter line's clear affordance (shown as × Clear): empty the filter and go back to the first row.",
        ),
        HelpItem::note("点击落空：什么都不做。", "A click on empty space does nothing."),
        HelpItem::note(
            "弹窗吞掉点击，不会穿透到下面的表格。",
            "Popups swallow clicks; nothing leaks through to the table below.",
        ),
    ],
};

pub(super) const STATUS: HelpSection = HelpSection {
    id: SectionId::Status,
    title_zh: "状态含义",
    title_en: "Status legend",
    items: &[
        HelpItem::note("● 已连通 / Up：转发已就绪，可以开始使用。", "● Up: the forward is ready to use."),
        HelpItem::note(
            "◐ 连接中 / Starting：ssh 已启动，等待就绪标记。",
            "◐ Starting: ssh is up, waiting for the ready marker.",
        ),
        HelpItem::note(
            "◌ 重试 N / Retrying：网络类失败且开了自动重连，退避后重试（第 N 次）。",
            "◌ Retrying N: a network-class failure with auto-restart on; it retries after a backoff (attempt N).",
        ),
        HelpItem::note(
            "✗ 失败 / Failed：配置类失败（或未开自动重连），改完配置需手动重启。",
            "✗ Failed: a configuration-class failure (or auto-restart off); fix it and restart by hand.",
        ),
        HelpItem::note("○ 未启用 / Off：用户意图为关闭。", "○ Off: the user intends it to be off."),
        HelpItem::note(
            "表格「最后错误」列只放归类短句，原始 stderr 在详情面板。",
            "The Last error column shows a classified phrase; the raw stderr lives in the detail overlay.",
        ),
    ],
};

pub(super) const SAFETY: HelpSection = HelpSection {
    id: SectionId::Safety,
    title_zh: "安全与诊断",
    title_en: "Safety & diagnostics",
    items: &[
        HelpItem::new(
            "ctrl+c",
            "等同 q：先弹退出确认，不会直接退出",
            "Same as q: asks before quitting, never quits outright",
        ),
        HelpItem::note(
            "确认框内（其真实键位见「确认框」一节）：y 或 enter 执行，n 或 esc 取消。",
            "In the dialog (see the Confirmation section for its real keys): y or enter runs it, n or esc cancels.",
        ),
        HelpItem::note(
            "需要确认：删除、停用、重启、批量启停 / 重启 / 删除、保存编辑、放弃编辑、退出。",
            "Asks first: delete, stop, restart, batch start / stop / restart / delete, saving an edit, discarding edits, quitting.",
        ),
        HelpItem::note(
            "不弹确认：启动、新建、移动光标、过滤、帮助、详情。",
            "Never asks: starting, creating, moving the cursor, filtering, help, detail.",
        ),
        HelpItem::note(
            "NO_COLOR=1 关闭全部颜色，状态仍由符号和文字表达。",
            "NO_COLOR=1 disables all colour; glyphs and words still carry the status.",
        ),
        HelpItem::note(
            "日志在 ~/.local/state/sshpf/logs/sshpf.log；SSHPF_LOG_DIR / SSHPF_LOG_LEVEL 可改目录与级别，详见 docs/logging.md。",
            "Logs live in ~/.local/state/sshpf/logs/sshpf.log; SSHPF_LOG_DIR / SSHPF_LOG_LEVEL change the directory and level, see docs/logging.md.",
        ),
    ],
};
