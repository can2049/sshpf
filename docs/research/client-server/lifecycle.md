# 生命周期与单例选举 / Process lifecycle & singleton election（调研稿）

> **这份文档回答**：client 与 server 怎么起、怎么保证全局只有一个 server、最后一个 client 退出时发生什么、宽限窗口为什么分两种、信号怎么处理、崩溃后怎么办。
> **不负责**：协议消息字段（见 [protocol.md](protocol.md)）、代码怎么改（见 [migration.md](migration.md)）、当前行为（该方案已回退，见 [../../architecture.md](../../architecture.md#进程模型--process-model)）。
> **相关代码路径**：`src/server/election.rs`、`src/server/start.rs`、`src/server/session.rs`、`src/client/connect.rs`、`src/sys/signals.rs`。

> 历史设计稿（2026-11），写于实现之前。**判断已落地**；细节以 `src/server/`、`src/client/` 为准。
> 已知偏差：端点不叫 `socket_path`，而是 `port`/`token`/`lock` 三个文件（回环 TCP 方案）；
> client 崩断的宽限实际为 5s、主动退出为 0.5s。

## 术语

| 词 | 含义 |
|---|---|
| `config_path` | 本次运行的配置文件路径（`-c` 或默认 XDG 路径），**server 的身份** |
| 端点 | 由 `config_path` 派生的 `port` / `token` / `lock` 文件 |
| `client_count` | 当前连在 server 上的 client 数（server 自己数的，权威） |
| `grace window` | `client_count` 归零后等待多久才关服的窗口 |
| `last_exit` | client 在 `Bye` 里声明"用户已确认后果"，**只是提议** |

## 1. 进程模式

| 调用 | 角色 | 连 server？ | 恢复 `enabled` 隧道？ |
|---|---|---|---|
| `sshpf` | client（无 server 则乐观拉起） | 是 | **否** |
| `sshpf --server` | 前台 server（调试） | 它是 server | **是** |
| `sshpf --keep-server` | client，退出后 server 常驻 | 是 | 否 |
| `sshpf --check` | 一次性校验，打印后退出 | **否** | 否 |
| `sshpf --list-hosts` | 一次性列主机，打印后退出 | **否** | 否 |

**两条最容易写错的区分**：

1. **只有 server 恢复 `enabled` 隧道。** 若 client 也调用恢复逻辑，第 2 个 client 会把已有隧道
   **再启动一遍**（对同一 id 会先 stop 再 start，表现为"连上就掉线再重连"）。所以恢复职责**整体搬进
   server 启动路径**。
2. **`--check` / `--list-hosts` 根本不连 server。** 它们是"读配置 + 打印"，引入 server 依赖会让
   `sshpf --check` 在无 server 时先拉起一个 server 再退出——这是可观察的行为变化，必须避免。

## 2. 单例选举

### 为什么需要

多个 client 并发启动时，每个都可能判定"没有 server"并各拉一个。若两个 server 同时活着并各自绑定
隧道端口，第二个的 `bind` 会失败，用户看到随机一半端口起不来。

### 判定顺序（顺序本身是安全性的一部分）

```text
client 启动
  │
  ① connect(端点)
  │    成功 ──────────────────────────────► 我是 client，正常会话
  │    失败(err)
  ▼
  ② 取 election lock（File::try_lock 语义）
  │    拿到锁 ──► 说明"当前没有活着的 server 在服务这个 config_path"
  │                ├─ ③ 若端点文件存在：它只可能是残留（上次被 SIGKILL）
  │                │     → 清掉端点，记 server.stale_endpoint_removed
  │                ├─ ④ spawn detached server 子进程
  │                └─ ⑤ 释放锁，退避重试 connect（总预算 5s）
  │   拿不到锁 ──► 说明"另一个进程正在选举/正在服务"
  │                └─ ⑤ 退避重试 connect，不 spawn（总预算 5s）
  ▼
  ⑥ 预算内连上 → 我是 client
     预算耗尽   → 报可操作错误，退出码非 0（不静默降级成"自己转发"）
```

**为什么"拿到锁"就能认定没有活 server？** 因为活的 server **在整个生命周期里一直持锁**。
锁的持有者是 server 进程本身，进程死亡（含 `SIGKILL` / `TerminateProcess`）时内核自动释放——
这是本方案不需要 pid 文件、不需要心跳、不需要超时探测的根本原因。
实测：两进程抢同一路径，先到者 `GOT LOCK`，后到者立刻 `BUSY: operation would block`。

**为什么"拿不到锁"不能直接放弃？** 两种真实情况必须区分：(a) 已有 server 在服务 → 应该当 client；
(b) 另一个进程刚好在 spawn server 途中（锁已被它拿到、端点还没 bind）→ 稍等即可。
两种情况的最优动作都是"退避重试 connect"，所以这里**不需要**区分它们。

### 并发启动两个 client 的时序

```text
时刻   client A                         client B                         server 进程
 t0    connect → 失败                    connect → 失败                    （无）
 t1    try_lock → 成功                    try_lock → 失败(BUSY)
 t2    spawn server ────────────────────►                                 fork
 t3    释放锁                                                              try_lock → 成功，持锁
 t4    connect(重试) → 失败               connect(重试) → 失败              bind 成功
 t5    connect → 成功 ──────────────────────────────────────────────────►  接受 A，count=1
 t6                                       connect → 成功 ─────────────────►  接受 B，count=2
```

A 与 B 最终都是 client，只有一个 server。**败者不需要任何协调协议**，也不知道自己输给了谁——
它只是重连成功而已。这是"乐观拉起 + 锁即真相"最值钱的地方。

### 残留资源

| 资源 | 谁创建 | 谁清理 | `SIGKILL` 后 |
|---|---|---|---|
| 端点文件（port/token） | server 启动时写 | server 正常退出时删 | **残留**（文件在，但没人 listen）→ 由 ①②③ 识别并清理 |
| `lock` | server 首次 `open` | 不需清理（文件可长期存在） | **自动释放**（内核行为），文件留着无害 |
| pid 文件 | **不需要** | — | — |

**不要 pid 文件**：它的唯一用途是"判断持有者是否还活着"，而这正是 `flock` 免费提供的。
多一个 pid 文件就多一份"文件说活着但进程已死"的失效模式。

**残留端点的识别为什么安全？** 判据是"`connect` 被拒 **且** 能拿到 election lock"。
只凭 `connect` 失败就清理是不安全的——那会在"server 正在启动、还没 bind/写端口"的窗口里
删掉别人即将使用的路径。**锁在前、清理在后**，顺序不能换。

## 3. 客户端计数与宽限窗口

### 计数的增删

| 事件 | `client_count` | 说明 |
|---|---|---|
| `Hello` 握手完成 | +1 | 只有完成版本/令牌协商才计数，半开连接不算 |
| 收到 `Bye` | −1 | 正常退出 |
| 端点读到 EOF | −1 | 崩溃 / `SIGKILL` / 终端被杀，**不依赖 `Bye`** |
| 握手超时（未发 `Hello`） | 不计数 | 防半开连接把 server 永久吊住 |

**必须靠 EOF 兜底。** 若只认 `Bye`，一个被 `kill -9` 的 client 会让 `client_count` 永远 ≥1，
server 永不退出——"最后 client 退出后 server 退出"这条需求直接失效。

### 宽限窗口的存在理由

`client_count` 归零后**不立即关服**：用户 `q` 退出后立刻重开是极高频动作，立即关服会让所有隧道
拆掉再建（端口闪断、重连风暴、远端进程感知到断开）。窗口内任何新 client 都会**取消**退出计时。

| 触发路径 | 宽限 | 理由 |
|---|---|---|
| 用户确认过的主动退出（`Bye{last_exit=true}` 且复核 `count==0`） | 短（≈500ms） | 用户明确表达了"我要退出全部"，只需覆盖"手滑" |
| 崩溃 / EOF 断开 | 长（≈5s，可配） | 无从确认意图，且**终端崩溃不该秒杀掉所有转发** |

两者语义不同，**不能共用一个常量**——这是本设计里最容易图省事写错的一处。

## 4. 最后一个客户端退出

> 用户明确追加的需求：**最后一个 client 退出时，必须给出风险提示：所有端口转发都要停止。**

它比"停用一条"严重得多，且**事后不易察觉**——正好命中既有确认闸判据
（[../../design.md](../../design.md) D11）三条中的两条。

### 提示的确切条件

| 条件 | 是否提示 |
|---|---|
| `client_count == 1`（退出后归零）**且**有运行中转发 | **提示**，文案必须说清「最后一个客户端」+「全部停止」+ 条数 |
| `client_count >= 2` | 不提示，普通退出 |
| `client_count == 1` 但无运行中转发 | 不提示（没有可停止的东西；多余的确认会训练用户无脑按 `y`） |

### 文案落点（逐字可测）

走既有 `ConfirmAction::Quit` 一个变体，**不新增变体**：

```text
这是最后一个客户端：退出后所有端口转发都会停止（{running} 条运行中）。确定退出？
Last client: quitting stops every port forward ({running} running). Quit anyway?
```

`q` 与 `ctrl+c` 的既有键位与"总是先确认"的行为不变。`{n}` 必须来自**快照的计数**，不能读
本地的 supervisor 运行数——client 已不持有 `Supervisor`，读它会静默显示 0
（这正是 `src/app/confirm.rs` 里那条注释的由来）。

### 权威性：client 的判断只是提议

client 依据的是**快照里的 `client_count`**，而这个值可能已经过期：

- client 确认后发 `Bye{last_exit:true}`，语义是"**用户已被告知并确认了后果**"；
- **server 收到 `Bye` 必须重新计数**：`count > 0` → **不退出**；`== 0` → 才进关服；
- **绝不允许** `last_exit` 直接触发关服——那等于让一个过期的客户端视图拥有停止所有人转发的权力。

可复现的交错（spike 断言 8c，`e2e` 用三个真实进程）：client 弹确认框 → 用户按 `y` → 在 `Bye`
送达前另一个 client 完成 `Hello` → server 计数 2−1=1 > 0 → **server 不退出**。

### 无法提示的不对称（必须承认）

client 崩溃、`SIGKILL`、终端窗口被关、SSH 会话断开——**没有机会弹任何确认框**。
结果是：**最后一个终端崩溃 = 全部转发静默停止**。只能靠策略，两条都做：

| 措施 | 说明 |
|---|---|
| 长宽限窗口 | 崩溃路径用完整宽限（默认 5s，可配），覆盖"终端重开/误关" |
| `--keep-server` | 逃生阀：最后 client 退出后 server 常驻。**明确文档化为与需求相反的可选项** |
| 意图保留 | `enabled` 本就持久化，server 下次启动按 `restore_on_start` 恢复——隧道会回来，只是中间断过 |

## 5. 关服顺序

```text
1. client_count == 0 且宽限已过（或 SIGTERM 收到的强关）
2. 记录 server.shutdown_begin（含 count、active 条数、触发原因）
3. 逐条停隧道；等宽限；超时记既有 tunnel.stop_timeout
4. save_state()                  ← 既有 state.toml，写最后状态
5. 删除端点文件（port/token）     ← 先摘掉入口，之后不会再有新 client 连上
6. 显式释放 election lock、清空 running 表
7. 记 app.shutdown（既有事件，字段不变）
8. 从事件循环正常 return，让 main 返回 ExitCode
```

**顺序要点**：

- **先删端点再退**：不删会留下"文件在、无人 listen"的残留，下一个 client 虽能靠 ①②③ 恢复，
  但那多绕一圈；正常退出不该制造需要恢复的现场。
- **不要 `std::process::exit()`**。仓库既有硬约定：`process::exit` 跳过 `Drop`，会让隧道不停、
  终端留在 raw mode。关服必须**`return` 出主循环**，由 `Drop` 兜底再停一次（幂等）。
- **停完再写 `state.toml`**：写的是"停止后"的真实终态，而不是"停止前"的残留。

## 6. 信号

沿用既有 `quit_signals`（Unix 含 `SIGHUP`/`SIGTERM`/`SIGINT`，Windows 只有 Ctrl-C）：

| 角色 | 信号 | 语义 |
|---|---|---|
| client | 任意 quit signal | 走**与 `q` 相同**的确认路径（`ctrl+c` 既有行为就是"先确认"） |
| server（前台 `--server`） | `SIGINT` / `SIGTERM` / `SIGHUP` | 等同"用户要关服"：跳过宽限直接执行关服顺序（**但仍走正常 return**） |
| server（detached） | `SIGTERM` | 强关 |
| server（detached） | 终端关闭（`SIGHUP`） | **忽略**：detached server 不该随拉起它的终端一起死 |

最后一行是**刻意的平台差异**：detached server 必须 `setsid`/忽略 `SIGHUP`，否则"拉起 server 的那个
client 的终端"拥有停止所有人转发的权力。这是安全语义，不是实现细节。

## 7. 崩溃与重连

### server 崩溃

| client 侧观察 | 行为 |
|---|---|
| 读到 EOF / 写失败 | 进入"连接丢失"状态，界面明确提示，**不静默** |
| 自动重启 server？ | **不自动**，只提示"server 已退出，按 ↵ 重启或按 q 退出" |

**为什么不自动重启**：自动重启会执行 `restore_enabled`，把 `enabled` 的隧道重新拉起——但用户此刻
的意图未知（可能是他在另一个终端 `kill` 了 server 想全停）。自动恢复等于"用户没要求的副作用"，
且是**制造连接**方向的副作用（比断开更危险），符合 D11 的"事后不易察觉"。

### client 重连状态机

```text
Connected ──EOF/write fail──► Disconnected ──用户按 ↵──► Reconnecting ──成功──► Connected
                                   │                          │
                                   │                          └──失败──► 显示原因，回 Disconnected
                                   └──用户按 q──────────────► 退出（此时**不**提示"全部停止"）
```

最后一条注意：server 已经崩溃时，client 退出**没有**"全部转发停止"的提示——因为已经没有转发在跑了。
误报一条"全部会停"比不报更糟（它会让用户以为还有救）。

## 8. 需要新增的日志事件（当时的草案）

遵循既有格式：点分 ASCII、字段带单位、级别按"读者要做什么"划分。**既有事件名一个都不改。**

| `event=` | 级别 | 关键字段 |
|---|---|---|
| `server.start` | INFO | `config`、`pid`、端点 |
| `server.election_lost` | DEBUG | `lock`、`msg`（抢锁失败，正常竞态，不是错误） |
| `server.stale_endpoint_removed` | INFO | 端点路径 |
| `server.client_connected` / `disconnected` | INFO | `count`、`reason`(`bye`/`eof`) |
| `server.grace_started` / `grace_cancelled` | INFO | `grace_ms`、`trigger` |
| `server.last_client_confirmed` | INFO | `tunnels_active` |
| `server.shutdown_begin` | INFO | `count`、`active`、`trigger` |
| `client.connected` / `disconnected` | INFO | `server_pid`、`reason` |

> 实际落地的事件清单以 [../../logging/events.md](../../logging/events.md) 的 `server.*` / `client.*` 两节为准。
