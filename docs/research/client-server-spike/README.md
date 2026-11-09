# client-server spike — 可运行原型与实测结论

> **这份文档回答**：client-server 方案里那些"听起来对"的假设，**实测是否成立**；关键数字是多少。
> **不负责**：设计本身（见 [../client-server.md](../client-server.md) 与 `client-server/`）、
> 可维护代码（这里全部是可丢弃的探针）。
> **相关代码路径**：本目录（独立 Cargo 工程，被根 `Cargo.toml` 的 `exclude` 覆盖，不参与主构建）。

## 这是什么 / What this is

五个二进制，每个证明一组设计断言。**每条 `PASS` 都是真跑出来的**，不是文档里的断言。
全部只在 Linux 实测；Windows 未做（`std` 无 named pipe API，本次范围外）。

```bash
cd docs/research/client-server-spike
cargo run --bin election     # 单例选举 + SIGKILL 残留恢复
cargo run --bin broadcast    # 多 client 一致性 + 慢消费者隔离
cargo run --bin lifecycle    # 最后 client 退出 + 陈旧 last_exit 竞态
cargo run --bin codec        # 帧 codec round-trip + 体积 + TOML 限制
cargo run --bin clock        # 时钟纪律（client 时钟偏移不影响 age）
cargo run --bin e2e          # 真实 server 进程 + 真实 client 进程端到端（含跨进程竞态）
cargo test                   # 9 个单元测试（codec / wire / frame）
```

所有产物写 `std::env::temp_dir()` 下的独立目录，可重复运行。

## 断言与结论 / Assertions and results

### `election` — 单例选举（6 PASS）

| 断言 | 结论 | 关键数字 |
|---|---|---|
| N 个进程抢同一把锁，**恰好一个**成功 | **成立** | 8 进程并发：`winners=1 clean_losers=7` |
| 输掉的进程干净退出（约定退出码） | **成立** | 7/7 退出码 3 |
| `SIGKILL` 后 socket 文件残留 | **成立** | 文件存在、`connect` 被 `ECONNREFUSED` |
| `SIGKILL` 后锁被内核自动释放 | **成立**（关键） | 无需 pid 文件即可判定持有者已死 |
| 拿到锁 → `unlink` → 重新 `bind` 成功 | **成立** | `removed=true rebind=true` |

**为什么这条最重要**：整个"只有一个 server"的设计不靠协调协议、不靠心跳、不靠 pid 文件，
只靠"活着的 server 一直持锁 + 进程死亡内核释放锁"。这条成立，选举就能成立。

### `broadcast` — 共享状态（8 PASS）

| 断言 | 结论 | 关键数字 |
|---|---|---|
| 每个 client 收到的 revision **严格递增** | **成立** | 3 client × 100 revision |
| 每个 client 都收到最终 revision | **成立** | `last_revs=[100,100,100]` |
| 每个 client 最终快照**逐字段相同** | **成立** | 摘要一致 `rev=100 count=3 tunnels=10` |
| 100 次广播在一帧预算内 | **成立** | **36 ms**（预算 200 ms） |
| 一个 client 卡住不影响其它 client 收数据 | **成立** | 好 client 收到 400/400 |
| 广播**绝不**因慢 client 阻塞 | **成立** | 400 帧共 **1 ms** 发布耗时 |
| 慢 client 到达有界队列上限后被断开 | **成立** | 队列容量 8，`try_send` 立即判定 |
| 真实 UDS 上"好 client 持续可读" | **成立** | 200 帧写入期间持续读取成功 |

**建模说明**：慢消费者用**有界 `sync_channel(8)` + `try_send`** 直接建模协议里的"每会话有界队列"，
而不是依赖内核 socket 缓冲区大小（各 OS 不同、会让结论不可复现）。另外用真实 UDS 单独验证了
"一个 peer 不读，另一个 peer 仍能持续收帧"。

### `lifecycle` — 最后 client 退出（8 PASS）

| 断言 | 结论 | 关键数字 |
|---|---|---|
| 还有别的 client 时退出 → server **继续运行** | **成立** | `KeepRunning` |
| 最后一个 client 退出 → 启动宽限计时 | **成立** | — |
| 宽限窗口结束后 server 退出 | **成立** | 300 ms 窗口内退出 |
| 宽限窗口**内**新 client 连入 → **取消**退出 | **成立** | — |
| **陈旧 `last_exit` 竞态**：A 认为是最后，但 B 已连上 → server **不退出** | **成立**（关键） | `KeepRunning`，且不启动计时 |
| 真正是最后一个时的 `last_exit` → 确实启动关服 | **成立** | `GraceStarted` |

竞态时序（实测构造）：

```text
t0  server 1 个 client（A）
t1  A 渲染退出确认框：快照 client_count==1 → 显示"最后一个客户端，全部转发将停止"
t2  用户按 y
t3  B 连上（另一个终端，在 A 的 Bye 被处理之前）   ← 关键
t4  A 的 Bye{last_exit:true} 到达 server
t5  server 重新计数 = 2-1 = 1 > 0  →  不退出
```

**这就是用户要求的那条风险提示能安全存在的前提**：提示是诚实的（"你是最后一个，退出会全停"），
但**陈旧视图永远不能拥有停掉别人转发的权力**。

### `codec` — 协议编码（11 PASS）

