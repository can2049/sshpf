# 键位参考 / Keymap reference

> **这份文档回答**：某个键（或鼠标手势）在当前上下文里做什么？界面底部提示和 `?` 帮助里写的是真的吗？
> **不负责**：为什么这样设计（见 [design.md](design.md)）、渲染规则（见 [tui.md](tui.md)）、按键路由代码结构（见 [architecture.md](architecture.md#keys-键分发--srckeys)）。
> **相关代码路径**：`src/keys/`（真正的 dispatch）、`src/help.rs`（界面内帮助目录，本文与之同源）、`tests/help_coverage.rs`（机器校验）。
>
> **本文被 `tests/help_coverage.rs` 用 `include_str!` 嵌入并校验**：帮助目录里的每个 token 都必须在本文中以反引号出现。**不要删除任何按键 token**，只加不删。
> This page is parsed by `tests/help_coverage.rs`; every catalog token must appear here as a backticked cell. Never delete a token.

界面内随时按 `?` 打开分区帮助，内容与本文一致（帮助浮层是同一份数据渲染出来的）。
Press `?` anywhere to open the in-app help; it renders the same catalog as this page.

## 目录 / Contents

| 上下文 / Context | 说明 / What it is |
|---|---|
| [主界面](#主界面--main-screen) | 转发表格，没有弹层 / the tunnel table, no overlay |
| [多选模式](#多选模式--selection-mode) | `v` 进入，部分键**重绑** / entered with `v`, some keys are **re-bound** |
| [表单·导航态](#表单导航态--form-navigating) | 新建 / 编辑表单，只移动高亮 / create & edit form, highlight moves only |
| [表单·编辑态](#表单编辑态--form-editing) | 某个文本字段正在接收输入 / a text field is taking input |
| [主机下拉](#主机下拉--host-dropdown) | 表单里的 `~/.ssh/config` 主机列表 / the host list inside the form |
| [导入面板](#导入面板--import-pane) | 主机已声明的转发 / forwards already declared by the host |
| [导入列筛选](#导入列筛选--import-column-filter) | 导入面板某一列的筛选正在输入 / typing one column's filter |
| [确认框](#确认框--confirmation-dialog) | 破坏性操作前的对话框 / the dialog before a destructive action |
| [详情 / 日志](#详情--日志--detail--log) | 单条转发的完整诊断 / full diagnostics for one tunnel |
| [过滤输入](#过滤输入--main-filter) | `/` 打开的主表过滤 / the table filter opened with `/` |
| [帮助浮层](#帮助浮层--help-overlay) | `?` 打开的本帮助 / this help, opened with `?` |
| [鼠标](#鼠标--mouse) | 点击、shift 点击、滚轮 / click, shift-click, wheel |
| [状态图例](#状态图例--status-legend) | 状态列符号的含义 / what the status glyphs mean |
| [安全规则](#安全规则--safety-rules) | 哪些操作先确认 / which actions ask first |

三种约定 / Three conventions:

- **中文在前、英文在后**（`中文 English`）是界面所有文案的统一形式，窄列放不下时用
  `help::fit` 退化为纯中文。Chinese always comes first; narrow columns fall back to
  Chinese-only via `help::fit`.
- 并列可选的键写作 `a / b`。Parallel alternatives are written `a / b`.
- 组合键用 `+` 连接，不分隔：`ctrl+s`、`shift+tab`。Modifiers use `+` and stay glued.

---

## 主界面 / Main screen

没有弹层时的转发表格。The tunnel table with no overlay open.

| 按键 / Key | 中文 | English |
|---|---|---|
| `j` / `↓` | 向下移动光标 | Move the cursor down |
| `k` / `↑` | 向上移动光标 | Move the cursor up |
| `g` / `home` | 跳到首行 | Jump to the first row |
| `G` / `end` | 跳到末行 | Jump to the last row |
| `space` | 启用 / 停用当前行；运行中会先确认 | Enable / disable the row; asks first if it is running |
| `r` | 重启当前行；运行中先确认，未运行直接启动 | Restart the row; asks if running, starts it if not |
| `n` | 新建转发 | Create a tunnel |
| `e` / `E` | 编辑当前行（两个键同效） | Edit the selected row (`e` and `E` are aliases) |
| `d` | 删除当前行（先确认） | Delete the selected row (asks first) |
| `enter` / `l` | 打开详情 / 完整日志 | Open the detail / full-log overlay |
| `/` | 按名称或 SSH 主机过滤 | Filter by name or SSH host |
| `a` | 全部启用（先确认） | Enable all (asks first) |
| `x` | 全部停用（先确认） | Disable all (asks first) |
| `?` | 打开 / 关闭本帮助 | Open / close this help |
| `v` / `V` | 进入 / 退出多选模式（两个键同效） | Enter / leave selection mode (`v` and `V` are aliases) |
| `esc` | 清除过滤（不清除列表内容） | Clear the filter (it never clears the list) |
| `q` | 退出 sshpf（先确认） | Quit sshpf (asks first) |
| `ctrl+c` | 等同 `q`：先弹退出确认，**不会**直接退出 | Same as `q`: asks before quitting, never quits outright |

说明 / Notes:

- `a` 在没有未启用条目时只给提示，`x` 在没有运行中条目时只给提示，都不弹确认框。
  `a` only reports when nothing is off, and `x` only reports when nothing is running;
  neither opens a dialog in that case.
- **启动**某条未启用的转发不弹确认（可逆）；停用它才需要确认。
  Starting a disabled tunnel never asks (it is reversible); stopping one does.

## 多选模式 / Selection mode

按 `v` 进入。这不是"叠加键"，而是**重新绑定**——`space`、`j`/`k` 的语义都变了。
Entered with `v`. This **re-binds** keys rather than adding to them.

| 按键 / Key | 中文 | English |
|---|---|---|
| `v` / `V` / `esc` | 退出多选模式（标记清空） | Leave selection mode (marks are cleared) |
| `j` / `↓` / `k` / `↑` | 移动光标，已勾选的标记保持不变 | Move the cursor; existing marks stay |
| `g` / `G` / `home` / `end` | 跳到首行 / 末行 | Jump to the first / last row |
| `J` / `shift+↓` | 从锚点向下扩选到光标 | Extend the marks down from the anchor |
| `K` / `shift+↑` | 从锚点向上扩选到光标 | Extend the marks up from the anchor |
| `space` | 勾选 / 取消当前行（**不是**启停） | Mark / unmark this row (it does **not** start or stop it) |
| `a` | 全选切换，只作用于筛选后可见行 | Toggle select-all, visible rows only |
| `s` | 批量启用（先确认） | Batch start (asks first) |
| `x` | 批量停用（先确认） | Batch stop (asks first) |
| `R` | 批量重启（先确认） | Batch restart (asks first) |
| `D` | 批量删除（先确认） | Batch delete (asks first) |

说明 / Notes:

- 本模式下 `r` / `d` / `n` / `e` **不生效**，`space` 也不再启停隧道。底部提示会整体换掉，
  因为留着旧提示等于广告一批会做别的事或什么都不做的键。
  Here `r` / `d` / `n` / `e` do nothing and `space` no longer starts or stops; the footer
  is replaced entirely, because leaving the old hints would advertise keys that now do
  something else or nothing at all.
- `R` / `D` 用大写，是为了不与单行的 `r` / `d` 混淆。`R` / `D` are uppercase so they
  cannot be confused with the single-row `r` / `d`.
- 批量确认框里的条数是**实际会变化的条数**，不是勾选条数。
  The batch prompt counts the rows that will actually change, not all marked rows.

## 表单·导航态 / Form (navigating)

新建 / 编辑表单的默认阶段：方向键只移动高亮，**不会改动任何值**。
The default stage: arrows only move the highlight and never change a value.

| 按键 / Key | 中文 | English |
|---|---|---|
| `tab` / `shift+tab` | 下一个 / 上一个字段 | Next / previous field |
| `↓` / `↑` | 下移 / 上移字段高亮 | Move the field highlight down / up |
| `enter` | 执行字段动作：文本字段进入编辑；`类型` 循环下一类型；`自动重连` 取反；`SSH 主机` 展开下拉 | Act on the field: edit text, cycle `Type`, toggle auto-restart, open the host list |
| `←` / `h` | `类型` 字段：上一个类型（L/R/D）；`自动重连` 字段：取反 | On `Type`: the previous kind (L/R/D); on auto-restart: toggle |
| `→` / `l` | `类型` 字段：下一个类型（L/R/D）；`自动重连` 字段：取反 | On `Type`: the next kind (L/R/D); on auto-restart: toggle |
| `space` | `类型` / `自动重连` 字段：切换；`SSH 主机` 字段：展开 / 收起导入面板 | Cycle `Type`, toggle auto-restart, or toggle the import pane on `SSH host` |
| `i` | `SSH 主机` 字段：展开 / 收起导入面板 | On `SSH host`: toggle the import pane |
| `f2` / `ctrl+h` / `ctrl+o` | `SSH 主机` 字段：打开主机下拉（三个键同效） | On `SSH host`: open the dropdown (three equivalent keys) |
| `ctrl+s` / `f10` | 保存（`f10` 等价键）。新建：直接保存并立即建连；编辑：先确认，运行中的连接会重建 | Save (`f10` is an alias). Create connects at once; edit asks first and rebuilds a live tunnel |
| `esc` | 取消；有未保存内容会先确认 | Cancel; asks first if there are unsaved edits |

说明 / Notes:

- 保存选 **`ctrl+s` / `f10`** 而不是 `enter`，因为 `enter` 在这个阶段已经用来进入编辑。
  Save is `ctrl+s` / `f10` rather than `enter`, because `enter` already means "start editing".
- `space` 在导入面板打开后归面板（勾选当前行），收面板用 `i` 或 `esc`。
  Once the import pane is open `space` belongs to it (tick the cursor row); close it with
  `i` or `esc`.

## 表单·编辑态 / Form (editing)

某个文本字段正在接收输入。注意 `esc` 在这里**不关闭表单**，只是结束编辑。
A text field is taking input. `esc` here does **not** close the form, it only ends editing.

| 按键 / Key | 中文 | English |
|---|---|---|
| 任意字符 | 插入到光标处 | Type to insert at the cursor |
| `backspace` | 删除光标前一个字符 | Delete the character before the cursor |
| `delete` | 删除光标后一个字符 | Delete the character after the cursor |
| `←` / `→` | 光标左移 / 右移 | Move the cursor left / right |
| `home` / `end` | 光标到行首 / 行尾 | Cursor to the start / end of the line |
| `enter` / `esc` | 结束编辑，留在本字段 | Stop editing, stay on this field |
| `tab` / `shift+tab` | 结束编辑并切到下一 / 上一字段 | Stop editing and move to the next / previous field |
| `ctrl+s` / `f10` | 保存（结束字段编辑后提交） | Save (ends the field edit, then submits) |

说明 / Notes:

- `ctrl+s` / `f10` 在两个阶段都能保存：编辑态会先结束当前字段的编辑，再提交表单，
  所以**不会**把字母 `s` 打进字段。此前编辑态的 `ctrl+s` 会把 `s` 插进正在编辑的内容
  （静默的数据损坏），`f10` 则完全无反应——而提示行一直写着「`ctrl+s` 保存」。
  `ctrl+s` / `f10` save from both stages: in the editing stage the current field's edit is
  ended first, then the form is submitted, so the letter `s` is never typed into the field.
  This used to be broken — `ctrl+s` inserted `s` into the field being edited (silent data
  corruption) and `f10` did nothing at all, while the footer kept advertising `ctrl+s`.
- 在 `远端端口` 输入会联动推荐本地端口；手改过 `本地端口` 后不再被推荐覆盖。
  Typing the remote port derives the local-port suggestion; once the local port is
  hand-edited the suggestion stops overwriting it.
- 两段式 `esc`：第一次退出编辑，第二次才是"放弃并关闭表单"（有未保存内容会确认）。
  `esc` is two-stage: the first press leaves editing, the second requests discarding the form
  (which asks when there are unsaved edits).

## 主机下拉 / Host dropdown

表单里 `SSH 主机` 字段上的 `~/.ssh/config` 主机列表（内嵌下拉，不是独立弹窗）。
The host list on the `SSH host` field; it is inline, not a separate overlay.

| 按键 / Key | 中文 | English |
|---|---|---|
| `↓` / `j` | 下一台主机 | Next host |
| `↑` / `k` | 上一台主机 | Previous host |
| `pgdn` / `pgup` | 向下 / 向上翻一页 | Page down / up |
| `home` / `end` | 第一台 / 最后一台主机 | First / last host |
| 任意字符 | 过滤主机（别名 / 地址 / 用户名） | Filter hosts by alias, address or user |
| `backspace` | 删除一个查询字符 | Delete a query character |
| `enter` | 选定该主机并收起下拉 | Pick this host and collapse the list |
| `esc` | 收起下拉，**不改**字段值 | Collapse without changing the field |

## 导入面板 / Import pane

主机已经声明的转发列表。面板打开后，键位**归面板**——`j`/`k`、`↑`/`↓`、`g`/`G`、
`pgup`/`pgdn` 移动的是导入行，不再是表单字段。
The forwards the host already declares. Once open, navigation keys belong to the pane.

| 按键 / Key | 中文 | English |
|---|---|---|
| `j` / `↓` | 下一行 | Next row |
| `k` / `↑` | 上一行 | Previous row |
| `pgdn` / `pgup` | 向下 / 向上翻页 | Page down / up |
| `g` / `home` | 跳到首行 | Jump to the first row |
| `G` / `end` | 跳到末行 | Jump to the last row |
| `space` | 勾选 / 取消当前行 | Tick / untick the current row |
| `a` | 全选（只作用于筛选后可见行） | Tick all visible rows |
| `U` | 全不选（只作用于可见行） | Untick all visible rows |
| `I` | 反选（只作用于可见行） | Invert the visible rows |
| `f` | 切到下一列筛选（循环：类型→监听→目标→原始指令→类型） | Start filtering the next column (cycles) |
| `←` / `→` | 上一列 / 下一列筛选 | Previous / next filter column |
| `F` | 清除当前列筛选；没有筛选时提示「没有筛选可清除」 | Clear this column's filter; says so when there is none |
| `ctrl+r` | 清除**全部**筛选 | Clear **every** filter |
| `z` | 撤销上一次筛选改动（LIFO） | Undo the last filter change (LIFO) |
| `esc` | 先退出筛选输入；不在输入时收起面板 | Leave filter input first; otherwise close the pane |
| `i` | 收起面板 | Close the pane |
| `ctrl+s` | 保存：勾选 N 行就生成 N 条隧道 | Save: one tunnel per ticked row |

说明 / Notes:

- `space` 只能**展开**面板。面板打开后 `space` 已经被"勾选当前行"占用，再按一次会勾行而
  不是收起；收起用 `i` 或 `esc`，与展开它的键对称。
  `space` can only open the pane. Once open it means "tick this row", so a second `space`
  ticks instead of collapsing; close with `i` or `esc`, symmetric with the key that opened it.
- 代价是**字面字符 `f` 无法输入到筛选里**（`f` 优先切列）。每列内容是端口、主机名或指令名，
  `←`/`→` 加退格足够覆盖。
  The cost is that a literal `f` cannot be typed into a filter (`f` always switches columns).
  Every column holds a port, a host or a directive name, so `←`/`→` plus backspace cover it.
- `ctrl+s` 与 `tab` 不被面板吞掉，仍然和普通表单里一样工作——面板是**增量**，不是模态。
  `ctrl+s` and `tab` are not swallowed by the pane; it is an addition, not a modal.

## 导入列筛选 / Import column filter

某一列的筛选正在输入。`enter` / `esc` 只是结束输入，面板**不**收起。
One column's filter is being typed. `enter` / `esc` only end the input; the pane stays open.

| 按键 / Key | 中文 | English |
|---|---|---|
| 任意字符 | 追加到当前列筛选（实时过滤） | Append to the current filter (filters live) |
| `backspace` | 删除一个字符 | Delete a character |
| `enter` | 结束筛选输入（面板不收起） | End filter input (the pane stays open) |
| `esc` | 结束筛选输入（面板不收起） | End filter input (the pane stays open) |
| `←` / `→` | 换到上一列 / 下一列筛选 | Switch to the previous / next column |
| `ctrl+r` | 清除全部筛选 | Clear every filter |
| `ctrl+z` | 撤销上一次筛选改动 | Undo the last filter change |

## 确认框 / Confirmation dialog

所有破坏性或有副作用的操作都会先弹出它。
Shown before every destructive or side-effecting action.

| 按键 / Key | 中文 | English |
|---|---|---|
| `y` / `Y` / `enter` | 执行该操作 | Run the action |
| `n` / `N` / `esc` | 取消，什么都不做 | Cancel and change nothing |

说明 / Notes:

- 确认框打开时其它按键不生效，点击也不会穿透到下面的表格。
  While the dialog is open no other key does anything, and clicks cannot leak through to the
  table underneath.

## 详情 / 日志 / Detail / log

| 按键 / Key | 中文 | English |
|---|---|---|
| `j` / `↓` | 向下滚动 | Scroll down |
| `k` / `↑` | 向上滚动 | Scroll up |
| `pgdn` / `pgup` | 向下 / 向上翻页 | Page down / up |
| `q` / `esc` / `enter` / `l` | 关闭，回到主界面 | Close and go back |

失败原因与原始 ssh stderr 在这里完整保留；表格的「最后错误」列只放归类短句。
The classified failure reason and the raw ssh stderr are kept here in full; the table's
"Last error" column only carries the short classified phrase.

## 过滤输入 / Main filter

按 `/` 打开的对主表的过滤。

| 按键 / Key | 中文 | English |
|---|---|---|
| 任意字符 | 即时过滤（名称 / SSH 主机模糊匹配） | Filter live by fuzzy match on name and SSH host |
| `backspace` | 删除一个字符 | Delete a character |
| `enter` | 保留过滤并关闭输入 | Keep the filter and close the input |
| `esc` | 清除过滤并关闭输入 | Clear the filter and close the input |

三种约定 / Three conventions:

- **中文在前、英文在后**（`中文 English`）是界面所有文案的统一形式，窄列放不下时用
  `text::fit` 退化为纯中文。Chinese always comes first; narrow columns fall back to
  Chinese-only via `text::fit`.
- 并列可选的键写作 `a / b`。Parallel alternatives are written `a / b`.
- 组合键用 `+` 连接，不分隔：`ctrl+s`、`shift+tab`。Modifiers use `+` and stay glued.

---

## 帮助浮层 / Help overlay

按 `?` 打开的这份帮助。它按上下文分区，内容就是本文的数据来源。
This help, opened with `?`. It is split by context and is the data source for this page.

| 按键 / Key | 中文 | English |
|---|---|---|
| `j` / `↓` | 向下滚动一行 | Scroll down one line |
| `k` / `↑` | 向上滚动一行 | Scroll up one line |
| `pgdn` / `pgup` | 向下 / 向上翻页 | Page down / up |
| `?` / `esc` / `q` / `enter` | 关闭帮助 | Close the help |

说明 / Notes: 浮层每行是「键列 | 中文 | 英文」，键列宽度**实测**得出（目录里最宽的 binding +2，钳制 10~22 列且不超半行——写死 12 曾窄于 `g / G / home / end`（18）和 `J / K / shift+↓ / shift+↑`（25），于是绑定直接贴住说明）；放不下时中文先排、英文换到续行；超长组合键独占一行；note 行整行 prose。滚动上限按**折行后的实际行数**计算。渲染规则见 [tui.md](tui.md#浮层--overlays)。
Notes: key cell width is measured, English wraps to continuation lines, over-long bindings take a row of their own, and the scroll limit counts wrapped lines. See [tui.md](tui.md#浮层--overlays).

## 鼠标 / Mouse

鼠标捕获是 best-effort 开启的：终端拒绝捕获时只影响鼠标，键盘 UI 完整可用。
Mouse capture is best-effort; if the terminal refuses, only the mouse is affected.

| 位置 / Where | 中文 | English |
|---|---|---|
| 表格行 | 单击移动光标；多选模式下同时切换该行标记 | Click moves the cursor; in selection mode it also toggles that row's mark |
| 表格行 + `shift` | 从锚点扩选到该行 | Extend the marks from the anchor to that row |
| 表格列表空白处 | 滚轮在该列表内滚动，不改变选择 | The wheel scrolls within the list; no selection change |
| 导入表复选框 | 勾选 / 取消该行 | Tick / untick that row |
| 导入表行其它位置 | 把导入表光标移到该行 | Move the import cursor to that row |
| 导入表列头 | 开始筛选该列 | Start filtering that column |
| 筛选 chip 的 `×` | 删除该列筛选 | Drop that column's filter |
| `全选` / `反选` / `全不选` | 对应批量勾选（只作用于可见行） | The matching bulk tick, visible rows only |
| `清除全部筛选` / `撤销筛选` | 清空 / 撤销筛选；无内容时变暗且**不可点** | Clear / undo filters; dimmed and **not clickable** when there is nothing to act on |
| 多选动作条按钮 | 对应批量操作（**仍走确认闸**） | The batch operation, still behind the confirmation gate |
| 列表上的滚轮 | 滚动（主表移动光标，导入表移动其光标） | Scroll (the main table moves the cursor, the import list its own) |

- 点击落空**什么都不做**，不"透传"给下面的控件：按钮旁边的误点不该启动隧道。
  A click on empty space does **nothing** and never falls through to the widget below.
- 弹窗吞掉点击。确认框是在破坏性动作**之前**拦人的，一次本想点对话框的点击绝不能落到某行上。
  Popups swallow clicks: the confirmation gate must never mark or start a row behind it.

## 状态图例 / Status legend

| 符号 / Glyph | 中文 | English |
|---|---|---|
| `●` | 已连通 —— 转发已就绪，可以开始使用 | Up — the forward is ready to use |
| `◐` | 连接中 —— ssh 已启动，等待就绪标记 | Starting — ssh is up, waiting for the ready signal |
| `◌` | 重试 N —— 网络类失败，退避后自动重连（第 N 次） | Retrying N — network-class failure, reconnecting with backoff |
| `✗` | 失败 —— 配置类失败，改完配置需手动重启 | Failed — configuration-class failure, fix it and restart by hand |
| `○` | 未启用 —— 用户意图为关闭 | Off — the user intends it to be off |

## 安全规则 / Safety rules

- **先确认再动手**：删除、停用、重启、批量启停 / 重启 / 删除、保存编辑、放弃编辑、退出、
  `ctrl+c` —— 全部先弹确认框，`y` / `enter` 执行，`n` / `esc` 取消。
  Delete, stop, restart, batch start / stop / restart / delete, saving an edit, discarding
  edits, quitting and `ctrl+c` all ask first: `y` / `enter` runs it, `n` / `esc` cancels.
- **刻意不弹确认**（可逆、无副作用）：启动、新建、移动光标、过滤、帮助、详情。
  Deliberately no dialog: starting, creating, moving the cursor, filtering, help, detail.
- `NO_COLOR=1` 关闭全部颜色，状态仍由符号和文字表达。
  `NO_COLOR=1` disables all colour; glyphs and words still carry the status.
- 日志在 `~/.local/state/sshpf/logs/sshpf.log`；`SSHPF_LOG_DIR` / `SSHPF_LOG_LEVEL`
  可改目录与级别，详见 [logging.md](logging.md)。
  Logs live in `~/.local/state/sshpf/logs/sshpf.log`; `SSHPF_LOG_DIR` / `SSHPF_LOG_LEVEL`
  change the directory and level — see [logging.md](logging.md).

---

## 这张表怎么保证不遗漏 / How this list stays complete

`tests/help_coverage.rs` 会解析 `src/keys.rs` 里每个 handler 真实 dispatch 的
`KeyCode` 字面量，与帮助目录 `src/help.rs` **双向**比对，并检查这里是否覆盖了目录里的
每一个 token。三种漂移都会让测试失败：

1. 加了按键却忘了写帮助（**不得遗漏**）—— 用户的键做了事，帮助里却没有；
2. 帮助里写了代码没有的键（**不得撒谎**）—— 用户按下去什么都不发生，然后不再相信帮助；
3. 本文件落后于帮助目录 —— 界面上写对了，查手册却查不到。

`tests/help_coverage.rs` parses the `KeyCode` literals each handler in `src/keys.rs`
really dispatches, compares them **both ways** against `src/help.rs`, and checks that this
page covers every token in the catalog. All three kinds of drift fail the test.
