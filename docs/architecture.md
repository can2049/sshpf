# 架构 / Architecture

> **这份文档回答**：分几层、依赖方向是什么、进程模型是什么、**哪个目录负责什么**（粗粒度）、新代码该放哪。
> **不负责**：逐文件说明——那个权威在**每个文件自己的 `//!` 头注释**里；具体业务流程见 [runtime.md](runtime.md)、[tui.md](tui.md)、[import.md](import.md)。
> **相关代码路径**：`src/lib.rs`（模块声明）、`src/<层>.rs`（各层地图）。

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
| `app`, `keys`, `ui` | `tunnel`、`config`、`ssh`、叶子层 | `ui` 调 `App` 的**变更**方法、`keys` 画界面、`ui`/`keys` 互相调用 |
| `tunnel`, `config` | `ssh`、叶子层 | `app`、`ui`、`keys` |
| `ssh` | `model`、`text`、`logging`、`eventlog`、`sys` | `app`、`ui`、`keys`、`tunnel`（`TunnelEvent` 在 `model`，就是为了不用依赖 `tunnel`） |
| 叶子层 | 彼此之间尽量少，且只向下 | 任何上层 |

**这次重构修掉的三个违规**（保留作判例，避免回潮）：

| 违规 | 修法 |
|---|---|
| `ssh/client.rs` → `crate::app::log`（底层调上层） | 事件词汇搬到顶层 `src/eventlog.rs`，`app::log` 不再存在 |
| `model.rs` → `crate::help::bi`（数据层调文案层） | `bi` 移到 `src/text.rs`，`help::bi` 变成 re-export（唯一保留的例外，新代码用 `text::bi`） |
| `ui/import.rs`、`ui/layout.rs` → `help::` 的渲染无关内容 | 改用 `crate::text::` |

## 进程模型 / Process model

**单进程：一个 `sshpf` 进程拥有全部隧道，不做进程间协调。** 没有后台服务进程，也没有视图
进程——你启动的那个交互进程就是唯一持有转发、唯一写 `config.toml` 的进程。

| 模式 | 谁启动 | 拥有隧道？ | 写 `config.toml`？ | 跑在哪 |
|---|---|---|---|---|
| 交互 TUI（默认） | 用户运行 `sshpf` | **是** | **是**（唯一写者） | 两平台 |
| `--check` / `--list-hosts` | 同一命令加参数 | 否（只读） | 否 | 两平台 |

由此得到两条语义，别按旧模型理解：

- **退出即全停**：进程退出时由它自己停掉全部隧道（`app.shutdown` → `tunnel.stop` / `tunnel.exit`），
  不存在"最后一个窗口退出才关服"这回事。
- **`--no-restore` 只影响本进程**：它表示"本进程启动时不自动恢复 enabled 的转发"，没有
  "传给谁"的问题。

同一份 `config.toml` 被两个进程同时打开时，两边各自持有自己的隧道并各自写盘——监听端口会撞车，
后写者覆盖前写者。这不是支持的多窗口共享，而是**两份独立状态**；要做到界面里只有一份真相，就只开
一个进程。

## 模块地图 / Module map

**这张表只到目录一级。** 想知道某个文件放什么、边界在哪，读该文件顶部的 `//!`——那是唯一权威（`tests/safety_guards.rs` 强制每个源文件都有）。

| 目录 | 一句话 |
|---|---|
| `src/main.rs`、`src/cli.rs`、`src/cli/args.rs`、`src/lib.rs` | 事件循环 + 信号、`USAGE` 双语文本 + 非 UI 模式、clap flag 声明与错误映射、`pub mod` 声明 |
| `src/app/` | 状态机：持有隧道、消费事件、确认闸、表单、多选 |
| `src/keys/` | 按键分发，一个上下文一个 handler 文件 |
| `src/ui/` | 纯渲染（ratatui），每帧重建命中图 |
| `src/tunnel.rs` | 隧道监督：一线程一 `current_thread` runtime |
| `src/config.rs` | TOML 持久化（原子写；Unix `0600`，Windows 靠 ACL） |
| `src/ssh/` | `~/.ssh/config`、known_hosts、SOCKS5、端口探测、russh 客户端 |
| `src/model/` | 纯数据：隧道、导入表、状态、表单、失败分类 |
| `src/text.rs` | 唯一的字符串层：宽度/截断/双语/折行/glob |
| `src/logging/` | 落盘日志 + 崩溃捕获（**唯一允许 `unsafe`**） |
| `src/eventlog/` | `event=` 词汇表（唯一真相） |
| `src/help/` | 双语帮助目录（数据，不是散文） |
| `src/sys/` | 平台差异：文件权限（Unix mode bit / Windows 空实现）、退出信号（Unix 多 `SIGHUP`） |