| 断言 | 结论 | 关键数字 |
|---|---|---|
| 50 条隧道快照 round-trip 逐字段相等 | **成立** | 负载 **9 702 B**，整帧 **9 722 B** |
| 帧远低于 1 MiB 上限 | **成立** | 9.7 KB |
| 1000 条隧道仍装得进一帧 | **成立** | **195 752 B** |
| TOML 无 `null`：`None` = 字段缺失 | **成立** | `a = 1`（b 整行消失） |
| 一层 `Option` round-trip | **成立** | — |
| **嵌套 `Option` 有损** | **成立** | `Some(None)` 与 `None` 都编码成空串 ← 所以协议禁止嵌套 Option |
| 手写"标量在表之后"解析失败 | **成立** | 所以两端都只走序列化器，不手拼 TOML |
| 未知字段被忽略（前向兼容） | **成立** | 新 server 加字段不炸旧 client |

**体积口径**：本 spike 的 `WireTunnelView` 是精简镜像（9.7 KB / 50 条）。
真实实现若每行都带 `reason`/时间戳等完整字段，同样是 50 条约 **19.4 KB**——
两者都在 1 MiB 上限的 2% 以内，**"全量快照而非增量"的判断成立**。

### `clock` — 时钟纪律（7 PASS）

| 断言 | 结论 | 关键数字 |
|---|---|---|
| age = 两个 server 时间戳之差 | **成立** | 300 000 ms |
| client 本机时钟偏移 ±5 分钟不影响 age | **成立** | 三种偏移都算出 90 000 ms |
| 会话内用单调钟推进 | **成立** | — |
| server 时钟回拨 → age 夹到 0，不显示负数 | **成立** | `saturating_sub` |

**为什么重要**：`TunnelStatus` 里的 `Instant` 不能跨进程。协议只传 `since_ms` + `server_now_ms`，
client **完全不读自己的墙钟**——这是"两台机器时钟差多少都不影响显示"的根本原因。

### `e2e` — 真实多进程端到端（12 PASS）

前面几个二进制用线程与内存模型证明**逻辑**；这个用**真实 OS 进程**证明规则能穿过进程边界
（独立地址空间、真正的 `flock`、真正的 socket EOF）。

| 断言 | 结论 | 关键数字 |
|---|---|---|
| 真 server 进程绑定 socket 并持有锁 | **成立** | 独立进程，`try_lock` 成功 |
| 3 个真 client 进程都观察到 3 个 client 已连接 | **成立** | 各自写 marker 文件 |
| 每个 client 的快照**完全相同** | **成立** | `rev=3 count=3 tunnels=3` |
| 每个 client 的 revision **严格递增** | **成立** | 3/3 |
| 走掉 2 个 client 后 server **继续运行** | **成立** | 剩 1 个时仍存活 |
| 最后一个 client 走掉 → server **自行退出** | **成立** | 退出原因记录为 `last_client` |
| **跨进程陈旧 `last_exit` 竞态**：A 观察到自己是唯一 client，B 在 A 告别前连上，A 仍发 `last_exit=true` | **成立**：server **不退出** | 3 个独立进程构造该交错 |
| 之后 B 真正离开 → server 才退出 | **成立** | 原因仍为 `last_client` |

这个交错是手工构造的**真实进程时序**，不是模拟：

```text
t0  server 进程 up
t1  client A 进程连上，观察到 client_count == 1  →  写 a.solo（此刻它就该显示风险提示）
t2  client B 进程连上（count 变 2）
t3  A 发 Bye{last_exit: true}（携带已过期的观察）
t4  server 重新计数 = 2-1 = 1 > 0  →  不退出  ✅
t5  B 发 Bye  →  count = 0  →  server 退出  ✅
```

**这是用户那条风险提示能安全存在的最终证据**：提示诚实，但过期视图在任何进程交错下都不能
停掉别人的转发。

## 未证实 / Not proven

诚实清单。以下**没有**实测证据，不要当成已验证：

| 项 | 原因 |
|---|---|
| Windows 上任何一条 | 无 Windows 环境；`std` 无 named pipe API，方案本身需要另设计 |
| 真实 SSH 隧道在多 client 下的行为 | spike 用模型状态替代，未接 `russh`；隧道层不受本改动影响（`Supervisor` 原样复用） |
| ~~真正的 server 进程 + 真 client 进程端到端~~ | **已证实**（`e2e`，12 条断言）。仍留白：真实 SSH 隧道介入、≥3 client 的慢消费者跨进程表现 |
| 网络文件系统上的 `flock` 语义 | 未测。若 `config_path` 在 NFS 上，`flock` 行为可能不同；缓存目录应在本地盘 |
| 长时间运行的内存/句柄增长 | 未做 soak；`Supervisor` 既有 30 次启停无泄漏的结论仍适用，但 IPC 层未测 |
| TOML 编码吞吐在极端规模下 | 1000 条隧道编码未计时；当前规模（几十条）远不是瓶颈 |
| 路径 hash 的具体算法 | 文档建议"转义路径"而非 hash；本 spike 未实现 `endpoint` 派生 |

## 与主仓库的关系 / Relation to the crate

- 本工程**不参与** `cargo build` / `cargo test`（根 `Cargo.toml` 的 `exclude` + 不在 workspace）。
- 依赖只有 `serde` + `toml`，**与主仓库既有依赖一致，没有新增依赖**——这本身是设计约束的验证。
- 代码遵守主仓库风格：文件头 `//!` 说明、单文件 ≤400 行、单函数 ≤60 行。
  目的就是证明这套结构**能迁进主仓库**而不违反 `tests/safety_guards.rs` 的断言。
- 被测量的类型是主仓库类型的**镜像**（`WireTunnelView` ← `TunnelView`），
  因为 `TunnelStatus` 含 `Instant`，不能直接序列化——这正是协议要独立 wire 类型的理由。
