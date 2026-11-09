# client-server 架构调研 / Client-server architecture study

> **这份文档回答**：为什么要把"每个 CLI 自己转发"改成 client-server、方案是什么、有哪些代价、
> 落地分几步、哪些事**不做**。这是本目录的入口，其余四篇是展开。
> **不负责**：协议字段细节（[protocol.md](client-server/protocol.md)）、选举与关服时序
> （[lifecycle.md](client-server/lifecycle.md)）、施工清单（[migration.md](client-server/migration.md)）、
> 实测数据（[client-server-spike/](client-server-spike/README.md)）。
> **相关代码路径**：现状 `src/main.rs`、`src/app.rs`、`src/tunnel.rs`；将来 `src/ipc/`、`src/server/`、`src/client/`。

## 结论先行 / Bottom line

| 问题 | 结论 |
|---|---|
| 值得做吗 | **值得**。局限是结构性的（两个进程各自绑端口），不是缺功能 |
| 技术上可行吗 | **可行**，5 条最危险的假设已实测成立（40 条断言全 PASS） |
| 要新增依赖吗 | **不需要**。`std` 的 `File::lock` + `UnixStream` + 已有 `serde`/`toml` 足够 |
| 最大风险 | **最后一个 client 退出 = 全部转发停止**（用户已要求加风险提示，见 §4） |
| 本次 PR 做什么 | **只做调研 + 原型 + 计划**，不动 `src/`；PR 提交后**不合并** |

## 1. 现状与局限 / The limitation

现在的 `sshpf`：**谁启动，谁转发**。

```text
终端 A: sshpf ──► 进程 A ──► 持有 3 条隧道（本地监听 8080/9090/1080）
终端 B: sshpf ──► 进程 B ──► 完全不知道 A 的存在
```

后果不是"体验差"，而是**结构上不可能正确**：

| 现象 | 根因 |
|---|---|
| B 里看不到 A 启动的转发 | 状态在进程 A 的内存里 |
| B 修改配置后 A 不知道 | 两进程各自持有 `ConfigFile`，各自 `persist`，**后写者覆盖前者** |
| A 退出 → 它的隧道全断，B 无能为力 | 隧道的生命周期绑在"谁启动"上 |
| 同一端口两条转发 → 第二条 `bind` 失败 | 没有协调者，端口是唯一仲裁者 |

最后一行是硬伤：**端口就是唯一的冲突检测机制**，而它只能告诉你"失败"，不能告诉你"谁占着"。

## 2. 目标形态 / Target shape

```text
终端 A ─┐
终端 B ─┼──► 本地 server（唯一）──► 全部隧道
终端 C ─┘        │
                 └── 一份 config.toml（唯一写者）
```

用户提出的三条要求，逐条对应到设计：

