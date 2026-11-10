# client-server 架构调研总览 / Client-server study

> **这份文档回答**：为什么要把"每个窗口自己转发"改成 client-server、实测证据是什么、有哪些代价、哪些事**不做**。
> **不负责**：当前行为——见 [../client-server.md](../client-server.md)（该文是当前事实来源）；协议字段见 [client-server/protocol.md](client-server/protocol.md)、生命周期见 [client-server/lifecycle.md](client-server/lifecycle.md)、施工清单见 [client-server/migration.md](client-server/migration.md)。
> **相关代码路径**：`src/server/`、`src/client/`、`src/ipc/`、`src/app/mirror.rs`。

> 历史调研（2026-11），写在改造之前。**方案已落地，但实现与本文有出入**：
> 传输层最终用**回环 TCP + 令牌**而不是本文的 UDS（Windows 上 `std` 无 UDS API）。
> 差异对照见 §9。数字（帧大小、耗时）来自 Linux 探针，是当时值。

## 1. 局限：为什么必须改

改造前是**谁启动，谁转发**：

```text
终端 A: sshpf ──► 进程 A ──► 持有 3 条隧道（本地监听 8080/9090/1080）
终端 B: sshpf ──► 进程 B ──► 完全不知道 A 的存在
```

后果不是"体验差"，而是**结构上不可能正确**：

| 现象 | 根因 |
|---|---|
| B 里看不到 A 启动的转发 | 状态在进程 A 的内存里 |
| B 修改配置后 A 不知道 | 两进程各自持有 `ConfigFile` 并各自 `persist`，**后写者覆盖前者** |
| A 退出 → 它的隧道全断，B 无能为力 | 隧道生命周期绑在"谁启动"上 |
| 同一端口两条转发 → 第二条 `bind` 失败 | 没有协调者，端口是唯一仲裁者 |

最后一行是硬伤：**端口就是唯一的冲突检测机制**，它只能告诉你"失败"，不能告诉你"谁占着"。

## 2. 目标形态

```text
终端 A ─┐
终端 B ─┼──► 本地 server（唯一）──► 全部隧道
终端 C ─┘        │
                 └── 一份 config.toml（唯一写者）
```

用户的三条要求逐条对应：

| 要求 | 实现 | 详见 |
|---|---|---|
| 不同 client 信息完全一致，一处修改各处相同 | 全量快照 + `revision` 单调递增 | [protocol.md](client-server/protocol.md) §3 |
| 所有 client 退出后 server 才退出 | `client_count` 归零 + 宽限窗口 | [lifecycle.md](client-server/lifecycle.md) §3 |
| 某个 client 可以停止所有转发 | `stop_all` 命令（server 侧校验） | [protocol.md](client-server/protocol.md) §4 |
| **最后一个 client 退出要给风险提示** | 确认闸文案升级 + server 复核计数 | §6 与 [migration.md](client-server/migration.md) §11 |

**"信息一致"的边界**：一致的是**转发状态**，不是"我打开的窗口"。两个 client 同时开不同表单、
各自过滤、各自滚动是**必须支持**的——`overlay`/`form`/`filter`/`select_mode` 都是本地态。
把 UI 态也同步会做出"别人动我光标"的体验，没人要求这个。

## 3. 关键设计选择（每条都写代价）

| 选择 | 好处 | 代价 |
|---|---|---|
| **全量快照，不做增量** | 乱序、丢帧、重连**结构上不可能** desync：丢一帧，下一帧就是完整真相 | 带宽比 delta 大（50 隧道 17 KB / 带日志尾巴 94 KB），所以**日志必须截断到 15 行** |
| **单例选举靠文件锁** | 不需要协调协议；进程死亡（含 `SIGKILL`）内核自动释放锁——**不需要心跳、不需要 pid 文件** | 残留 socket/端点需要"先抢锁、后 `unlink`"的识别流程，顺序不能反 |
| **传输层要一条两端都有的路** | 只有一套发现协议、一套失败模式 | 详见 §9：最终选了回环 TCP，代价是多一个令牌 |
| **wire 用 TOML，不用 JSON** | 仓库禁止新增依赖，依赖树里只有 `toml` | TOML 有自己的限制（无 `null`、嵌套 `Option` 有损、不能手拼），已逐条实测 |
| **`Instant` 不上线，只传整数毫秒** | client **完全不读自己的墙钟**——两台机器时钟差多少、NTP 跳变都不影响显示 | 传输延迟让 age 略滞后（毫秒级）；server 时钟回拨时 age 夹到 0 |
| **client 的意见只是提议，server 权威** | 过期视图不能决定现在的行为 | 错误要在 client 优雅呈现，不能指望"我早拦住了" |

