# 架构 / Architecture

> **这份文档回答**：本项目分几层、每层的模块叫什么名字、依赖方向是什么、新代码该放哪个文件。
> **不负责**：具体业务流程（见 [runtime.md](runtime.md)、[tui.md](tui.md)）、设计历史（见 [design.md](design.md)）。
> **相关代码路径**：`src/lib.rs`（模块声明）、`src/*/mod.rs`（各层地图）。

## 分层 / Layers

依赖**只许向下**。反向依赖（下层 `use crate::` 上层）一律视为缺陷。

```text
   main / cli            进程入口：参数、终端、信号、事件循环
        │
   app    keys    ui     app=状态机  keys=键分发  ui=纯渲染
        │      │      │
        └──────┴──────┘
               ▼
   tunnel    config       tunnel=线程/runtime 监督   config=TOML 持久化
               │
               ▼
        ssh               russh 客户端 / known_hosts / SOCKS5 / forwards
               │
               ▼
   model   text   logging   sys   eventlog
   （叶子层：纯数据 / 纯字符串 / 落盘日志 / 平台 / 事件词汇）
```

| 层 | 允许依赖 | 绝不允许 |
|---|---|---|
| `main`, `cli` | 任何层 | — |
| `app`, `keys`, `ui` | `tunnel`、`config`、`ssh`、叶子层 | 互相依赖（`ui` 不调 `app`，`keys` 不画界面） |
| `tunnel`, `config` | `ssh`、叶子层 | `app`、`ui`、`keys` |
| `ssh` | `model`、`text`、`logging`、`eventlog`、`sys` | `app`、`ui`、`keys`、`tunnel` |
| 叶子层 | 彼此之间尽量少，且只向下 | 任何上层 |

**本次重构修掉的三个违规**（保留作判例）：

| 违规 | 修法 |
|---|---|
| `ssh/client.rs` → `crate::app::log`（底层调上层） | 事件词汇搬到顶层 `src/eventlog.rs`，`app::log` 不再存在 |
| `model.rs` → `crate::help::bi`（数据层调文案层） | `bi` 移到 `src/text.rs`，`help::bi` 变成 re-export |
| `ui/import.rs`、`ui/layout.rs` → `help::` 的渲染无关内容 | 改用 `crate::text::` |

**唯一例外**：`help::bi` 仍存在（`pub use crate::text::bi;`），供尚未迁移的调用点使用。新代码用 `text::bi`。

## 模块地图 / Module map

表列：**文件 → 放什么 → 谁在用 → 加新东西放哪**。这是给 agent 的定位表，读代码前先读它。

### 入口 / Entry

| 文件 | 放什么 | 谁在用 | 加新东西放哪 |
|---|---|---|---|
| `src/main.rs` | `main()` 调度 + 事件循环 + 信号注册（311 行） | 二进制 | 只加"进程级"编排 |
| `src/cli.rs` | 参数解析、`USAGE` 双语文本、`--check`/`--list-hosts`（367 行） | `main` | 新 flag → 这里，并同步 `USAGE` |
| `src/lib.rs` | `pub mod` 声明 + 顶层 `pub use` | 二进制与集成测试 | 新模块 → 这里登记 |

### app 状态机 / `src/app/`

