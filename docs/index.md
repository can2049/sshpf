# 文档导航（给 AI agent 的入口）/ Docs index

> **这份文档回答**：我想做 X，该读哪一篇、看哪个文件？
> **不负责**：任何具体规则、设计理由、键位表——这里只有**索引**。
> **相关代码路径**：无（纯导航）。粗粒度代码地图在 [architecture.md](architecture.md#模块地图--module-map)。

读法：先在这一页找到「我想…」那一行，跳到对应文档；不要在仓库里盲搜。

## 三条使用规则 / Three rules

1. **代码是唯一事实来源。** 文档与代码冲突时以代码为准，并回来修文档。
2. **同一事实只详述一处。** 其余地方用链接——看到被复制两遍的规则，是缺陷。
3. **`docs/research/` 是历史调研，不是当前事实。** 见 [research/README.md](research/README.md)。

## 我想… / Task → doc → code

| 我想… / I want to… | 读哪篇 / Read | 看哪个文件 / Code |
|---|---|---|
| **要动代码（新特性 / 修 bug）** | [agents.md](agents.md#一特性一-worktree--one-worktree-per-change) — 先开 worktree，合并后清理 | `.agents/worktrees/` |
| 了解整体分层、进程模型、有哪些目录 | [architecture.md](architecture.md) | `src/lib.rs` |
| 决定「新功能该放哪」 | [architecture.md](architecture.md#新功能放哪--where-does-new-code-go) | `src/*/mod.rs` 头注释 |
| 加一个按键 / 改键位 | [agents.md](agents.md#加按键--add-a-key) → [keymap.md](keymap.md) | `src/keys/`、`src/help.rs` |
| 加一个界面面板 / 改渲染 | [tui.md](tui.md) | `src/ui/` |
| 改鼠标点击行为 | [tui.md](tui.md#命中图--hit-map) | `src/ui/hit.rs`、`src/app/mouse.rs` |
| 改隧道启动 / 停止 / 重试 | [runtime.md](runtime.md) | `src/app/runtime.rs`、`src/tunnel.rs` |
| 改连通性检测 / 休眠后不重连 | [runtime.md](runtime.md#健康检查--health-checks) | `src/ssh/client/health.rs` |
| 改「谁拥有隧道」/ 多窗口共享状态 | [client-server.md](client-server.md) | `src/server/`、`src/client/`、`src/app/mirror.rs` |
| 改 client-server 协议（消息/字段/版本） | [client-server.md](client-server.md) 与 [research/client-server/protocol.md](research/client-server/protocol.md) | `src/ipc/` |
| 查「第二个窗口为什么连不上 / 状态为什么不一致」 | [client-server.md](client-server.md#排障--recipes) 与 [logging/events.md](logging/events.md) 的 `server.*`、`client.*` | `src/client/`、`src/server/election.rs` |
| 改 SSH 连接 / 认证 / 主机密钥 | [runtime.md](runtime.md) 与 [design.md](design.md)；主机密钥策略见 [security.md](security.md#主机密钥--host-keys) | `src/ssh/`、`src/ssh/hostkey_conf.rs` |
| 加一个日志事件 / 改字段 | [agents.md](agents.md#加日志事件--add-a-log-event) → [logging/events.md](logging/events.md) | `src/eventlog/` |
| 查某个 `event=` 是什么意思 | [logging/events.md](logging/events.md) | `src/eventlog/` |
| 排查日志（重连风暴、崩溃、降级） | [logging.md](logging.md#排查配方--recipes) | `src/logging/` |
| 理解「为什么不 panic」「unsafe 在哪」 | [security.md](security.md) | `src/logging/crash.rs` |
| 改崩溃捕获 / 权限 / 日志脱敏 | [security.md](security.md) | `src/logging/`、`src/sys/` |
| 加测试 / 跑测试 / 测试放哪 | [testing.md](testing.md) | `src/**/#[cfg(test)]`、`tests/` |
| 理解某条设计为什么这样 | [design.md](design.md) | — |
| **想弄懂「导入 / 新增」整条链** | [import.md](import.md) — 发现 → 面板 → 保存 → 对端端口 | `src/ssh/forwards.rs`、`src/app/import*`、`src/model/import/` |
| 改 `~/.ssh/config` 的**发现**逻辑（作用域/Include/哪些行可导入） | [import.md](import.md#从-sshconfig-发现--discovery) | `src/ssh/forwards.rs`、`src/ssh/forwards/` |
| 改导入面板的**交互**（筛选/全选/自动展开） | [import.md](import.md#面板--the-pane) | `src/app/import_pane.rs`、`src/model/import/` |
| 改 `ctrl+s` **保存**导入行（字段映射/端口顺延/命名） | [import.md](import.md#保存归属--who-wins-the-save) | `src/app/import.rs`、`src/app/form_submit.rs` |
| 改「新增时怎么判定已有相同转发」 | [dedup.md](dedup.md) | `src/model/tunnel.rs`、`src/app/duplicate.rs` |
| 改「已经重复的条目怎么办」 | [dedup.md](dedup.md#已写进配置的重复载入时折叠--fold-duplicates-on-load) | `src/model/dedup.rs`、`src/config.rs` |
| 改「导入面板里哪一行不能勾」 | [import.md](import.md#已存在的行--rows-that-already-exist) | `src/model/import/row.rs`、`src/app/import_pane.rs` |
| 改「手填表单与导入面板谁赢 `ctrl+s`」 | [import.md](import.md#提交归属默认提议-vs-明确指令) | `src/app/form_submit.rs`、`src/model/import/table.rs` |
| 改「对端监听端口」选择 / 扫描 | [import.md](import.md#对端监听端口--the-peers-listening-ports) | `src/ssh/probe.rs`、`src/app/port_scan.rs`、`src/model/portscan.rs` |
| 查键位 / 帮助文案 | [keymap.md](keymap.md) | `src/help.rs`、`src/keys/` |
| 改配置字段 / 持久化 | [../README.md](../README.md#配置--configuration) | `src/config.rs` |
| 改双语文案 / 宽度 / 截断 / 折行 | [agents.md](agents.md#改文案--change-user-facing-text) | `src/text.rs` |
| 提交前自检 | [agents.md](agents.md#提交前自检--pre-flight-checklist)、[testing.md](testing.md) | — |

## 文档清单 / What each doc is for

| 文档 | 回答什么问题 | 不负责 |
|---|---|---|
| [index.md](index.md) | 我该读哪篇 | 任何细节 |
| [architecture.md](architecture.md) | 分层、依赖方向、进程模型、目录级模块地图、新代码放哪 | 逐文件说明（在 `//!` 头注释里） |
| [runtime.md](runtime.md) | 隧道生命周期、就绪判定、重试、事件泵 | 渲染、日志格式 |
| [tui.md](tui.md) | 渲染规则、布局、命中图、浮层 | 键位全表（在 keymap） |
| [security.md](security.md) | panic/unsafe 政策、崩溃捕获、权限、脱敏、主机密钥策略 | 日志事件目录 |
| [testing.md](testing.md) | 测试分层、怎么跑、新测试放哪、CI | 具体断言内容 |
| [logging.md](logging.md) | 日志格式、级别、怎么排查 | 事件目录（在 logging/events.md） |
| [logging/events.md](logging/events.md) | 每个 `event=` 的级别/字段/含义 | 格式与排查配方 |
| [keymap.md](keymap.md) | 全部按键与鼠标操作 | 实现与设计理由 |
| [import.md](import.md) | **导入与新增全链**：发现、作用域、行去重、面板交互、`touched`、筛选、`ctrl+s` 归属、字段映射、端口顺延、对端端口扫描 | 转发身份去重与载入折叠（在 dedup） |
| [dedup.md](dedup.md) | 转发**身份**去重：判据、三个入口、留痕、已写进配置的重复怎么折叠 | 行去重与面板交互（在 import） |
| [client-server.md](client-server.md) | 一个 server 多个 client：角色、选举、传输层、令牌、宽限窗口、命令重新校验 | 帧格式与消息字段（在 research 协议文档） |
| [design.md](design.md) | 关键设计决策索引（1 段 + 链接） | 任何展开叙述 |
| [agents.md](agents.md) | 改码/写作约定与 checklist | 项目背景 |
| [research/README.md](research/README.md) | 历史调研的定位与目录 | 当前行为 |
| [../README.md](../README.md) | 用户视角：装、用、配、排查 | 内部结构 |

## 代码目录速查 / Repository at a glance

**逐目录职责见 [architecture.md](architecture.md#模块地图--module-map)。逐文件「放什么 / 边界在哪」不用查文档：读那个文件自己的 `//!` 头注释**（`tests/safety_guards.rs` 强制每个源文件都有）。

## 维护本页 / Keeping this index honest

- 新增文档 ⇒ 同时加进上面两张表；删文档 ⇒ 先全仓 `grep` 掉引用。
- 每篇文档开头必须有 3–8 行「回答什么 / 不负责什么 / 相关代码路径」。
- 单篇 ≤ 400 行；**不要因为主题不同就拆文件**——一个读者任务（例如"导入怎么工作"）应该落在一篇里。拆篇的判据是两个任务，不是两个子话题。
- 路径引用必须真实存在：文档间的 md 链接与代码文件路径由自检脚本校验（见 [agents.md](agents.md#提交前自检--pre-flight-checklist)）。
