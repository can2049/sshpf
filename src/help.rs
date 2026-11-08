//! Bilingual (Chinese + English) help catalog for the whole interface.
//!
//! Every operation the TUI offers — every key in every context, every mouse
//! gesture, every destructive-action gate — is described here **twice**: once in
//! Chinese and once in English. The help overlay renders this data (it is pure
//! data, no rendering lives in this module), and the on-screen hint lines use
//! the same wording so a user never has to guess whether the footer and `?`
//! agree.
//!
//! Why a data module instead of prose inside the renderer:
//!
//! * It can be **tested**. `tests/help_coverage.rs` scans the real key dispatch
//!   in `src/keys.rs` and fails when a dispatched key is not documented here, so
//!   "add a key, forget the help" is a test failure rather than a support ticket.
//! * It can be **counted**. [`SECTIONS`] is the single source of truth for the
//!   overlay's content; the renderer only decides layout, wrap and scroll.
//! * Both languages live next to each other in the source, which is the only
//!   practical way to keep them from drifting apart.
//!
//! Conventions used by every entry:
//!
//! * `keys` is the display form of a binding (`"j / ↓"`, `"ctrl+s"`). An empty
//!   `keys` means the row is prose (a note), rendered indented without a key
//!   column. A row whose `keys` is non-empty must have a *short* `zh`/`en` that
//!   fits one line of the overlay.
//! * `zh` is Chinese, `en` is English, both required. Neither may be empty.
//! * Every destructive action states in both languages that it asks first.

/// Contexts the help overlay documents. One section per context keeps the list
/// navigable and lets the coverage test map a dispatch function to exactly one
/// section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionId {
    /// Main dashboard: the tunnel table, no overlay open.
    Main,
    /// Selection mode (`v`): keys are *re-bound*, not added to.
    Selection,
    /// Create/edit form, navigating stage.
    Form,
    /// Create/edit form, text-editing stage.
    FormEditing,
    /// The `~/.ssh/config` host dropdown inside the form.
    HostPicker,
    /// The "import the host's forwards" pane inside the form.
    ImportPane,
    /// A column filter of the import pane being typed into.
    ImportFilter,
    /// Confirmation dialog.
    Confirm,
    /// Detail/log overlay.
    Detail,
    /// The `/` filter input on the main table.
    FilterBar,
    /// This overlay's own navigation.
    HelpOverlay,
    /// Mouse gestures (click / shift-click / wheel).
    Mouse,
    /// What the status glyphs and words mean.
    Status,
    /// Cross-cutting rules: the confirmation gate, colour, logs.
    Safety,
}

impl SectionId {
    /// Sections in display order inside the overlay.
    pub const ALL: [SectionId; 14] = [
        SectionId::Main,
        SectionId::Selection,
        SectionId::Form,
        SectionId::FormEditing,
        SectionId::HostPicker,
        SectionId::ImportPane,
        SectionId::ImportFilter,
        SectionId::Confirm,
        SectionId::Detail,
        SectionId::FilterBar,
        SectionId::HelpOverlay,
        SectionId::Mouse,
        SectionId::Status,
        SectionId::Safety,
    ];

    /// Stable identifier used by the coverage test and the docs.
    pub fn key(self) -> &'static str {
        match self {
            SectionId::Main => "main",
            SectionId::Selection => "selection",
            SectionId::Form => "form",
            SectionId::FormEditing => "form_editing",
            SectionId::HostPicker => "host_picker",
            SectionId::ImportPane => "import_pane",
            SectionId::ImportFilter => "import_filter",
            SectionId::Confirm => "confirm",
            SectionId::Detail => "detail",
            SectionId::FilterBar => "filter_bar",
            SectionId::HelpOverlay => "help_overlay",
            SectionId::Mouse => "mouse",
            SectionId::Status => "status",
            SectionId::Safety => "safety",
        }
    }
}

/// One documented row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpItem {
    /// Display form of the binding, e.g. `"j / ↓"`, `"ctrl+s"`, `"space"`.
    /// Empty for a prose note.
    pub keys: &'static str,
    /// Chinese description (required).
    pub zh: &'static str,
    /// English description (required).
    pub en: &'static str,
}

impl HelpItem {
    /// A key row.
    pub const fn new(keys: &'static str, zh: &'static str, en: &'static str) -> Self {
        HelpItem { keys, zh, en }
    }

    /// A prose note (no key column).
    pub const fn note(zh: &'static str, en: &'static str) -> Self {
        HelpItem { keys: "", zh, en }
    }

    /// Whether this row is prose rather than a binding.
    pub fn is_note(&self) -> bool {
        self.keys.is_empty()
    }
}

/// One section of the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpSection {
    pub id: SectionId,
    pub title_zh: &'static str,
    pub title_en: &'static str,
    pub items: &'static [HelpItem],
}

