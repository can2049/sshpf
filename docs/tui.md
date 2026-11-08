# 渲染层与交互 / TUI

> **这份文档回答**：界面怎么画（布局、宽度、浮层、颜色）、鼠标点击怎么路由、窄终端/超长值怎么办、加一个可点控件要动哪些地方。
> **不负责**：键位全表（见 [keymap.md](keymap.md)）、每个按键的语义（见 [keymap.md](keymap.md) 与 `src/keys/`）、日志事件（见 [logging/events.md](logging/events.md)）。
> **相关代码路径**：`src/ui/`（纯渲染）、`src/ui/hit.rs`（命中图）、`src/app/mouse.rs`（鼠标派发）、`src/text.rs`（宽度/折行）。

## 三条铁律 / Three rules

1. **`ui` 是纯函数。** 不 spawn、不 I/O、不改隧道状态。唯一允许的可变操作是夹紧滚动偏移（`clamp_scrolls`）。
2. **颜色永远不是唯一信息载体。** 每个状态都有符号 + 文字；`NO_COLOR=1` 时界面仍完整可用。
3. **中文宽度按显示列算。** 全部中文用 `display_width`（CJK = 2 列），**绝不用** `str::len()`。

## 主界面布局 / Frame layout

`draw()`（`src/ui.rs`）的顺序：夹紧滚动 → 清空命中图 → 画静态区 → 画浮层 → 写回命中图。

静态区自上而下 `Layout::vertical`：

| 行 | 高度 | 内容 | 渲染器 |
|---|---|---|---|
| 0 | 1 | 标题：版本、ssh-config 主机数、配置路径、时钟 | `ui::header::draw_title` |
| 1 | 1 | 计数器：`● 2 up ◐ 0 starting ◌ 0 retrying ✗ 1 failed ○ 1 off` | `draw_counters` |
| 2 | `Min(1)` | 转发表格 | `ui::table::draw_table` |
| 3 | 1 | 消息条（`StatusMsg`） | `draw_message` |
| 4 | `select_mode` | 多选动作条（仅多选模式） | `ui::selection::draw_selection_bar` |
| 5 | 3 | 键提示 3 行（每条都双语） | `ui::hints::draw_keys` |

浮层按 `App.overlay` 画在最上层：`Help` / `Form` / `Confirm` / `Detail` / `Filter`。
浮层用 `centered_pct`（帮助 78×84%、详情 88×86%）或 `centered`（表单 74 列宽、确认框按文案宽度），
`clear_and_block` 先 `Clear` 再画边框——否则下面的表格会透出来。

**分区块取不到时**：`band(rows, i, "what")` 用 `rows.get(i)`，`None` 就记 `ui.render_degraded reason=layout_band_missing` 并返回零矩形（这一块这一帧不画），**不 panic**。

## 列宽与降级 / Columns

`src/ui/layout.rs` 定列、宽度预算与收缩顺序。列：

| 列 | 典型宽 | 内容 |
|---|---|---|
| `Name` | 14 | 隧道名 |
| `Kind` | 1 | `L` / `R` / `D` |
| `Local` | 17 | `127.0.0.1:8080` |
| `Remote` | 17 | `127.0.0.1:3000` / `(SOCKS5)` |
| `LocalRemote` | 20 | `8080→3000`，极窄终端下合并 |
| `SshHost` | 14 | `~/.ssh/config` 别名 |
| `Status` | 11 | 符号 + 双语短语 |
| `LastError` | ≤34 | 归类短句（弹性列，先吸收/让出余量） |
| `Uptime` | 10 | `1m 20s` |

牺牲顺序（写死的规格）：① 时长 → ② SSH 主机 → ③ 合并本地/远端。

| 终端宽度 | 保留列 |
|---|---|
| ≥110 | 全部 8 列 |
| ≥88 | 去掉 `Uptime` |
| ≥64 | 去掉 `SshHost` |
| <64 | 本地/远端合并成 `LocalRemote` |

