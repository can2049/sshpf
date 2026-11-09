# 文档导航（给 AI agent 的入口）/ Docs index

> **这份文档回答**：我想做 X，该读哪一篇、看哪个文件？
> **不负责**：任何具体规则、设计理由、键位表——这里只有**索引**。
> **相关代码路径**：无（纯导航）。代码地图在 [architecture.md](architecture.md)。

读法：先在这一页找到「我想…」那一行，跳到对应文档；不要在仓库里盲搜。

## 三条使用规则 / Three rules

1. **代码是唯一事实来源。** 文档与代码冲突时以代码为准，并回来修文档。
2. **同一事实只详述一处。** 其余地方用链接——看到被复制两遍的规则，是缺陷。
3. **`docs/research/` 是历史调研，不是当前事实。** 见 [research/README.md](research/README.md)。

## 我想… / Task → doc → code

| 我想… / I want to… | 读哪篇 / Read | 看哪个文件 / Code |
|---|---|---|
| 了解整体分层、有哪些模块 | [architecture.md](architecture.md) | `src/lib.rs` |
| 决定「新功能该放哪」 | [architecture.md](architecture.md#新功能放哪--where-does-new-code-go) | `src/*/mod.rs` 头注释 |
| 加一个按键 / 改键位 | [agents.md](agents.md#加按键--add-a-key) → [keymap.md](keymap.md) | `src/keys/`、`src/help.rs` |
| 加一个界面面板 / 改渲染 | [tui.md](tui.md) | `src/ui/` |
| 改鼠标点击行为 | [tui.md](tui.md#命中图--hit-map) | `src/ui/hit.rs`、`src/app/mouse.rs` |
| 改隧道启动 / 停止 / 重试 | [runtime.md](runtime.md) | `src/app/runtime.rs`、`src/tunnel.rs` |
| 改 SSH 连接 / 认证 / 主机密钥 | [runtime.md](runtime.md) 与 [design.md](design.md)；主机密钥策略见 [security.md](security.md#主机密钥--host-keys) | `src/ssh/`、`src/ssh/hostkey_conf.rs` |
| 加一个日志事件 / 改字段 | [agents.md](agents.md#加日志事件--add-a-log-event) → [logging/events.md](logging/events.md) | `src/eventlog/` |
| 查某个 `event=` 是什么意思 | [logging/events.md](logging/events.md) | `src/eventlog/` |
| 排查日志（重连风暴、崩溃、降级） | [logging.md](logging.md#排查配方--recipes) | `src/logging/` |
| 理解「为什么不 panic」「unsafe 在哪」 | [security.md](security.md) | `src/logging/crash.rs` |
| 改崩溃捕获 / 权限 / 日志脱敏 | [security.md](security.md) | `src/logging/`、`src/sys/` |
| 加测试 / 跑测试 / 测试放哪 | [testing.md](testing.md) | `src/**/#[cfg(test)]`、`tests/` |
| 理解某条设计为什么这样 | [design.md](design.md) | — |
| 改 `~/.ssh/config` 导入逻辑 | [import-forwards.md](import-forwards.md) | `src/ssh/forwards.rs`、`src/app/import.rs` |
| 查键位 / 帮助文案 | [keymap.md](keymap.md) | `src/help.rs`、`src/keys/` |
| 改配置字段 / 持久化 | [README.md](../README.md#配置--configuration) | `src/config.rs` |
| 改双语文案 / 宽度 / 截断 / 折行 | [agents.md](agents.md#改文案--change-user-facing-text) | `src/text.rs` |
| 提交前自检 | [agents.md](agents.md#提交前自检--pre-flight-checklist)、[testing.md](testing.md) | — |

## 文档清单 / What each doc is for

| 文档 | 回答什么问题 | 不负责 |
|---|---|---|
| [index.md](index.md) | 我该读哪篇 | 任何细节 |
| [architecture.md](architecture.md) | 分层、模块地图、依赖方向、新代码放哪 | 具体实现细节 |
| [runtime.md](runtime.md) | 隧道生命周期、就绪判定、重试、事件泵 | 渲染、日志格式 |
| [tui.md](tui.md) | 渲染规则、布局、命中图、浮层 | 键位全表（在 keymap） |
| [security.md](security.md) | panic/unsafe 政策、崩溃捕获、权限、脱敏 | 日志事件目录 |
| [testing.md](testing.md) | 测试分层、怎么跑、新测试放哪、CI | 具体断言内容 |
| [logging.md](logging.md) | 日志格式、级别、怎么排查 | 事件目录（在 logging/events.md） |
| [logging/events.md](logging/events.md) | 每个 `event=` 的级别/字段/含义 | 格式与排查配方 |
| [keymap.md](keymap.md) | 全部按键与鼠标操作 | 实现与设计理由 |
| [import-forwards.md](import-forwards.md) | 导入规则、去重、坑、批量创建 | 表单交互细节 |
| [design.md](design.md) | 关键设计决策索引（1 段 + 链接） | 任何展开叙述 |
| [agents.md](agents.md) | 改码/写作约定与 checklist | 项目背景 |
| [research/README.md](research/README.md) | 历史调研的定位与目录 | 当前行为 |
| [../README.md](../README.md) | 用户视角：装、用、配、排查 | 内部结构 |

## 代码目录速查 / Repository at a glance

| 路径 | 一句话 |
|---|---|
| `src/main.rs` | 进程入口 + 事件循环 |
| `src/cli.rs` | 参数解析 + `USAGE` 双语文本 |
| `src/app/` | 状态机：持有隧道、消费事件、确认闸、表单、多选、鼠标 |
| `src/keys/` | 按键分发（每个上下文一个 handler 文件） |
| `src/ui/` | 纯渲染（ratatui），每帧重建命中图 |
| `src/tunnel.rs` | 隧道监督：一线程一 runtime |
| `src/ssh/` | `~/.ssh/config`、known_hosts、SOCKS5、russh 客户端 |
| `src/model/` | 纯数据：隧道、导入表、状态、表单、失败分类 |
| `src/config.rs` | TOML 持久化（原子写、`0600`） |
| `src/logging/` | 落盘日志 + 崩溃捕获（唯一允许 `unsafe`） |
| `src/eventlog/` | `event=` 词汇表（唯一真相） |
| `src/help/` | 双语帮助目录（数据，不是散文；按上下文分文件） |
| `src/text.rs` | 唯一的字符串层：宽度/截断/双语/折行/glob |
| `src/sys/` | 平台差异：文件权限、退出信号 |

完整逐文件地图（含「放什么 / 谁在用 / 加东西放哪」）见 [architecture.md](architecture.md#模块地图--module-map)。

## 维护本页 / Keeping this index honest

- 新增文档 ⇒ 同时加进上面两张表；删文档 ⇒ 先全仓 `grep` 掉引用。
- 每篇文档开头必须有 3–8 行「回答什么 / 不负责什么 / 相关代码路径」。
- 单篇 ≤ 400 行；超过就按职责再拆一篇并登记到本页。
- 路径引用必须真实存在：文档间的 md 链接与代码文件路径由自检脚本校验（见 [agents.md](agents.md#提交前自检--pre-flight-checklist)）。