| 文件 | 放什么 | 谁在用 | 加新东西放哪 |
|---|---|---|---|
| `src/app.rs` | `App` 结构、`Overlay`、`ConfirmAction`、常量（`MAX_EVENTS_PER_FRAME`、`HOST_DROPDOWN_ROWS`、重试边界） | `main`、`keys`、`ui` | 新的 App 级状态字段 |
| `src/app/query.rs` | 只读查询：选中、过滤后可见行、计数器 | `app`、`ui` | 新的纯读取访问器 |
| `src/app/overlay.rs` | `open_*` / `close_overlay`（浮层唯一开关） | `keys`、`app` | 新的浮层打开/关闭 |
| `src/app/helpers.rs` | 无 `App` 依赖的纯函数：视图构造、config repair、`backoff` | `app`、`main` | 纯计算 |
| `src/app/import_pane.rs` | 导入面板的**交互**半边：展开/收起、列筛选、光标、勾选、「已存在」标记的来源 | `keys::import`、`ui::import` | 面板交互规则 |
| `src/app/runtime.rs` | 启停/重启、`start_tunnel_with`（`preflight()` + `launch()`） | `app` | 隧道"启动/停止"这一侧 |
| `src/app/events.rs` | 事件泵的**读**侧：`drain_events`、`on_ready`/`on_line`/`on_exit`、`flush_pending_exits`、`tick_retries`/`arm_retry` | `app` | 事件 → 状态转移、退避规则 |
| `src/app/persist.rs` | `persist`（config.toml）、`save_state`（state.toml）、`persist_quietly` | `app` | 落盘策略 |
| `src/app/form.rs` | 表单**状态** + 校验（纯状态机，不 spawn、不落盘） | `app`、`keys::form` | 表单字段与校验规则 |
| `src/app/form_submit.rs` | 表单**提交**：校验 → 持久化 → supervisor（create 直接保存，edit 确认后重建），以及「手填 vs 导入面板谁赢 `ctrl+s`」的路由 | `keys`、`app` | 提交顺序与副作用、提交归属 |
| `src/app/duplicate.rs` | 转发**身份去重**：`duplicate_of` / `duplicate_in_batch`（只比 `forward_identity`，不做 I/O） | `form_submit`、`import`、`port_batch` | 新的重复判定入口；身份定义在 `model::tunnel` |
| `src/app/import.rs` | 导入的**提交**半边：发现主机转发、批量校验、`config_from_forward`、建隧道 | `app` | 导入语义与校验 |
| `src/app/port_scan.rs` | 对端端口扫描的**交互**半边：`p` 打开、扫描线程/代次、光标与勾选 | `keys::ports`、`app` | 扫描状态与面板交互 |
| `src/app/port_batch.rs` | 对端端口批量的**提交**半边：勾选端口 → 每条一条 `-L` 隧道（全有或全无） | `app::form_submit` | 批量提交语义与命名 |
| `src/app/confirm.rs` | 确认闸：`ask_confirm`、文案、`apply` | `app`、`keys::misc` | 新受保护操作 → `ConfirmAction` 一个变体 |
| `src/app/select.rs` | 多选模式：标记集合、批量派发 | `app`、`keys::select` | 新批量动作 → `BatchAction` |
| `src/app/mouse.rs` | 鼠标：查 `HitMap` → 调与键盘同一批 `App` 方法 | `main` | 新的可点语义 |
| `src/app/picker.rs` | 主机下拉：过滤、移动、接受 | `app`、`keys::dropdown` | 下拉行为 |

### keys 键分发 / `src/keys/`

| 文件 | handler | 上下文 / help section |
|---|---|---|
| `src/keys.rs` | `handle_key` / `handle_mouse` + 上下文路由表 | 全局（`ctrl+c` 归 Safety） |
| `src/keys/main.rs` | `handle_main_key` | 主界面 |
| `src/keys/select.rs` | `handle_select_key` | 多选模式 |
| `src/keys/form.rs` | `handle_form_key`、`handle_field_navigating/editing`、`activate_field`、`adjust_field`、`move_field`、`edit_field` | 表单（导航 / 编辑） |
| `src/keys/import.rs` | `handle_import_key`、`handle_import_filter_key`、`active_filter_column` | 导入面板 / 导入列筛选 |
| `src/keys/dropdown.rs` | `handle_dropdown_key` | 主机下拉 |
| `src/keys/misc.rs` | `handle_help_key`、`handle_detail_key`、`handle_confirm_key`、`handle_filter_key`、`status_level_name`、`confirm_is_delete` | 帮助 / 详情 / 确认 / 过滤 |
| `src/keys/testkit.rs` | `#[cfg(test)]` 共享夹具（`app_with`、key 构造器、场景构造器），**无生产代码** | 测试夹具 |

