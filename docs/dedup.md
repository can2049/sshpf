# 转发去重 / Forward dedup

> **这份文档回答**：新增一条转发时，怎么判定「已经存在完全一样的」；三个创建入口（导入面板、对端端口批量、手填表单）各自怎么反馈；为什么 `-L` 的本地监听端口不算身份。
> **不负责**：`~/.ssh/config` 内部的**行**去重（见 [import-forwards.md](import-forwards.md#行去重--row-dedup)）、端口顺延规则（见 [import-save.md](import-save.md#本地端口顺延--local-port-shifting)）、`ctrl+s` 的归属路由（见 [import-save.md](import-save.md#提交归属默认提议-vs-明确指令)）。
> **相关代码路径**：`src/model/tunnel.rs::forward_identity`（身份定义）、`src/app/duplicate.rs`（两个查找）、`src/app/import.rs` / `src/app/port_batch.rs` / `src/app/form_submit.rs`（三个调用点）。

新增转发时，如果**已有完全一样的**，新增的不生效。防的失败模式很具体：同一个转发被建两遍，两条隧道抢同一个意图，用户还得手动删掉一条，且删错会丢配置。

## 两层去重，别混为一谈 / Two layers, two questions

| 层 | 问题 | 判据 | 位置 |
|---|---|---|---|
| **行去重** | 同一条**指令**被多个 `Host` 块声明了几次 | `SshForward::key()`，逐字比较原始指令 | `ssh::forwards::dedup` |
| **转发去重**（本页） | 这个转发是否**已经存在**（已保存的隧道，或本批次前面的行） | `TunnelConfig::forward_identity()`，比**意图**不比端口 | `app::duplicate` |

行去重发生在列表里（同一份 `~/.ssh/config` 内部）；转发去重发生在**创建时**，跨「上次运行留下的 `config.toml`」与「这一次的面板/表单」。两层都要：第二层能看见的第一类重复（已保存条目）在第一层根本不可见。

## `forward_identity()`：意图身份

```text
-L:  L|ssh_host|local_addr|remote_host:remote_port      ← 不含 local_port
-R:  R|ssh_host|remote_host:remote_port|local_addr:local_port
-D:  D|ssh_host|local_addr:local_port
```

`-L` 的本地监听端口**故意不参与**身份。因为它是一个**环境会改写的派生值**：`plan_local_port` 在端口被占时向上顺延，而顺延后的数字会被**持久化**（原始请求不可恢复）。所以"已保存 18790、这次请求 18789、远端都是 18789"这种形态很常见，任何依赖 `saved.local_port == requested` 的判定都会漏掉它。

实测到的重复就是这么产生的：

```text
已有： cloud_pc-L-18789-2   local 127.0.0.1:18789 → remote 127.0.0.1:18789
导入： LocalForward 18789 127.0.0.1:18789   （18789 被占 → 本地顺延成 18790）
结果： 又多一条 cloud_pc-L-18789  local 127.0.0.1:18790 → remote 127.0.0.1:18789
        ← 同一个转发，两个条目
```

`-R` / `-D` 的本地端口仍在身份里：`-R` 两端都是意图（服务端监听端 + 本地目标端），`-D` 的本地监听端就是全部意图。

`ssh_host` 一律参与：同一组端口在不同主机上是完全不同的转发。

**已知边界（刻意）**：因为 `-L` 不含本地端口，`LocalForward 8080 host:80` 与 `LocalForward 9090 host:80` 会被判成同一个转发，只保留一条。取舍理由：同一目标开两个本地监听是冗余的；反过来漏判重复会让用户**根本清不掉**重复条目。真的需要两个本地监听，请改 `local_addr`/远端目标。

## 三个入口的行为 / What each entry point does

| 入口 | 行为 |
|---|---|
| 批量导入（`~/.ssh/config` 面板） | 跳过重复行、创建其余；状态栏「…；跳过 N 条重复转发」。**全部**重复时创建 0 条、状态栏「N 条转发已存在，未重复创建」并关闭表单 —— 这不是错误 |
| 按对端扫描端口批量建 | 同上 |
| 单条手填表单 | **不创建**，表单里报「已存在相同的转发「X」，未重复创建」 |

两处细节值得记住：

- **单条路径的重复判定先于监听地址冲突判定**。重打一条已有转发同时也会撞监听端口，但"你已经有这个转发了"才描述真实情况，"监听地址冲突"只描述症状。
- **编辑已有条目不是创建**，`duplicate_of` 跳过候选自己的 id，所以"改个名字再保存"不会被判成重复。

## 留痕 / Log trail

每次跳过记一条 `event=action.duplicate_skipped`（INFO，字段 `source` / `name` / `existing`）：

```text
INFO seq=12 event=action.duplicate_skipped source=import name=cloud_pc-L-18789 existing=cloud_pc-L-18789-2
```

`source` 是三个稳定值之一：`form` / `import` / `port-scan`。所以"我勾了 3 条怎么只建了 2 条"能在日志里找到答案，见 [logging/events.md](logging/events.md)。
