# docs/research/ — 历史调研档案

> **这份文档回答**：`docs/research/` 里这些文件是什么、还能不能当参考。
> **不负责**：任何当前行为、当前事实。
> **相关代码路径**：无。

## 定位 / What this is

**这里全部是历史调研笔记，不是当前事实来源。**

它们在 v1（用系统 `ssh` 子进程）走向纯 Rust 实现的过程中写成，用于记录当时的实测数据、API 核实与选型对比。结论已经被落实进代码（见 [../design.md](../design.md) 的决策索引），本文档集只保留**为什么当时那样判断**以及**测量证据**。

| 现在要知道什么 | 去哪 |
|---|---|
| 当前行为、规则、政策 | [../index.md](../index.md) 指向的文档 |
| 某个设计为什么这样 | [../design.md](../design.md) |
| 当时的实测数字与库缺陷复现 | 本目录 |

## 什么时候可以读 / When it is still useful

- **想复查某个库的缺陷证据**：`pure-rust-ssh-spike.md` §8 记录了 `russh` known_hosts 的四个实测缺陷（含 `@revoked` 放行）。
- **想知道为什么选 `ring` 而不是 `aws-lc-rs`**：跨平台构建的实验数据在 `pure-rust-ssh-plan.md`。
- **想看竞品与交互设计依据**：`prior-art.md`（含表格列宽、状态符号的来源）。

## 什么时候**不要**读 / When not to

- 查当前键位、配置字段、日志事件 —— 这些在代码与 [../keymap.md](../keymap.md)、[../logging/events.md](../logging/events.md)。
- 查当前的 `~/.ssh/config` 解析行为 —— 见 [../import-forwards.md](../import-forwards.md) 与 `src/ssh/`。
- 引用其中的版本号、行数、依赖版本 —— 全部可能已过期。

## 目录 / Contents

| 文件 | 内容 | 备注 |
|---|---|---|
| [prior-art.md](prior-art.md) | 同类工具盘点、竞品功能对照、TUI 交互设计调研 | 表格列宽与状态符号的出处 |
| [tech-options.md](tech-options.md) | 早期技术选型实测（TUI 栈、进程模型） | 部分结论已被后续方案取代 |
| [pure-rust-ssh-plan.md](pure-rust-ssh-plan.md) | 纯 Rust 化的**决策文档**（含 `ring` vs `aws-lc-rs` 实测） | 最接近当前实现的动机说明 |
| [pure-rust-ssh-russh.md](pure-rust-ssh-russh.md) | `russh` API 层面核实 | 版本号已过期，以 `Cargo.toml` 为准 |
| [pure-rust-ssh-config.md](pure-rust-ssh-config.md) | `~/.ssh/config` 解析、密钥、agent、known_hosts 生态调研 | `ssh2-config` 选型依据 |
| [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) | 三种转发的端到端实测 | §8 是库缺陷复现证据 |
| [client-server.md](client-server.md) | **client-server 架构调研总览**：为什么做、方案、代价、路线图、不做清单 | 另有 [client-server/](client-server/) 三篇展开与 [client-server-spike/](client-server-spike/) 实测 |
| [client-server/](client-server/) | 协议字段（`protocol.md`）、生命周期与选举（`lifecycle.md`）、迁移计划（`migration.md`） | 设计文档，非当前行为 |
| `client-server-spike/` | client-server 假设的可运行探针（52 条断言全 PASS，含真实多进程 e2e） | 独立工程，可丢弃 |
| `spike/` | 上述实测用的可丢弃工程与探针程序 | 不参与构建，不进 crates.io 包 |

`spike/` 不在 `cargo build` 的路径上：它的 `Cargo.toml` 是独立工程，`spike/sshd/run.sh` 起一个临时 sshd。不要把它当作可维护代码。

## 维护约定 / Rules

- **不更新**这些文件的内容去"跟上代码"——它们是历史记录，修改会破坏其证据价值。
- 需要新调研时**新建**文件并在此登记，不要覆写旧的。
- 若某个结论已被代码推翻，在本文"判断"上记录一句并把结论写进 [../design.md](../design.md)；**不要**改历史笔记。
- 发布包排除本目录（`Cargo.toml` 的 `exclude`），仓库里保留。

`client-server-spike/` 与 `spike/` 同理：独立 `Cargo.toml`，不在根 workspace，不参与 `cargo build`。
区别：`spike/` 记录 v1 时代对 `russh` 的实测；`client-server-spike/` 记录 client-server 方案的实测
（选举、广播、生命周期、codec、时钟），运行方式见其 `README.md`。

**注意定位**：[client-server.md](client-server.md) 及其子目录是**面向未来的设计调研**，
描述的是「打算怎么改」，不是当前行为。当前行为仍以 `src/` 与 [../index.md](../index.md) 为准。