## 4. 实测证据（52 条断言全 PASS）

来自 6 个可运行二进制（原 `client-server-spike/` 独立工程，**已删除**；重建方法见下）：

| 二进制 | 证明了什么 | 关键数字 |
|---|---|---|
| `election` | 8 进程抢锁**恰好 1 个**赢；`SIGKILL` 后锁自动释放、残留可恢复 | winners=1, losers=7，7/7 退出码 3 |
| `broadcast` | 3 client × 100 revision 严格递增、最终逐字段相同；慢 client 不阻塞别人 | 36 ms 广播 100 次；400 帧发布 1 ms |
| `lifecycle` | 最后 client 退出才关服；宽限内新 client 取消；**陈旧 `last_exit` 不关服** | 宽限 300 ms 内正确取消 |
| `codec` | 50 隧道 round-trip 无损；TOML 三项限制实测确认；1000 隧道仍装进一帧 | 负载 9 702 B / 整帧 9 722 B；1000 隧道 195 752 B |
| `clock` | client 时钟偏移 ±5 分钟**不影响** age | 三种偏移都算出 90 000 ms |
| `e2e` | **真实 server 进程 + 真实 client 进程**：3 client 一致、逐步退出、最后一个走掉才关服；**跨进程陈旧 `last_exit` 不关服** | 12 条断言，3 个独立进程构造竞态 |

**最关键的一条**（`e2e`，真实进程时序，不是模拟）：

```text
t0  server 进程 up
t1  client A 连上，观察到 client_count == 1  →  此刻它就该显示风险提示
t2  client B 连上（count 变 2）
t3  A 发 Bye{last_exit: true}（携带已过期的观察）
t4  server 重新计数 = 2-1 = 1 > 0  →  不退出  ✅
t5  B 发 Bye  →  count = 0  →  server 退出  ✅
```

**这就是"最后一个客户端"风险提示能安全存在的前提**：提示诚实，但过期视图在任何进程交错下都不能
停掉别人的转发——不是靠时序运气，而是**协议上没有让 client 拥有决定权**。

### 未证实（当时的诚实清单）

Windows 全项（无环境，且方案本身需要另设计）、真实 SSH 隧道介入的多 client 行为（spike 用模型状态
替代）、≥3 client 的慢消费者跨进程表现、NFS 上的 `flock` 语义、长时间运行的内存/句柄增长
（未做 soak）。

### 复跑方法（原 spike 已删除）

一个独立 Cargo 工程（依赖只有 `serde` + `toml`，**与主仓库既有依赖一致**，这本身是设计约束的验证），
六个 binary 加一个 `#[cfg(test)]` 单元测试集；产物写 `std::env::temp_dir()` 下独立目录。
慢消费者要用**有界 `sync_channel(8)` + `try_send`** 建模，不能依赖内核 socket 缓冲区大小
（各 OS 不同，结论不可复现）。

## 5. 代价盘点

| 代价 | 严重度 | 说明 |
|---|---|---|
| **复杂度上升一档** | 高 | 多一个进程、一层 IPC、一个状态机 |
| **排障变难** | 中 | "为什么没连上"从"看一个进程"变成"看两个进程 + 端点文件" |
| **调试路径变长** | 中 | 单步跟踪跨进程不可能；`--server` 前台模式保留就是为了这个 |
| **用户认知负担** | **高** | 关掉最后一个终端 = 全停。最需要文案和文档的地方 |
| **`App` 拆分的回归风险** | 高 | 大状态机要拆两半，`ui`/`keys` 必须几乎不动才安全 |
| **Windows 曾是缺口** | 中 | 后来的实现用回环 TCP 解决了（§9） |

**值得吗？** 值得。替代方案不是"保持简单"，而是"继续在两个进程间手动同步两套配置"——那会长期产生
"后写者覆盖前者"这类静默数据丢失。

## 6. 最后一个 client 退出（用户明确要求）

> 「最后一个 client 退出时，这个时候要给出风险提示，所有端口转发都要停止啦！」

这是本方案**最容易被忽略的破坏性后果**：用户以为只是关掉一个终端窗口，实际停掉的是全部转发。

### 提示条件（只在真的会全停时打扰）