**加键 = 改两个文件**：handler 里加 dispatch，`src/help.rs` 对应 section 加一行。漏一个 `tests/help_coverage.rs` 会红。见 [agents.md](agents.md#加按键--add-a-key)。

### ui 渲染 / `src/ui/`

| 文件 | 放什么 |
|---|---|
| `src/ui.rs` | 模块地图 + `draw()` 入口（81 行，只做编排） |
| `src/ui/frame.rs` | `draw()` 主流程、`clamp_scrolls`、`band()`、`centered()`、`popup_block()` |
| `src/ui/palette.rs` | `Palette`、颜色、`fmt_uptime` / `fmt_unix` / `cell()` |
| `src/ui/text.rs` | 本层包装（`pair`/`fit_pair`/`fit_pair_spaced` → `crate::text`）+ `warn_once` |
| `src/ui/layout.rs` | 列定义 `Col`、`column_layout()`、宽度预算 |
| `src/ui/hit.rs` | `HitTarget` + `HitMap`（每帧重建的语义命中图） |
| `src/ui/header.rs` | 标题、计数器、消息条 |
| `src/ui/hints.rs` | 底部提示行（3 行）、`flow_hints` |
| `src/ui/table.rs` | 转发表格、状态短句 |
| `src/ui/selection.rs` | 多选动作条 |
| `src/ui/help.rs` | 帮助浮层（`help_lines` 与滚动上限共用） |
| `src/ui/form.rs` | 新建/编辑表单 |
| `src/ui/dropdown.rs` | 主机下拉 |
| `src/ui/confirm.rs` | 确认框 |
| `src/ui/detail.rs` | 详情 + 完整日志 |
| `src/ui/filter.rs` | 过滤输入行 |
| `src/ui/import.rs` | 导入面板根 + `draw_pane` + `draw_empty` |
| `src/ui/ports.rs` | 对端监听端口面板（扫描中/失败/空/行列表 + 页脚键位） |
| `src/ui/import/body.rs` | 行渲染、滚动窗口 |
| `src/ui/import/header.rs` | 列头、chips、按钮 |
| `src/ui/import/footer.rs` | 页脚：勾选统计、hint 打包折行 |
| `src/ui/import/row.rs` | `ColSpec`、列宽计算、`row_line`、`cell_span` |
| `src/ui/import/cursor.rs` | `LineCursor` |
| `src/ui/test_support.rs` | `#[cfg(test)]` 渲染测试夹具（`TestBackend` 包装） |

渲染规则（不改状态、`NO_COLOR`、CJK 宽度、命中图纪律）见 [tui.md](tui.md)。

### 其余层 / The rest

| 文件 | 放什么 |
|---|---|
| `src/tunnel.rs` | `Supervisor`、`TunnelEvent`、一线程一 `current_thread` runtime、`catch_unwind` |
| `src/config.rs` | `ConfigFile`、`Defaults`、`Paths`、原子写、`0600`/`0700` |
| `src/ssh.rs` | `ssh` 层地图 |
| `src/ssh/config.rs` | `~/.ssh/config` → 主机与有效连接参数（`ssh2-config`），`strip_match_blocks` |
| `src/ssh/forwards.rs` | `LocalForward`/`RemoteForward`/`DynamicForward` 行扫描、作用域、去重 |
| `src/ssh/known_hosts.rs` | 主机密钥校验（自写）、`HostKeyPolicy`、`@revoked` |
| `src/ssh/socks5.rs` | `-D` 的 SOCKS5 服务端（仅 `CONNECT` + `NOAUTH`） |
| `src/ssh/client.rs` | 模块地图 + `run()` + `pub use Options/classify` |
| `src/ssh/client/classify.rs` | `russh::Error` → `FailureKind`、`classify_text`、`short_reason` |
| `src/ssh/client/handler.rs` | `SharedVerdict`、`Handler`（`check_server_key`、`forwarded-tcpip`） |
| `src/ssh/client/auth.rs` | `Auth`、`load_auth`、`connect_agent`、`expand_path`、`authenticate` |
| `src/ssh/client/connect.rs` | `timeout_connect`、`session()`、`connect()`（含 `ProxyJump` 链） |
| `src/ssh/client/driver.rs` | `serve_local` / `serve_remote` / `serve_dynamic` |
| `src/ssh/probe.rs` | 对端端口探测：`list_command`、`interpret`、`scan_listen_ports`、`ScanOutcome` |
| `src/ssh/client/probe.rs` | 走隧道同一条 SSH 路径执行命令并取回 stdout |

### 叶子层 / Leaves

| 文件 | 放什么 | 加东西放哪 |
|---|---|---|
| `src/model.rs` | 模块地图 + `mod`/`pub use`（63 行，只做导航） | 新类型 → 下表对应文件 |
| `src/model/tunnel.rs` | `ForwardKind`、`TunnelConfig`（含 `forward_identity`）、`identity_of`（唯一身份算法，`SshForward` 也用它） | 隧道定义字段、身份规则 |
| `src/model/dedup.rs` | `fold_duplicates`、`Folded`：载入时把「同一个转发」的多条已保存条目折成一条 | 折叠规则（哪条留下、怎么报告） |
| `src/model/import.rs` | `SshForward`（含 `ends` / `forward_identity`：未规划端口的转发身份）、`ForwardEnds`、`LocalPortReason`、`PORT_SCAN_LIMIT`、`plan_local_port`、`col_match` | 导入的发现规则、方向映射与端口规则 |
| `src/model/import/row.rs` | `ImportColumn`、`ImportRow`（一行怎么显示、怎么勾，含 `existing` / `creatable`：已保存的转发列出但不可创建） | 可筛选列 / 每行标志 |
| `src/model/import/table.rs` | `ImportTable`：候选行、光标、滚动、选中、`touched`、`with_existing` | 行与选中状态 |
| `src/model/import/filter.rs` | `ImportTable` 的逐列筛选 + 撤销栈 | 筛选/撤销行为 |
| `src/model/portscan.rs` | `RemotePort`、`PortRow`、`PortPick`、`PortScanState` | 对端端口与选择器状态 |
| `src/model/portscan/parse.rs` | `ss` / `netstat` 文本 → `RemotePort`（不可信输入） | 端口解析规则 |
| `src/model/portscan/declared.rs` | `declared_ports` / `filter_declared`：排除 ssh config 已列出的端口 | 端口去重规则 |
| `src/model/fixtures.rs` | `#[cfg(test)]` 专用共享夹具（不进 release） | 测试夹具 |
| `src/model/batch.rs` | `MultiSelect`、`BatchAction` | 批量动作 |
| `src/model/failure.rs` | `FailureKind`、`FailureInfo` | 新失败分类（同步 `kind()`/`short()`/`displays`） |
| `src/model/status.rs` | `TunnelStatus`、`LogLevel`、`LogLine`、`LogRing`、`TunnelView` | 状态与内存日志 |
| `src/model/form.rs` | `FormField`、`FormMode`、`InputBuf` | 表单字段 |
| `src/model/picker.rs` | `HostPicker` | 下拉状态 |
| `src/model/msg.rs` | `StatusMsg`、`now_hms`、`now_unix` | 状态栏消息 |
| `src/model/match.rs` | `fuzzy_match` | 模糊匹配 |
| `src/text.rs` | `display_width`、`char_width`、`truncate_to_width`、`pad_to_width`、`bi`、`fit`、`wrap`、`glob_match` | **任何**字符串操作 |
| `src/help.rs` | 模块地图 + `SECTIONS` 组装、`bi`/`fit` re-export、`SectionId` | 帮助目录的入口 |
| `src/help/main.rs` | 主界面、多选模式两节 | 这两个上下文的文案 |
| `src/help/form.rs` | 表单导航 / 编辑、主机下拉三节 | 表单相关文案 |
| `src/help/import.rs` | 导入面板、导入列筛选两节 | 导入相关文案 |
| `src/help/dialogs.rs` | 确认框、详情、过滤、帮助浮层四节 | 弹层文案 |
| `src/help/notes.rs` | 鼠标、状态图例、安全规则三节 | 跨上下文 prose（note 行） |
| `src/logging.rs` | 模块地图 + 公共 API（`Level`/`Config`/`logger`/`init`/`info`/`warn`…）+ 全局状态 | 见 `logging/*` |
| `src/logging/level.rs` | `Level`、`Config`、`default_dir`、`level_from_env` | 级别与配置 |
| `src/logging/sink.rs` | `Sink`、`Logger`、`CriticalSection`、`record`、`rotate`、`open_append`、`rewire_crash_fd` | 写入与轮转 |
| `src/logging/format.rs` | `now_timestamp`、`token`、`push_value`（字段转义） | 格式 |
| `src/logging/panic.rs` | `install_panic_hook`、`log_panic`、`payload_text`、`write_panic_record`、`backtrace` | panic 通路 |
| `src/logging/crash.rs` | **唯一 `unsafe` 文件**：signal handler + `set_tui_active` | 见文件头 SAFETY CONTRACT |
| `src/eventlog.rs` | `event=` 值的唯一真相 + `pub use` | 新事件 → 下表对应族 |
| `src/eventlog/process.rs` | 进程/配置类：`app_start`、`app_exit`、`config_loaded`… | 进程与配置事件 |
| `src/eventlog/tunnel.rs` | 隧道生命周期：`tunnel_start_requested`、`tunnel_ready`、`tunnel_retry_abandoned`… | 隧道事件 |
| `src/eventlog/ssh.rs` | SSH 配置与主机密钥类 | SSH 事件 |
| `src/eventlog/user.rs` | 用户动作、界面降级：`render_degraded`、`slow_frame`、`mouse_capture_failed`… | 用户动作与降级 |
| `src/sys/mod.rs` | `signal_name` 平台函数 + 转发到 `unix`/`windows` | 新平台差异 |
| `src/sys/unix.rs` / `src/sys/windows.rs` | `quit_signals`、`create_private_file`、`open_private_append`、`restrict_file`、`restrict_dir`、`perms_too_open`（Windows 侧为空实现/`Ctrl-C` 子集） | — |

## 依赖规则 / Dependency rules

1. **叶子层不许 `use crate::app` / `ui` / `keys` / `tunnel`。** 需要数据就定义在 `model`。
2. **`ui` 是纯函数。** 除 `clamp_scrolls` 外不写状态；不 spawn、不 I/O。
3. **`ssh` 不读环境变量以外的全局状态**；不 import `logging` 的事件词汇（用 `eventlog` 的函数或返回 `FailureInfo`）。
4. **事件词汇只在 `eventlog` 定义。** 业务代码写 `eventlog::tunnel_ready(..)`，不写 `logging::info("tunnel.ready", ..)`。
5. **字符串操作只在 `text`。** 不要在 `ui` / `help` / `ssh` 里再写第三份宽度表或折行。
6. **跨层的"数据"必须过 `model`。** 不要在 `ui` 里 import `ssh` 的私有类型。

## 新功能放哪 / Where does new code go

| 需求 | 放哪 | 还要动 |
|---|---|---|
| 新的隧道字段 | `src/model/tunnel.rs` + `src/config.rs`（serde 默认值） | `src/ui/import/row.rs` 若影响展示；`--check` 校验 |
| 新的失败分类 | `src/model/failure.rs`（`kind()`/`short()`/`displays()` 三处） | `docs/logging/events.md` 的 `kind` 稳定值列表 |
| 新的确认闸动作 | `src/app/confirm.rs` 的 `ConfirmAction` + 文案 | `src/keys/` 触发点；帮助 Safety section |
| 新的批量动作 | `src/model/batch.rs` 的 `BatchAction` | `src/app/select.rs`、`src/ui/selection.rs`、keymap |
| 新的界面面板（浮层） | `src/app.rs` 的 `Overlay` 变体 + `src/ui/<name>.rs` + `src/keys/<name>.rs` | `HitTarget` 变体 + help section + keymap |
| 新的导入列 | `src/model/import/row.rs` 的 `ImportColumn` | `src/ui/import/row.rs`、`src/keys/import.rs`、`src/model/import/filter.rs` |
| 新的日志事件 | `src/eventlog/<族>.rs` 一个函数 | `docs/logging/events.md` 一行 |
| 新的配置项 | `src/config.rs` `Defaults` | README 配置段、`config.defaults_clamped` 若需钳制 |
| 新的字符串工具 | `src/text.rs`（唯一实现） | 单测放同文件尾部 |
| 新的平台差异 | `src/sys/mod.rs` + `unix.rs`/`windows.rs` | Windows CI 必须仍能编译 |

## 单文件与单函数上限 / Size limits

生产代码（不含 `#[cfg(test)]`）：

| 限制 | 值 | 谁来守 |
|---|---|---|
| 单文件生产行数 | ≤ 400 | `tests/safety_guards.rs` 断言（`MAX_PROD_LINES`） |
| 单函数行数 | ≤ 60 | `tests/safety_guards.rs` 断言（`MAX_FN_LINES`，测试函数豁免） |
| 模块根（`src/ui.rs` 等） | 只做地图 + `draw()` 之类入口 | 评审；它是 agent 的路标，不要堆实现 |

豁免要写进 `tests/safety_guards.rs` 的 `SIZE_EXEMPT` 并注明理由——**不是** `#[allow]`。