`src/<层>.rs` 是**模块根**：只做地图 + re-export + 入口函数，不堆实现。（全仓只有一个 `mod.rs` 例外：`src/sys/mod.rs`。）

### keys 键分发 / `src/keys`

| 文件 | handler | 上下文 / help section |
|---|---|---|
| `src/keys.rs` | `handle_key` / `handle_mouse` + 上下文路由表 | 全局（`ctrl+c` 归 Safety） |
| `src/keys/main.rs` | `handle_main_key` | 主界面 |
| `src/keys/select.rs` | `handle_select_key` | 多选模式 |
| `src/keys/form.rs` | `handle_form_key`、`handle_field_navigating/editing`、`activate_field`、`adjust_field`、`move_field`、`edit_field` | 表单（导航 / 编辑） |
| `src/keys/import.rs` | `handle_import_key`、`handle_import_filter_key`、`active_filter_column` | 导入面板 / 导入列筛选 |
| `src/keys/ports.rs` | `handle_port_key` + 移动/行两个 helper | 对端端口列表 |
| `src/keys/dropdown.rs` | `handle_dropdown_key` | 主机下拉 |
| `src/keys/misc.rs` | `handle_help_key`、`handle_detail_key`、`handle_confirm_key`、`handle_filter_key` | 帮助 / 详情 / 确认 / 过滤 |
| `src/keys/testkit.rs` | `#[cfg(test)]` 共享夹具，**无生产代码** | 测试夹具 |

**加键 = 改两个文件**：handler 里加 dispatch，`src/help.rs` 对应 section 加一行。漏一个 `tests/help_coverage.rs` 会红。见 [agents.md](agents.md#加按键--add-a-key)。

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
| 新的失败分类 | `src/model/failure.rs`（`name()`/`short()`/`short_en()`/`is_retryable()` 四处） | [logging/events.md](logging/events.md) 的 `kind` 稳定值列表 |
| 新的确认闸动作 | `src/app/confirm.rs` 的 `ConfirmAction` + 文案 | `src/keys/` 触发点；`help` 的 Safety section |
| 新的批量动作 | `src/model/batch.rs` 的 `BatchAction` | `src/app/select.rs`、`src/ui/selection.rs`、[keymap.md](keymap.md#多选模式--selection-mode) |
| 新的界面面板（浮层） | `src/app.rs` 的 `Overlay` 变体 + `src/ui/<name>.rs` + `src/keys/<name>.rs` | `HitTarget` 变体 + help section + keymap（清单见 [tui.md](tui.md#加一个可点控件--add-a-clickable-control)） |
| 新的导入列 | `src/model/import/row.rs` 的 `ImportColumn` | `src/ui/import/row.rs`、`src/keys/import.rs`、`src/model/import/filter.rs`、[import.md](import.md) |
| 新的日志事件 | `src/eventlog/<族>.rs` 一个函数 | [logging/events.md](logging/events.md) 一行 |
| 新的连通性判定 / 探活 | `src/ssh/client/health.rs`（判据与 `PING_TIMEOUT`） | `src/ssh/client/driver.rs` 的循环 race、[runtime.md](runtime.md#健康检查--health-checks) |
| 新的配置项 | `src/config.rs` `Defaults` | [../README.md](../README.md#配置--configuration)、`config.defaults_clamped` 若需钳制 |
| 新的字符串工具 | `src/text.rs`（唯一实现） | 单测放同文件尾部 |
| 新的平台差异 | `src/sys/mod.rs` + `unix.rs`/`windows.rs` | Windows CI 必须仍能编译 |

## 单文件与单函数上限 / Size limits

生产代码（不含 `#[cfg(test)]`）：

| 限制 | 值 | 谁来守 |
|---|---|---|
| 单文件生产行数 | ≤ 400 | `tests/safety_guards.rs` 断言（`MAX_PROD_LINES`） |
| 单函数行数 | ≤ 60 | `tests/safety_guards.rs` 断言（`MAX_FN_LINES`，测试函数豁免） |
| 模块根（`src/ui.rs` 等） | 只做地图 + 入口 | 评审；它是 agent 的路标，不要堆实现 |

豁免要写进 `tests/safety_guards.rs` 的 `SIZE_EXEMPT` 并注明理由——**不是** `#[allow]`。
