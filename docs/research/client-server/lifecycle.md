# 生命周期与单例选举 / Process lifecycle & singleton election

> **这份文档回答**：client 与 server 两个进程怎么起、谁先谁后、怎么保证全局只有一个 server、
> 最后一个 client 退出时发生什么（**以及用户在哪一步被告知全部转发将停止**）、崩溃与残留资源怎么收尾。
> **不负责**：协议消息字段（见 [protocol.md](protocol.md)）、代码怎么改（见 [migration.md](migration.md)）、
> 为什么选 UDS 而不是 TCP（见 [protocol.md](protocol.md#传输层--transport)）。
> **相关代码路径**：`src/main.rs`（入口与信号）、`src/cli.rs`（参数与 `USAGE`）、`src/app/runtime.rs`
> （`restore_enabled`）、`src/tunnel.rs`（`Supervisor::stop_all` 与 `Drop`）、`src/config.rs`（`Paths`）、
> `src/sys/mod.rs`（`quit_signals`）。设计完成后新增 `src/server/`、`src/ipc/`。

## 术语 / Terms

| 词 | 含义 |
|---|---|
| `config_path` | 本次运行的配置文件路径（`-c` 或默认 XDG 路径），**server 的身份** |
| `socket_path` | 该 server 的 UDS 路径，由 `config_path` 派生 |
| `lock_path` | 单例选举用的文件锁路径，`File::lock` 目标 |
| `client_count` | 当前连在 server 上的 client 数（server 自己数的，权威） |
| `grace window` | `client_count` 归零后等待多久才关服的窗口 |
| `last_exit` | client 在 `Bye` 里声明「用户已确认后果」，**只是提议，不是命令** |

## 进程模式 / Process modes

| 调用 | 角色 | 连 server？ | 恢复 `enabled` 隧道？ |
|---|---|---|---|
| `sshpf` | client（无 server 则乐观拉起） | 是 | **否** |
| `sshpf --server` | 前台 server（调试 / systemd） | 它是 server | **是** |
| `sshpf --keep-server` | client，退出后 server 常驻 | 是 | 否 |
| `sshpf --check` | 一次性校验，打印后退出 | **否** | 否 |
| `sshpf --list-hosts` | 一次性列主机，打印后退出 | **否** | 否 |
| `sshpf -c PATH` | 以上任一，作用于该配置身份 | 按上面 | 按上面 |

**两条关键区分（最容易写错的地方）：**

1. **只有 server 恢复 `enabled` 隧道。** 现有 `App::restore_enabled`（`src/app/runtime.rs`）在
   `restore_on_start` 为真时启动所有 `enabled` 隧道。client-server 之后，若 client 也调用它，
   第 2 个 client 会把已有隧道**再启动一遍**（`Supervisor::spawn` 对同一 id 会先 `stop` 再起，
   表现为「连上就掉线再重连」）。所以恢复职责**整体搬进 server 启动路径**，client 启动时**不恢复**。
   `--no-restore` 因此只对 `--server` 模式有意义，其余模式退化为「不连 server」或「不触发恢复」。

2. **`--check` / `--list-hosts` 根本不连 server。** 它们是「读配置 + 打印」，引入 server 依赖会让
   `sshpf --check` 在无 server 时先拉起一个 server 再退出——这是可观察的行为变化，必须避免。

## 单例选举 / Singleton election

### 为什么需要它

多个 client 并发启动时，每个都可能判定「没有 server」并各拉一个。若两个 server 同时活着并各自绑定
隧道端口，第二个的 `bind` 会失败，用户看到随机一半端口起不来。

### 判定顺序（顺序本身是安全性的一部分）

```text
client 启动
  │
  ① connect(socket_path)
  │    成功 ──────────────────────────────► 我是 client，正常会话
  │    失败(err)
  ▼
  ② 取本地 election lock（.lock，File::try_lock 语义）
  │    拿到锁 ──► 说明"当前没有活着的 server 在服务这个 config_path"
  │                │
  │                ├─ ③ 若 socket_path 存在：它只可能是残留（上次被 SIGKILL）
  │                │     → unlink(socket_path)，记 server.stale_socket_removed
  │                ├─ ④ spawn detached server 子进程
  │                └─ ⑤ 释放锁，退避重试 connect（20ms→40ms→…，总预算 5s）
  │   拿不到锁 ──► 说明"另一个进程正在选举/正在服务"
  │                └─ ⑤ 退避重试 connect，不 spawn（总预算 5s）
  ▼
  ⑥ 预算内连上 → 我是 client
     预算耗尽   → 报错可操作错误，退出码非 0（不静默降级成"自己转发"）
```

**为什么「拿到锁」就能认定没有活 server？** 因为活的 server **在整个生命周期里一直持锁**。
锁的持有者是 server 进程本身，进程死亡（含 `SIGKILL`）时内核自动释放 `flock`——这是本方案不需要
pid 文件、不需要心跳、不需要超时探测的根本原因。这一点已在本机实测：
两进程抢同一路径，先到者 `GOT LOCK`，后到者立刻 `BUSY: operation would block`。

**为什么「拿不到锁」不能直接放弃？** 拿不到锁的两种真实情况必须区分开：
(a) 已有一个 server 在服务 → 应该当 client；
(b) 另一个进程刚好在 spawn server 的途中（锁已被它拿到、socket 还没 bind）→ 稍等即可。
两种情况的最优动作都是「退避重试 connect」，所以这里**不需要**区分它们。

### 并发启动两个 client 的时序

```text
时刻   client A                         client B                         server 进程
 t0    connect → 失败                    connect → 失败                    （无）
 t1    try_lock → 成功                    try_lock → 失败(BUSY)
 t2    spawn server ────────────────────►                                 fork
 t3    释放锁                                                              try_lock → 成功，持锁
 t4    connect(重试) → 失败               connect(重试) → 失败              bind(socket_path) 成功
 t5    connect → 成功 ──────────────────────────────────────────────────►  接受 A，count=1
 t6                                       connect → 成功 ─────────────────►  接受 B，count=2
```

A 与 B 最终都是 client，只有一个 server。**败者不需要任何协调协议，也不需要知道自己输给了谁**——
它只是重连成功而已。这是「乐观拉起 + 锁即真相」这个设计最值钱的地方。

### 残留资源

| 资源 | 谁创建 | 谁清理 | `SIGKILL` 后 |
|---|---|---|---|
| `socket_path` | server `bind` 时 | server 正常退出 `unlink` | **残留**（文件在，但没人 listen）→ 由 ①②③ 流程识别并 `unlink` |
| `lock_path` | server 首次 `open` | 不需清理（文件可长期存在） | **自动释放**（内核行为），文件留着无害 |
| pid 文件 | **不需要** | — | — |

**不要 pid 文件**：它的唯一用途是「判断持有者是否还活着」，而这正是 `flock` 免费提供的。
多一个 pid 文件就多一份「文件说活着但进程已死」的失效模式要处理。

**残留 socket 的识别为什么安全？** 判据是「`connect` 得到 `ECONNREFUSED`（或 `ENOENT` 之外的失败）
**且**能拿到 election lock」。只凭 `connect` 失败就 `unlink` 是不安全的——那会在
「server 正在启动、还没 bind」的窗口里删掉别人即将使用的路径。**锁在前、`unlink` 在后**，
顺序不能换。

## 客户端计数与宽限窗口 / Client count & grace window

### 计数的增删

| 事件 | `client_count` | 说明 |
|---|---|---|
| `Hello` 握手完成 | +1 | 只有完成版本协商才计数，半开连接不算 |
| 收到 `Bye` | −1 | 正常退出，client 主动告别 |
| socket 读到 EOF | −1 | 崩溃 / `SIGKILL` / 终端被杀，**不依赖 `Bye`** |
| 握手超时（未发 `Hello`） | 不计数 | 防半开连接把 server 永久吊住 |

**必须靠 EOF 兜底。** 若只认 `Bye`，一个被 `kill -9` 的 client 会让 `client_count` 永远 ≥1，
server 永不退出——「最后 client 退出后 server 退出」这条需求直接失效。

### 宽限窗口的存在理由

`client_count` 归零后**不立即关服**，而是等 `grace window`：

- 用户 `q` 退出后立刻重开（或 `sshpf` 被 shell 的 up-arrow 重跑）是极高频动作。
  立即关服会让所有隧道拆掉再建，端口闪断、重连风暴、远端进程感知到连接断开。
- 宽限窗口内的任何新 client 都会**取消**退出计时（`server.grace_cancelled`）。

| 触发路径 | 建议宽限 | 理由 |
|---|---|---|
| 用户确认过的主动退出（`Bye{last_exit=true}` 且复核 `count==0`） | 短（≈500ms） | 用户明确表达了「我要退出全部」，只需覆盖"手滑" |
| 崩溃 / EOF 断开 | 长（≈5s，可配） | 无从确认意图，且**终端崩溃不该秒杀掉所有转发** |

两者语义不同，不能共用一个常量——这是本设计里最容易图省事写错的一处。

## 最后一个客户端退出 / Last client exit

> 用户明确追加的需求：**最后一个 client 退出时，必须给出风险提示：所有端口转发都要停止。**

这是本方案最危险的破坏性后果：用户以为只是关掉一个终端窗口，实际停掉的是**全部**转发
（包括其它历史上启动的）。它比"停用一条"严重得多，且**事后不易察觉**——正好命中仓库既有
确认闸判据（`docs/design.md` D11）的三条中的两条（会断连接、事后不易察觉）。

### 提示的确切条件

| 条件 | 是否提示 |
|---|---|
| `client_count == 1`（退出后归零）**且**有运行中转发 | **提示**，文案必须说清「最后一个客户端」+「全部停止」+ 条数 |
| `client_count >= 2` | 不提示，普通退出（仍有别的 client 在，转发不受影响） |
| `client_count == 1` 但无运行中转发 | 不提示（没有可停止的东西；多余的确认会训练用户无脑按 `y`） |

第二行是刻意设计：**只在真的会停止全部转发时打扰用户**。给高频动作加确认会让用户习惯性按 `y`，
真正危险时的确认随之失效（D11 的原话）。

### 文案落点（逐字可测）

现有实现是 `src/app/confirm.rs` 的 `count_prompt` → `ConfirmAction::Quit` 分支，
当前文案为：

```text
还有 {running} 条转发在运行，退出将全部停止。
{running} forwards are still running; quitting stops them all.
```

改造后**只在「最后一个 client」时**增强为（中文在前、英文在后，保持双语纪律）：

```text
这是最后一个客户端：退出后所有端口转发都会停止（{running} 条运行中）。确定退出？
Last client: quitting stops every port forward ({running} running). Quit anyway?
```

仍走既有 `ConfirmAction::Quit` 一个变体，不新增变体；`q` 与 `ctrl+c` 的既有键位与
"总是先确认"的行为不变。若需要携带 `last_exit` 给 `Bye`，在 `confirm_yes` 的分支里附带即可。
`src/help/notes.rs` 的 Safety 一节与 `docs/keymap.md` 的确认清单需同步一句话，
`tests/help_coverage.rs` 必须仍绿。

### 权威性：client 的判断只是提议

client 依据的是**快照里的 `client_count`**，而这个值可能已经过期：它以为自己是最后一个，
期间另一台机器上的 client 刚连上。因此：

- client 确认后发 `Bye { last_exit: true }`，语义是「**用户已被告知并确认了后果**」；
- **server 收到 `Bye` 必须重新计数**：本次离开后 `count > 0` → **不退出**，正常继续服务；
  `count == 0` → 才进入关服流程；
- **绝不允许** `last_exit` 直接触发关服。那等于让一个过期的客户端视图拥有停止所有人转发的权力。

对应的可复现交错（spike 断言 8c）：client 弹出确认框 → 用户按 `y` → 在 `Bye` 送达前
另一个 client 完成 `Hello` → server 的计数为 2−1=1 > 0 → **server 不退出**。
不是靠时序运气，而是**协议上没有让 client 拥有决定权**。

### 无法提示的不对称（必须承认）

client 崩溃、`SIGKILL`、终端窗口被关、SSH 会话断开——这些路径**没有机会弹任何确认框**。
结果是：**最后一个终端崩溃 = 全部转发静默停止**。这不是可以靠文案解决的，只能靠策略。

结论（选 (c)，两条都做）：

| 措施 | 说明 |
|---|---|
| (a) 长宽限窗口 | 崩溃路径用完整宽限（默认 5s，可配 `shutdown_grace_ms`），覆盖"终端重开/误关"这类误伤 |
| (b) `--keep-server` | 逃生阀：最后 client 退出后 server 常驻。**明确文档化为与需求相反的可选项**，给「我不想它停」的用户 |
| (c) 意图保留 | `enabled` 本就持久化在 `config.toml`，server 下次启动按 `restore_on_start` 恢复。隧道会回来，只是中间断过 |

代价要写清：宽限窗口让"真的想全停"的用户多等 5 秒；`--keep-server` 引入了"没有 client 但 server 在跑"
的状态，需要文档解释。接受这两个代价，因为反方向的代价（静默杀掉所有转发）更贵。

## 关服顺序 / Shutdown order

```text
1. client_count == 0 且宽限已过（或 SIGTERM 收到的强关）
2. 记录 server.shutdown_begin（含 count、active 条数、触发原因）
3. Supervisor::stop_all()        ← 逐条停；等宽限；超时记 tunnel.stop_timeout（既有事件）
4. save_state()                  ← 既有 state.toml，写最后状态
5. unlink(socket_path)           ← 先摘掉入口，之后不会再有新 client 连上
6. 显式释放 election lock、清空 running 表
7. 记 app.shutdown（既有事件，字段不变）
8. 从 event_loop 正常 return，让 main 返回 ExitCode
```

**顺序要点：**

- **先 `unlink` 再退**：不 `unlink` 会留下"文件在、无人 listen"的残留，下一个 client 虽能靠
  ①②③ 流程恢复，但那多绕一圈；正常退出不该制造需要恢复的现场。
- **不要 `std::process::exit()`**。仓库既有硬约定（`docs/runtime.md`、`src/main.rs` 注释）：
  `process::exit` 跳过 `Drop`，会让隧道不停、终端留在 raw mode。server 的关服因此必须
  **`return` 出主循环**，由 `Supervisor::drop` 兜底再 `stop_all` 一次（幂等）。
- **`stop_all` 之后再写 `state.toml`**：写的是"停止后"的真实终态，而不是"停止前"的残留。

## 信号 / Signals

沿用 `src/sys/mod.rs::quit_signals`（Unix 含 `SIGHUP`/`SIGTERM`/`SIGINT`，Windows 只有 Ctrl-C）：

| 角色 | 收到信号 | 语义 |
|---|---|---|
| client | 任意 quit signal | 走**与 `q` 相同**的确认路径（`ctrl+c` 既有行为就是"先确认"） |
| server（前台 `--server`） | `SIGINT` | 等同"用户要关服"：跳过宽限，直接执行关服顺序（**但仍走正常 return，不 `process::exit`**） |
| server（前台 `--server`） | `SIGTERM` | 同上；`SIGHUP` 同 `SIGTERM`（终端关闭不该让隧道变孤儿） |
| server（detached） | `SIGTERM` | 强关，同上面 |
| server（detached） | 终端关闭（`SIGHUP`） | **忽略**：detached server 不该随拉起它的终端一起死，否则第一个 client 的终端一关，所有人的转发全停 |

最后一行是本设计里一个**刻意的平台差异**：detached server 必须 `setsid`/忽略 `SIGHUP`，
否则「拉起 server 的那个 client 的终端」拥有停止所有人转发的权力。这是安全语义，不是实现细节。

## 崩溃与重连 / Crashes & reconnect

### server 崩溃

| client 侧观察 | 行为 | 结论 |
|---|---|---|
| 读到 EOF / 写失败 | 进入"连接丢失"状态，界面上明确提示，**不静默** | 用户必须知道转发已停 |
| 自动重启 server？ | **不自动**，只提示「server 已退出，按 ↵ 重启或按 q 退出」 | 见下 |

**为什么不自动重启 server：** 自动重启会执行 `restore_enabled`，把 `enabled` 的隧道重新拉起——
但用户此刻的意图未知（可能是他在另一个终端 `kill` 了 server 想全停）。自动恢复等于
「用户没要求的副作用」，且是**制造连接**方向的副作用（比断开更危险）。所以选择让用户按一下。
这符合 D11：制造新的监听端口属于"事后不易察觉"，应经用户之手。

### client 重连状态机

```text
Connected ──EOF/write fail──► Disconnected ──用户按 ↵──► Reconnecting ──成功──► Connected
                                   │                          │
                                   │                          └──失败──► 显示原因，回 Disconnected
                                   └──用户按 q──────────────► 退出（此时**不**提示"全部停止"，
                                                              因为 server 已不在了）
```

注意最后一条：server 已经崩溃时，client 退出**没有**"全部转发停止"的提示——因为已经没有转发在跑了。
误报一条"全部会停"比不报更糟（它会让用户以为还有救）。

## 新增日志事件草案 / New log events

遵循既有格式：点分 ASCII、字段带单位、级别按"读者要做什么"划分（`docs/logging/events.md`）。
**既有事件名一个都不改。**

| `event=` | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `server.start` | INFO | `config`、`pid`、`socket` | server 就绪 |
| `server.election_lost` | DEBUG | `lock`、`msg` | 抢锁失败，静默退出（正常竞态，不是错误） |
| `server.stale_socket_removed` | INFO | `socket` | 识别并清理了 `SIGKILL` 残留 |
| `server.client_connected` | INFO | `count` | client 握手完成 |
| `server.client_disconnected` | INFO | `count`、`reason`(`bye`/`eof`) | client 离开 |
| `server.grace_started` | INFO | `grace_ms`、`trigger`(`last_exit`/`eof`) | 计数归零，开始等待 |
| `server.grace_cancelled` | INFO | `count` | 宽限期内新 client 连入 |
| `server.last_client_confirmed` | INFO | `tunnels_active` | 用户确认了"最后客户端退出"的后果 |
| `server.shutdown_begin` | INFO | `count`、`active`、`trigger` | 开始关服 |
| `client.connected` / `client.disconnected` | INFO | `server_pid`、`reason` | client 侧连接状态 |

## 未决 / Open

| 问题 | 现状 |
|---|---|
| 并发选举的实测数字（8 进程抢占唯一性、残留恢复耗时） | 已由 [spike](../client-server-spike/README.md) 实测：8 进程恰好 1 个赢 |
| 本页的关服时序与跨进程竞态 | 已由 spike 的 `lifecycle`（线程模型）与 `e2e`（真实多进程）双重验证 |
| 宽限窗口默认值 5s 是否合适 | 需真实使用反馈；先给可配置项 |
| Windows | `std` 无 named pipe API，本 PR 不做，见 [migration.md](migration.md) |