/// The whole catalog, in display order. Populated below; see the module docs for
/// the conventions every entry follows.
pub const SECTIONS: &[HelpSection] = &[
    HelpSection {
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
    },
    HelpSection {
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
    },
    HelpSection {
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
            HelpItem::new("f2 / ctrl+h / ctrl+o", "SSH 主机字段：打开主机下拉", "On SSH host: open the host list"),
            HelpItem::new(
                "ctrl+s / f10",
                "保存（仅导航态生效；新建直接连，编辑先确认，运行中会重建连接）",
                "Save (navigating stage only; create connects at once, edit asks first and rebuilds a live tunnel)",
            ),
            HelpItem::new("esc", "取消（有未保存内容会先确认）", "Cancel (asks first if there are unsaved edits)"),
        ],
    },
    HelpSection {
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
    },
    HelpSection {
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
    },
    HelpSection {
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
    },
    HelpSection {
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
    },
    HelpSection {
        id: SectionId::Confirm,
        title_zh: "确认框",
        title_en: "Confirmation",
        items: &[
            HelpItem::new("y / Y / enter", "执行该操作", "Run the action"),
            HelpItem::new("n / N / esc", "取消，什么都不做", "Cancel and change nothing"),
            HelpItem::note("确认框打开时，其它按键不生效。", "While the dialog is open no other key does anything."),
        ],
    },
    HelpSection {
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
    },
    HelpSection {
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
    },
    HelpSection {
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
    },
    HelpSection {
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
            HelpItem::note(
                "单击导入列头：开始筛选该列。",
                "Click an import column header: start filtering that column.",
            ),
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
    },
    HelpSection {
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
    },
    HelpSection {
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
    },
];

/// Look one section up by id.
pub fn section(id: SectionId) -> &'static HelpSection {
    SECTIONS.iter().find(|s| s.id == id).expect("every SectionId appears in SECTIONS")
}

/// `中文 English`, the form used by every inline hint line and dialog label.
pub fn bi(zh: &str, en: &str) -> String {
    format!("{zh} {en}")
}

/// Bilingual text fitted to `width` cells.
///
/// Progressive disclosure for the narrow columns and hint lines: show both
/// languages when they fit, otherwise Chinese only, otherwise a truncation. The
/// English half is never cut mid-word and left dangling — a half English phrase
/// is worse than none — hence the two fallbacks rather than one truncation.
pub fn fit(zh: &str, en: &str, width: usize) -> String {
    let both = bi(zh, en);
    if crate::text::display_width(&both) <= width {
        return both;
    }
    if crate::text::display_width(zh) <= width {
        return zh.to_string();
    }
    crate::text::truncate_to_width(zh, width)
}

/// Total number of documented rows across all sections (used by tests and the
/// overlay's scroll clamp).
pub fn item_count() -> usize {
    SECTIONS.iter().map(|s| s.items.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_has_a_title_in_both_languages() {
        for s in SECTIONS {
            assert!(!s.title_zh.is_empty(), "{}: missing Chinese title", s.id.key());
            assert!(!s.title_en.is_empty(), "{}: missing English title", s.id.key());
            assert!(
                s.title_zh.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{}: title_zh has no CJK",
                s.id.key()
            );
        }
    }

    #[test]
    fn every_item_documents_both_languages() {
        for s in SECTIONS {
            for item in s.items {
                assert!(!item.zh.is_empty(), "{}: empty Chinese description for {:?}", s.id.key(), item.keys);
                assert!(!item.en.is_empty(), "{}: empty English description for {:?}", s.id.key(), item.keys);
                assert!(
                    !item.en.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                    "{}: English description contains CJK for {:?}",
                    s.id.key(),
                    item.keys
                );
            }
        }
    }

    #[test]
    fn sections_are_listed_in_order_and_each_appears_once() {
        assert_eq!(SECTIONS.len(), SectionId::ALL.len());
        for (i, id) in SectionId::ALL.iter().enumerate() {
            assert_eq!(SECTIONS[i].id, *id, "section {i} is out of order");
            let hits = SECTIONS.iter().filter(|s| s.id == *id).count();
            assert_eq!(hits, 1, "{} appears {hits} times", id.key());
        }
    }

    #[test]
    fn fit_prefers_both_languages_then_chinese_only() {
        assert_eq!(fit("移动", "Move", 20), "移动 Move");
        // Too narrow for both, wide enough for Chinese.
        assert_eq!(fit("移动", "Move", 5), "移动");
        // Too narrow even for Chinese: truncate.
        let narrow = fit("移动选择", "Move selection", 3);
        assert!(crate::text::display_width(&narrow) <= 3);
    }

    #[test]
    fn notes_have_no_key_column() {
        let note = HelpItem::note("提示", "Note");
        assert!(note.is_note());
        assert!(!HelpItem::new("q", "退出", "Quit").is_note());
    }
}