| 情况 | 提示？ |
|---|---|
| 它是最后一个 client **且**有运行中转发 | **是**，文案说出「最后一个客户端 / 全部停止 / N 条」 |
| 还有别的 client | 否，普通退出 |
| 最后一个但没有运行中的转发 | 否，没有可停的东西 |

第三条是刻意设计：给高频操作加确认会让用户习惯无脑按 `y`，真正危险时的确认随之失效
（[../design.md](../design.md) D11 的原话）。

### 文案（落在既有确认闸，不新增交互）

```text
这是最后一个客户端：退出后所有端口转发都会停止（{running} 条运行中）。确定退出？
Last client: quitting stops every port forward ({running} running). Quit anyway?
```

`q` 与 `ctrl+c` 的既有键位与"总是先确认"的行为不变。

### 无法提示的不对称（必须承认）

client 崩溃、`SIGKILL`、终端被杀、SSH 断开——**没有机会弹确认框**。结果是：
**最后一个终端崩溃 = 全部转发静默停止**。这不是文案能解决的，只能靠策略：

| 措施 | 说明 |
|---|---|
| 长宽限窗口 | 崩溃路径用完整宽限（默认 5s，可配），覆盖"终端误关/重开" |
| `--keep-server` 逃生阀 | 最后 client 退出后 server 常驻。**明确文档化为与需求相反的可选项** |
| 意图保留 | `enabled` 已持久化，server 下次启动按 `restore_on_start` 恢复——隧道会回来，但中间断过 |

三条都做。反方向的代价（静默杀掉所有转发）更贵。

## 7. 不做什么

| 不做 | 原因 |
|---|---|
| 常驻 daemon / systemd 服务 | 与"最后 client 退出即停"的需求直接冲突，是另一个需求 |
| 跨主机 / 监听非回环地址 | 引入认证与加密需求，安全模型完全不同 |
| 传输层加密 | 见 §9：回环 + 令牌已是本机同 uid 边界；加密收益低 |
| 防同 uid 进程 | 需要 `unsafe`（`getsockopt(SO_PEERCRED)` 或等价），违反「`unsafe` 只在 crash 层」的硬约束 |
| 多 server 同步 | 身份按 `config_path` 隔离，不同配置本就是不同 server |
| 动 SSH 层 | 与本改动正交，零收益 |

## 8. 开放问题（当时的倾向）

| 问题 | 倾向 |
|---|---|
| 宽限窗口默认值 | 先 5s 可配，收集真实反馈再定 |
| 是否默认提供 `--keep-server` | 提供，但**默认关**（默认行为必须符合用户需求） |
| 端点命名用可读路径还是 hash | 最终用 hash（FNV-1a），见 §9 |
| 是否接受 Unix-only 一段时间 | 最终没有接受——回环 TCP 让两端同码 |

## 9. 本文预测 vs 实际落地（差异清单）

| 项 | 本文（2026-11 调研时） | 实际实现 | 原因 |
|---|---|---|---|
| 传输层 | **Unix domain socket**，0700 目录 + 0600 socket，靠**文件系统权限**认证 | **回环 TCP + 128-bit 令牌**，端点 = `port` / `token` / `lock` 三个文件 | `std` 在 Windows 上没有 UDS API；要一条两端都同码的路 |
| Windows | 明确列为不做 | 与 Linux 同码 | 同上 |
| 同 uid 防护 | 承认做不到（`peer_cred()` 不可用） | 令牌把"能连上端口"与"能被服务"分开 | 回环端口没有权限边界 |
| 端点边界 | `socket_path` / `lock_path` | 同名概念，多了 `port` / `token` | 需要发现临时端口 |
| 协议版本 | `proto_version: 1` | 已到 2 | 后续变更按 [protocol.md](client-server/protocol.md) §8.2 的规则 bump |

**结论**：本文的**判断**（全量快照、锁即真相、server 权威、最后 client 提示）全部成立并落地；
**传输机制**被换掉了。当前事实见 [../client-server.md](../client-server.md)。

## 10. 文档地图

| 想知道 | 读 |
|---|---|
| 帧格式、消息字段、命令、体积、时钟、慢消费者 | [protocol.md](client-server/protocol.md) |
| 进程怎么起、怎么选主、何时关服、退出提示的完整时序 | [lifecycle.md](client-server/lifecycle.md) |
| 改哪些文件、分几阶段、风险与回滚 | [migration.md](client-server/migration.md) |