**不变式**：`sum(widths) <= area_width`，且列不为空。所有 u16 运算先扩宽到 u32、`try_from` 收口。

## 颜色 / Palette

`src/ui/palette.rs`：`palette()` 读 `NO_COLOR`（设了就用无色档），`palette_with()` 供测试注入。

- 状态色只强化符号，不替代符号：`●` 绿 / `◐` 黄 / `◌` 黄 / `✗` 红 / `○` 灰。
- 表格行有三档视觉：**编辑态实心选中条 > 仅高亮加粗强调色 > 普通**。光标只在编辑态出现。
- 变暗（dimmed）用于不可用控件的**视觉**表达，同时该控件**不记 hit**（否则点击"看起来没反应"）。

## 文本与折行 / Text

所有字符串操作走 `src/text.rs`（唯一实现）：`display_width`、`char_width`、`truncate_to_width`、`pad_to_width`、`bi`、`fit`、`wrap`、`glob_match`。

| 场景 | 规则 |
|---|---|
| 双语标签 | `bi(zh, en)` → `"中文 English"`（中文在前，兼容既有断言） |
| 窄列放不下 | `fit(zh, en, sep, width)` 三级退化：双语 → 仅中文 → 截断 |
| **不做**半个英文短语 | 截断的英文句子比没有更糟，用户会以为那是完整信息 |
| 帮助浮层 | `wrap` 贪心折行；滚动上限按**折行后实际行数**算（渲染与限位共用 `help_lines`） |

`ui::text` 只是本层薄包装（`pair` / `fit_pair` / `fit_pair_spaced`），实现全在 `crate::text`。

## 命中图 / Hit map

`src/ui/hit.rs` 的 `HitMap` 每帧重建，记录「屏幕这块矩形是什么**意图**」，不是坐标本身：

```rust
ImportColumnHeader(ImportColumn)   // ← 而不是 x = 23
TunnelRow(TunnelId)                // ← 而不是行号
```

三条规则：

| 规则 | 为什么 |
|---|---|
| **只有渲染器写**，每帧开头 `clear()` | 陈旧的图会把点击送到上一帧的布局 |
| **后 push 的赢**（`entries.iter().rev().find(..)`） | 弹窗后画就自动遮蔽下面的表格，不需要 z-index 记账 |
| **零面积矩形忽略** | 隐藏的控件不能截走点击 |

**"后画就遮蔽"有个前提**：命中图是一个**栈**，不是 z-index。弹窗画在表格之后，如果它不记录自己的矩形，表格的矩形仍在栈顶，点击就穿过弹窗落到表格行上——视觉上盖住了，行为上没有。所以 `draw_help` / `draw_confirm` / `draw_detail` 都先把 popup 矩形记成 `HitTarget::Popup`（吞掉点击、什么也不做）再画内容。

鼠标处理器（`src/app/mouse.rs`）**从不计算几何**，只问"这个点是什么"然后调与键盘同一批 `App` 方法——所以鼠标免费继承键盘的安全规则（点批量删除按钮一样弹确认框）。

| 位置 | `HitTarget` |
|---|---|
| 主表行 / 列表区 | `TunnelRow(id)` / `TunnelList` |
| 导入行 / 复选框 / 列头 | `ImportRow(usize)` / `ImportCheckbox(usize)` / `ImportColumnHeader(Col)` |
| 筛选 chip `×` | `ImportFilterChip(Col)` |
| 导入页脚按钮 | `ImportClearFilters` / `ImportUndoFilter` / `ImportCheckAll` / `ImportInvert` / `ImportUncheckAll` |
| 导入列表区（滚轮） | `ImportList` |
| 多选动作条 / 退出 / 全选 | `BatchAction(action)` / `BatchExit` / `BatchSelectAll` |
| 浮层背景 | `Popup` |
| 过滤输入行 / 清除 | `FilterBar` / `FilterClear` |

