# 转发去重 / Forward dedup

> **这份文档回答**：新增一条转发时怎么判定「已经存在完全一样的」；三个创建入口各自怎么反馈；为什么 `-L` 的本地监听端口不算身份；**已经写进 `config.toml` 的重复条目怎么清掉**。
> **不负责**：`~/.ssh/config` 内部的**行**去重（见 [import.md](import.md#行去重--row-dedup)）、端口顺延规则（见 [import.md](import.md#本地端口顺延--local-port-shifting)）、`ctrl+s` 的归属路由（见 [import.md](import.md#提交归属默认提议-vs-明确指令)）。
> **相关代码路径**：`src/model/tunnel.rs::forward_identity` / `identity_of`（身份定义）、`src/model/import.rs::SshForward::forward_identity`（未规划端口的同一身份）、`src/app/duplicate.rs`（三个查找）、`src/model/dedup.rs`（载入时折叠）、`src/app/import.rs` / `src/app/port_batch.rs` / `src/app/form_submit.rs`（三个调用点）。

新增转发时，如果**已有完全一样的**，新增的不生效。防的失败模式很具体：同一个转发被建两遍，两条隧道抢同一个意图，用户还得手动删掉一条，且删错会丢配置。

## 两层去重，别混为一谈 / Two layers, two questions

| 层 | 问题 | 判据 | 位置 |
|---|---|---|---|
| **行去重** | 同一条**指令**被多个 `Host` 块声明了几次 | `SshForward::key()`，逐字比较原始指令 | `ssh::forwards::dedup` |
| **转发去重**（本页） | 这个转发是否**已经存在**（已保存的隧道，或本批次前面的行） | `TunnelConfig::forward_identity()`，比**意图**不比端口 | `app::duplicate` |

行去重发生在列表里（同一份 `~/.ssh/config` 内部）；转发去重发生在**创建时**，跨「上次运行留下的 `config.toml`」与「这一次的面板/表单」。第二层能看见的第一类重复（已保存条目）在第一层根本不可见。

还有**第三层，方向相反**：前两层拦「即将新建的重复」，[载入时折叠](#已写进配置的重复载入时折叠--fold-duplicates-on-load) 处理「已经在 `config.toml` 里的重复」——它们在守卫出现之前就写进去了，守卫永远拦不到。

## forward_identity意图身份

```text
-L:  L|ssh_host|local_addr|remote_host:remote_port      ← 不含 local_port
-R:  R|ssh_host|remote_host:remote_port|local_addr:local_port
-D:  D|ssh_host|local_addr:local_port
```

`-L` 的本地监听端口**故意不参与**：它是**环境会改写的派生值**——`plan_local_port` 在端口被占时向上顺延，而顺延后的数字会被**持久化**（原始请求不可恢复）。实测的重复就是这么产生的：

```text
已有： cloud_pc-L-18789-2   local 127.0.0.1:18789 → remote 127.0.0.1:18789
导入： LocalForward 18789 127.0.0.1:18789   （18789 被占 → 本地顺延成 18790）
结果： 又多一条 cloud_pc-L-18789  local 127.0.0.1:18790 → remote 127.0.0.1:18789
        ← 同一个转发，两个条目
```

任何依赖 `saved.local_port == requested` 的判定都会漏掉它。`-R` / `-D` 的本地端口仍在身份里（`-R` 两端都是意图，`-D` 的本地监听端就是全部意图）；`ssh_host` 一律参与——同一组端口在不同主机上是完全不同的转发。

**已知边界（刻意）**：`LocalForward 8080 host:80` 与 `LocalForward 9090 host:80` 会被判成同一个转发，只保留一条。取舍：同一目标开两个本地监听是冗余的；反过来漏判重复会让用户**根本清不掉**重复条目。真需要两个本地监听，请改 `local_addr`/远端目标。

## 身份有两个入口，算法只有一个 / One recipe, two entry points

`forward_identity()` 需要 `TunnelConfig`，而导入面板要问的是一行**尚未变成配置**的 `LocalForward`——先建配置就要跑 `plan_local_port`，那会探测端口、可能顺延，于是答案会随当前谁在监听而变。所以 `model::tunnel::identity_of` 是唯一的规则实现，两个入口都调它：

| 调用方 | 名字 | 本地端口从哪来 |
|---|---|---|
| 已保存/待建配置 | `TunnelConfig::forward_identity()` | 配置自己的 `local_port` |
| 发现到的行（面板、导入前） | `SshForward::forward_identity(bind)` | 指令里写的端口，**不规划** |

`-L` 的身份里没有本地端口，所以两入口对 `-L` 完全等价；`-R`/`-D` 的本地端口就是意图，两入口都用「用户请求的那个端口」。方向映射（`-R` 两端对调）只有 `SshForward::ends` 一份，`app::import` 在它之上再加端口规划。

## 三个入口的行为 / What each entry point does

| 入口 | 行为 |
|---|---|
| 批量导入（`~/.ssh/config` 面板） | **已保存的行在列表里就不勾选**（见 [import.md](import.md#已存在的行--rows-that-already-exist)），所以默认不会提出重复；若仍被跳过（同批次内重复），状态栏「…；跳过 N 条重复转发」。**全部**无效时创建 0 条、状态栏「N 条转发已存在，未重复创建」并关闭表单 —— 这不是错误 |
| 按对端扫描端口批量建 | 同上 |
| 单条手填表单 | **不创建**，表单里报「已存在相同的转发「X」，未重复创建」 |

两处细节值得记住：**单条路径的重复判定先于监听地址冲突判定**（重打一条已有转发也会撞监听端口，但"你已经有这个转发了"才描述真实情况，"监听地址冲突"只描述症状）；**编辑已有条目不是创建**（`duplicate_of` 跳过候选自己的 id，所以"改个名字再保存"不会被判成重复）。

## 留痕 / Log trail

每次跳过记一条 `event=action.duplicate_skipped`（INFO，字段 `source` / `name` / `existing`）：

```text
INFO seq=12 event=action.duplicate_skipped source=import name=cloud_pc-L-18789 existing=cloud_pc-L-18789-2
```

`source` 是三个稳定值之一：`form` / `import` / `port-scan`。所以"我勾了 3 条怎么只建了 2 条"能在日志里找到答案，见 [logging/events.md](logging/events.md)。

## 已写进配置的重复：载入时折叠 / Fold duplicates on load

守卫只在**创建时**起作用，已经存在的重复条目它看不见——用户实测的那份配置就是这样：`cloud_pc` 的两条转发各被历史导入复制成 3 条（`18789` / `18790` / `18791` 都指向 `127.0.0.1:18789`），共 6 条，全部在运行，开了 6 个 SSH 连接。不修的话这 6 条是**永久的**：表格显示 6 行、拨 6 条连接，删掉多余的只到下次扫描为止。

所以 `config::load` 读完就折叠一次，规则在 `model::dedup::fold_duplicates`：**按 `forward_identity()` 分组，每组保留文件顺序里的第一条，其余移除**。

| 问题 | 答案 |
|---|---|
| 为什么「第一条」而不是「最老的 / 端口没被占的 / 已启用的」 | 三条都不需要看代码就能被用户预测。`created_at` 可被手改成 0；「端口没被占」随谁在监听而变，重启两次折出两种结果；「已启用」等于擅自启动用户关掉的监听。文件顺序就是表格顺序，**顶部那条留下**是唯一可预测的规则 |
| 被移除的条目会丢数据吗 | 不丢。留下那条是完整的 `TunnelConfig`；其余与它身份等价，本地端口本就是派生值 |
| 会改 `~/.ssh/config` 吗 | **不会**。那条指令可能是用户在 sshpf 之外的唯一记录，面板照旧列出它并标「已存在」 |
| 会重新规划端口吗 | **不会**。幸存者保留持久化里的端口——启动时悄悄改端口是比重复更坏的意外 |
| 用户怎么知道 | 每组记一条 `event=config.deduped`（WARN，字段 `kept` / `dropped`），启动后状态栏说明「已合并 N 条重复转发」。表格无故变短会被读成丢数据 |

`config::load` 返回 `ConfigFile`（旧签名不变），`load_with_report` 额外返回 `Vec<Folded>` 供 `App` 在启动后播报。

**磁盘上的文件什么时候才变干净**：载入是**只读**的——`--check`、`--list-hosts` 都不改写用户的文件。折叠结果随下一次 `persist` 落盘，而那段代码本来就在写配置（建/删/启停隧道）。默认 `restore_on_start = true` 时启动恢复会触发一次 `persist_quietly`，所以重启一次文件就干净了；手写 `restore_on_start = false` 又不做任何改动的一次会话，文件保持原样——再次启动会再折一次，结果相同（幂等）。不选"载入即写盘"是因为：启动时改写用户文件比重复条目更需要对得上账。