| 要求 | 实现 | 详见 |
|---|---|---|
| 不同 client 信息完全一致，一处修改各处相同 | 全量快照 + `revision` 单调递增广播 | [protocol.md](client-server/protocol.md#3-消息全集--messages) |
| 所有 client 退出后 server 才退出 | `client_count` 归零 + 宽限窗口 | [lifecycle.md](client-server/lifecycle.md#客户端计数与宽限窗口--client-count--grace-window) |
| 某个 client 可以停止所有转发 | `stop_all` 命令（server 侧校验，任何 client 都能发） | [protocol.md](client-server/protocol.md#33-命令与应答) |
| **最后一个 client 退出要给风险提示** | 确认闸文案升级 + server 复核计数 | §4 与 [lifecycle.md](client-server/lifecycle.md#最后一个客户端退出--last-client-exit) |

**"信息一致"的边界要说清**：一致的是**转发状态**，不是"我打开的窗口"。两个 client 同时开不同表单、
各自过滤、各自滚动是**必须支持**的——它们的 `overlay`/`form`/`filter`/`select_mode` 都是本地态。
把 UI 态也同步会做出一个"别人动我光标"的恐怖体验，没人要求这个。

## 3. 关键设计选择 / Key decisions

每条都写清**代价**，不只写好处。

### 3.1 全量快照，不做增量

每次状态变更广播**完整**快照，客户端按 `revision` 丢弃旧的。

- **好处**：乱序、丢帧、重连**结构上不可能** desync。丢一帧，下一帧就是完整真相。
- **代价**：带宽比 delta 大。实测 50 条隧道 ≈ **9.7 KB**（精简 schema）/ **19.4 KB**（完整 schema），
  1000 条约 **196 KB**。在本机 UDS 上是噪声级。
- **什么时候该改**：隧道数上千、或每秒上百次状态变更时才需要 delta。现在不需要。

### 3.2 单例选举靠文件锁，不靠 pid 文件 + 心跳

client 先 `connect`；失败就 `spawn` server；server 启动第一件事抢 `File::lock`，抢不到立刻静默退出。

- **好处**：不需要协调协议。**实测 8 进程并发抢占，恰好 1 个赢，7 个干净退出**。
  进程死亡（含 `SIGKILL`）内核自动释放锁——**不需要心跳、不需要超时探测、不需要 pid 文件**。
- **代价**：残留 socket 文件需要识别流程（`ECONNREFUSED` + 拿到锁 → 判定残留 → `unlink`）。
  这个顺序不能反：先 `unlink` 会删掉正在启动的 server 即将用的路径。

### 3.3 Unix domain socket，不用 TCP

- **好处**：**文件系统权限即认证**（0700 目录 + 0600 socket）。任何其它 uid 的进程连不上。
- **代价**：Windows 不支持（`std` 无 named pipe API）→ 明确列为不做。
  同 uid 的其它进程**仍能连**（`peer_cred()` 在 std 仍是 unstable，无法做内核级 uid 校验）——
  但同 uid 本来就能改你的配置、`kill` 你的进程，这不是新增攻击面。

### 3.4 wire 用 TOML，不用 JSON

仓库**禁止新增依赖**，而依赖树里没有 `serde_json`，只有 `toml`。

- **好处**：零新增依赖，`serde` 直接复用。
- **代价**：TOML 有自己的限制，**已逐条实测**：无 `null`（`None` = 字段缺失）、
  嵌套 `Option` **有损**（协议禁止）、不能手拼（两端只走序列化器）。见
  [protocol.md](client-server/protocol.md#6-toml-编码限制与规避--toml-limits)。

### 3.5 `Instant` 不上线，只传整数毫秒

`TunnelStatus` 含 `Instant { since }`，**不能跨进程**（单调钟基准点不同）。

- **做法**：wire 传 `since_ms` + `server_now_ms`（server 墙钟），client 算 age 后本地用 `Instant` 推进。
- **好处**：**client 完全不读自己的墙钟**——两台机器时钟差多少、client 被 `ntpdate` 跳变，都不影响显示。
  **实测：偏移 ±5 分钟算出的 age 完全相同**。
- **代价**：传输延迟会让 age 略滞后（毫秒级）；server 时钟回拨时 age 夹到 0（记 WARN，不试图修正）。

### 3.6 client 的意见只是提议，server 才是权威

协议里没有"client 让 server 做什么就做什么"这条路径。具体两处：

| client 说 | server 怎么做 |
|---|---|
| `expect_rev = 42` | 不符就回 `Stale`，让用户看新状态再决定，**不自动重放** |
| `Bye{last_exit: true}` | **重新计数**；还有别的 client 就不许关服 |

第二条是 §4 那个风险提示能安全存在的前提。

## 4. 最后一个 client 退出 / Last client exit（用户明确要求）

> 「最后一个 client 退出时，这个时候要给出风险提示，所有端口转发都要停止啦！」

这是本方案**最容易被忽略的破坏性后果**：用户以为只是关掉一个终端窗口，实际停掉的是全部转发。

### 提示条件（只在真的会全停时打扰）

| 情况 | 提示？ |
|---|---|
| 它是最后一个 client **且**有运行中转发 | **是**，文案说出「最后一个客户端 / 全部停止 / N 条」 |
| 还有别的 client | 否，普通退出 |
| 最后一个但没有运行中的转发 | 否，没有可停的东西 |

第三条与第二条是刻意设计：给高频操作加确认会让用户习惯无脑按 `y`，真正危险时的确认随之失效
（`docs/design.md` D11 的原话）。

### 文案（落在既有确认闸，不新增交互）

```text
这是最后一个客户端：退出后所有端口转发都会停止（{running} 条运行中）。确定退出？
Last client: quitting stops every port forward ({running} running). Quit anyway?
```

现有实现在 `src/app/confirm.rs` 的 `count_prompt` → `ConfirmAction::Quit`；
`q` 与 `ctrl+c` 的既有键位与"总是先确认"的行为不变。

### 竞态：client 的判断可能过期

client 依据快照里的 `client_count`，而快照会过期：它以为自己是最后一个，期间另一个终端刚连上。
**实测构造了这个交错并验证**：

```text
A 认为是最后 → 用户按 y → B 在此之前连上 → A 的 Bye 到达 → server 计数 2-1=1 > 0 → 不退出 ✅
```

**协议上根本没有让 client 拥有决定权**，所以这不是靠时序运气。

### 无法提示的不对称（必须承认）

client 崩溃、`SIGKILL`、终端被杀、SSH 断开——**没有机会弹确认框**。
结果是：**最后一个终端崩溃 = 全部转发静默停止**。

| 措施 | 说明 |
|---|---|
| 长宽限窗口 | 崩溃路径用完整宽限（默认 5s，可配），覆盖"终端误关/重开" |
| `--keep-server` 逃生阀 | 最后 client 退出后 server 常驻。**明确文档化为与需求相反的可选项** |
| 意图保留 | `enabled` 已持久化，server 下次启动按 `restore_on_start` 恢复——隧道会回来，但中间断过 |

三条都做。反方向的代价（静默杀掉所有转发）更贵。

## 5. 实测证据 / Evidence

40 条断言全 PASS，来自 5 个可运行二进制（[client-server-spike/](client-server-spike/README.md)）：

| 二进制 | 证明了什么 | 关键数字 |
|---|---|---|
| `election` | 8 进程抢锁**恰好 1 个**赢；`SIGKILL` 后锁自动释放、残留 socket 可恢复 | winners=1, losers=7 |
| `broadcast` | 3 client × 100 revision 严格递增、最终逐字段相同；慢 client 不阻塞别人 | 36 ms 广播 100 次；400 帧发布 1 ms |
| `lifecycle` | 最后 client 退出才关服；宽限内新 client 取消；**陈旧 `last_exit` 不关服** | 宽限 300 ms 内正确取消 |
| `codec` | 50 隧道 round-trip 无损；TOML 三项限制实测确认 | 帧 9 722 B / 1 MiB 上限 |
| `clock` | client 时钟偏移 ±5 分钟**不影响** age | 三种偏移同结果 |

**未证实**（诚实清单在 spike README）：Windows 全项、真实 SSH 隧道多 client 行为、
进程级端到端（`broadcast`/`lifecycle` 用线程与模型，非真 server 进程）、NFS 上的 `flock` 语义。

## 6. 代价盘点 / Costs

不粉饰。这次改动买来的东西不便宜：

| 代价 | 严重度 | 说明 |
|---|---|---|
| **复杂度上升一档** | 高 | 多一个进程、一层 IPC、一个状态机。现在 `main.rs` 是一个同步循环就能读完的 |
| **排障变难** | 中 | "为什么没连上"从"看一个进程"变成"看两个进程 + 一个 socket"。靠 `server.*`/`client.*` 事件补 |
| **调试路径变长** | 中 | 单步跟踪跨进程不可能。`--server` 前台模式保留就是为了这个 |
| **用户认知负担** | **高** | 关掉最后一个终端 = 全停。这是最需要文案和文档的地方 |
| **`App` 拆分的回归风险** | 高 | 1394 行的状态机要拆两半，`ui`/`keys` 必须几乎不动才安全 |
| **Windows 暂不支持** | 中 | 当前 Windows CI 仍绿（因为不实现），但功能是 Unix-only |

**值得吗？** 值得，因为替代方案不是"保持简单"，而是"继续在两个进程间手动同步两套配置"——
那会长期产生"后写者覆盖前者"这类静默数据丢失。

## 7. 落地路径 / Roadmap

**5 个阶段，5 个 PR，每个独立可合并、CI 绿**。细节见 [migration.md](client-server/migration.md#5-分阶段落地--phased-rollout)。

| 阶段 | 内容 | 规模 | 行为变化 |
|---|---|---|---|
| 1 | 抽出 `src/ipc/` 与协议 codec（无人使用） | M | **零** |
| 2 | `App` 拆分（仍是单进程，只搬字段） | L | 零 |
| 3 | server 进程 + 单 client（真 IPC） | L | 出现两个进程 |
| 4 | 多 client + 选举 + 生命周期 + 退出提示 | L | **用户可见** |
| 5 | 清理旧路径 + 文档 | S | 零 |

**本 PR 只做调研 + spike + 计划**，不含任何 `src/` 改动。按用户要求，**PR 不合并**，
留给评审后再决定是否进入阶段 1。

## 8. 不做什么 / Out of scope

| 不做 | 原因 |
|---|---|
| Windows | `std` 无 named pipe API；需单独设计发现与认证 |
| 常驻 daemon / systemd 服务 | 与"最后 client 退出即停"的需求直接冲突，是另一个需求 |
| 跨主机 / 监听 TCP | 引入认证与加密需求，安全模型完全不同 |
| UDS 加密 | 0700/0600 已是同 uid 边界；加密收益低 |
| 防同 uid 进程 | 需 `unsafe` 调 `getsockopt(SO_PEERCRED)`，违反"unsafe 只在 crash.rs"的硬约束 |
| 多 server 同步 | 身份按 `config_path` 隔离，不同配置本就是不同 server |
| 动 SSH 层 | 与本改动正交，零收益 |

## 9. 开放问题 / Open questions

需要评审者拍板的，不是我能单方面定的：

| # | 问题 | 我的倾向 |
|---|---|---|
| 1 | 宽限窗口默认值（5s？） | 先 5s 可配，收集真实反馈再定 |
| 2 | 是否默认提供 `--keep-server` | 提供，但**默认关**（默认行为必须符合用户需求） |
| 3 | socket 路径用转义路径还是 hash | 转义路径（可读、无依赖）；长路径用"末段 + 摘要"兜底 |
| 4 | 阶段 2（`App` 拆分）能否更小步 | 可以按"字段组"再拆，但每步都要过全量测试 |
| 5 | 是否接受 Unix-only 一段时间 | 倾向接受，Windows 单独立项 |

## 10. 文档地图 / Where to read next

| 想知道 | 读 |
|---|---|
| 帧格式、消息字段、命令、体积、时钟、慢消费者 | [protocol.md](client-server/protocol.md) |
| 进程怎么起、怎么选主、什么时候关服、退出提示的完整时序 | [lifecycle.md](client-server/lifecycle.md) |
| 改哪些文件、`App` 怎么拆、分几阶段、风险与回滚 | [migration.md](client-server/migration.md) |
| 哪些假设被实测证实、数字是多少、什么没证实 | [client-server-spike/README.md](client-server-spike/README.md) |