点击落空**什么都不做**，不"透传"。`tests/help_coverage.rs` 断言**每个 `HitTarget` 变体**都在帮助的鼠标一节被说明——新增可点控件不写帮助会红。

## 浮层 / Overlays

| 浮层 | 渲染 | 关键约束 |
|---|---|---|
| 帮助 | `ui::help::draw_help` + `help_lines(width, palette)` | 键列宽度**实测**（目录里最宽的 binding +2，钳制 `[10, 22]` 且不超半行）；滚动上限 = `help_lines(..).len() - 可视行` |
| 表单 | `ui::form::draw_form` | 标签列宽度随弹窗宽度切换（双语 20 列 / 中文 8 列）；光标只在编辑态放置（`place_form_cursor`） |
| 主机下拉 | `ui::dropdown::draw_host_dropdown` | 内嵌在表单里，`HOST_DROPDOWN_ROWS = 8` 的滚动窗口；键处理与渲染**共用**这个常量 |
| 确认框 | `ui::confirm::draw_confirm` | 高度按文案**折行后**的行数自适应（`prompt_rows`），避免大片空白 |
| 详情 | `ui::detail::draw_detail` | `DETAIL_LOG_TAIL = 15` 行日志尾巴；`tunnel_command` 给出等价 `ssh` 命令行 |
| 导入面板 | `ui::import::draw_pane` | 面板嵌在表单内，`IMPORT_ROWS_VISIBLE = 8` 行滚动窗口 |

帮助浮层的精确排版规则（键列测量、超长 binding 独占一行、note 行整行 prose）见 [design.md](design.md#d13-帮助是数据不是散文) 与 `src/ui/help.rs`。

## 鼠标捕获 / Mouse capture

- 在 `main.rs` 里 **best-effort** 开启：终端拒绝捕获（某些 multiplexer、CI pty）时忽略失败，记 `ui.no_mouse`（DEBUG），键盘 UI 完整可用。
- 退出前必定 `DisableMouseCapture`——留着它会让重新获得控制权的 shell 每次鼠标移动都打印转义字符。
- `mobile` 事件同样走命中图；点击落空/命中变暗控件都是 no-op。

## 降级 / Degradation

渲染层每帧都可能触发边界条件，所以**限流**：`ui.render_degraded` 每进程每处只记一次（`ui::text::warn_once` + 各自的 `static SEEN`）。否则一次 resize 就能把 2 MiB 日志刷满，把真正有意思的历史挤掉。

| `reason` | 触发 | 后果 |
|---|---|---|
| `layout_band_missing` | `Layout::split` 少给了分区 | 该块这一帧不画 |
| `width_overflow` | 文本显示宽度超 `u16` | 截断渲染 |
| `row_overflow` | 行号 + 表头顶出 `u16` | 视为屏幕外，不进命中图 |

**降级不是静默的**：每一处都有一条可 grep 的记录。排查见 [logging.md](logging.md#排查配方--recipes)。

## 加一个可点控件 / Add a clickable control

1. `src/ui/hit.rs`：加 `HitTarget` 变体（带**语义**载荷，不是坐标）。
2. `src/ui/<panel>.rs`：画完后 `hits.push(rect, HitTarget::… )`；不能点时**不要** push。
3. `src/app/mouse.rs`：在 dispatch 里接住它，调与键盘相同的方法（不要新写业务逻辑）。
4. `src/help.rs` 的 `SectionId::Mouse`：加一行说明——否则 `tests/help_coverage.rs` 的
   `every_hit_target_is_described_in_the_mouse_section` 会红。
5. `docs/keymap.md` 的鼠标表加一行。

回归测试先例：`keys::tests::a_modal_swallows_clicks_instead_of_passing_them_to_the_table`——在弹窗背后放一个表格行，断言点击被消费、光标不动、也没有行被标记。
