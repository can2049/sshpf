# 运行时 / Runtime：隧道生命周期、就绪、重试、事件泵

> **这份文档回答**：一条隧道怎么从"用户按了 space"变成"真的通了"、失败后谁决定重试、UI 线程怎么拿到事件、退出时怎么收尾。
> **不负责**：SSH 协议实现细节（见 [design.md](design.md) 与 `src/ssh/`）、渲染（见 [tui.md](tui.md)）、日志事件全表（见 [logging/events.md](logging/events.md)）。
> **相关代码路径**：`src/app/runtime.rs`（启停编排）、`src/app/events.rs`（事件泵：事件→状态转移、重试）、`src/tunnel.rs`（监督 + `TunnelEvent`）、`src/ssh/client.rs`（驱动）、`src/ssh/client/health.rs`（定时探活）、`src/main.rs`（事件循环）。

## 线程模型 / Threads

UI 线程保持**同步**：`crossterm::event::poll(200 ms)` 阻塞等键，每帧非阻塞地排空事件、处理到期重试、重绘。

```text
main 线程（同步）                        每条隧道一个后台线程
  poll(200ms) ── 键/鼠标 ──▶ App            current_thread tokio runtime
  try_recv ◀── TunnelEvent ─── mpsc ◀──  russh session / driver
  tick_retries()                             （握手、bind、转发数据）
  draw()
```

| 事实 | 值 / 位置 |
|---|---|
| 每帧最多消费事件数 | `MAX_EVENTS_PER_FRAME = 4096`（`src/app.rs`） |
| 每帧最多事件是有界的原因 | 一帧处理上万条会把 UI 卡死；超出留到下一帧 |
| 一条隧道一线程一 runtime 的理由 | 隔离性好（一条 panic 不传染），几条到几十条规模够用；10 条并发建连实测 80 ms |
| `russh::client::Handle` 跨 task 共享 | 必须 `Arc<Handle>`（它不是 `Clone`） |
| 隧道线程名 | `sshpf-tunnel-<id>`（panic 记录靠它区分是谁挂了） |

## 启动 / Start

`start_tunnel(id)` → `start_tunnel_with(id, trigger)`（`src/app/runtime.rs`）拆成两段：

| 段 | 做什么 | 失败时 |
|---|---|---|
| `preflight()` | 校验配置 → 端口占用预检 → 解析 `ssh_host` endpoint | 返回失败结论 → `tunnel.rejected`（`stage=validate`/`preflight_bind`/`resolve_endpoint`） |
| `launch()` | `Supervisor::spawn` → 起线程 → 状态置 `Starting` | `tunnel.spawn_failed` |

`trigger` 字段是稳定 ASCII 值，告诉读者**这次启动是谁发起的**：`manual` / `restore` / `retry` / `batch` / `created` / `edited`。

预检在**建连之前**做，因为这两类错误可以立刻确定、不值得等一次 SSH 握手：端口被占、主机解析不出来。

## 就绪判定 / Readiness

> **`TunnelEvent::Ready` 是通往 `TunnelStatus::Up` 的唯一路径。任何日志文本都不得改变状态。**

`app::on_ready` 只由 `Ready` 驱动。就绪信号来自真实结果，不是文本匹配：

| 类型 | 就绪 = |
|---|---|
| `-L` | `TcpListener::bind` 返回 `Ok` |
| `-D` | 同上（SOCKS5 监听器 bind 成功） |
| `-R` | `tcpip_forward()` await 到服务端对 `GLOBAL_REQUEST` 的回复 |

旧实现解析 `ssh -v` 的 `Entering interactive session.`，有三个实测过的假信号（双栈半失败、ProxyJump 子进程混入、`-R` 顺序倒置）——改为真实结果后这类 bug 结构上不可能再发生。

## 停止 / Stop

1. 置停止标志；
2. driver 的 `select!` 立刻返回 → runtime 丢弃 → socket 关闭 → 会话结束；
3. `Supervisor::poll` 等线程退出，有宽限期；超时记 `tunnel.stop_timeout`（UI 已判停止，但端口可能还被占着）。

端口释放实测 **172 µs**。`SIGKILL` 掉 sshpf 也不会留孤儿：没有子进程，socket 由内核关闭。30 次启停循环无 fd 泄漏。

## 失败分类与重试 / Classification & retry

失败分两类，决定"要不要等"：

| 类 | 包含 | 结果 |
|---|---|---|
| **配置类** | 端口占用、权限不足(<1024)、认证失败、主机密钥、远端转发被拒 | `Failed{reason}`（`✗`），**不重试** |
| **网络类** | 连接拒绝、超时、DNS、进程异常退出 | `Retrying{attempt, next_at}`（`◌`），指数退避 |

- 退避：`retry_initial_secs` 起步，翻倍，封顶 `retry_max_secs`（默认 2s→…→30s）；受每条隧道的 `auto_restart` 与全局 `defaults.auto_restart` 控制。
- `arm_retry` 启动时先插占位记录 `attempt = 0`；`tick_retries` 只处理 `attempt >= 1` 的记录——否则占位符会让程序在启动瞬间无限重连（集成测试抓到过的真实缺陷）。
- **重试链放弃**记 `tunnel.retry_abandoned`（带 `attempt` + `stage` + `kind`）：重试也修不好（比如端口一直被占），退避到此为止。
- `tunnel.failed` 带 `retryable` 字段，读者不用猜"等一下会不会自己好"。

`tunnel.failed` 与 `tunnel.retry_abandoned` 的区别、以及按 `kind` 统计失败分布，见 [logging.md](logging.md#排查配方--recipes)。

## 健康检查 / Health checks

**为什么需要**：`-L`/`-D` 的本地 listener 无法感知 SSH 会话已死——`accept()` 继续成功返回，端口却转不出去；Windows 休眠唤醒后就是这种半开连接，旧行为永远发现不了，界面一直 `●`。

| 项 | 值 |
|---|---|
| 频率 | 每条 `Up` 隧道由**自己的后台线程**每 `defaults.health_check_secs` 探一次；`0` = 关闭 |
| 探什么 | 向对端发 `keepalive@openssh.com` 全局请求（`want_reply=true`）并等回复 |
| 超时 | `PING_TIMEOUT = 5s`（`src/ssh/client/health.rs`），不超过最短间隔 |
| 判失效 | 探活超时 / 发送失败 / 会话已被 russh 关闭（`is_closed()`） |
| 判活着 | 收到回复；对端回 `REQUEST_FAILURE`（`Err(RequestDenied)`）**也算活着**——字节通了 |
| 失效后 | 线程以 `StaleConnection` 结束 → 既有 `Exited` → `was_up=true` → 退避重连（受 `auto_restart`） |
| 日志 | 失效记 `tunnel.health_failed`（WARN，带 `cause`/`detail`/`interval_ms`）；**成功不记 INFO** |

**判据必须留在隧道线程里**：探活的结论是一条"本次运行结束"的错误，`select!` 里哪个分支先返回，driver 就返回哪个——这才是既有 `Exited` 路径。另起一个 task 会让 serve 循环继续活着陪伴一个死会话，正是要修的 bug；而放在 UI 线程就把网络等待带进了 `poll(200ms)` 的同步循环。

→ [design.md](design.md) D36 · `src/ssh/client/health.rs`、`src/ssh/client/driver.rs`

## 事件泵 / Event pump

每帧的顺序（`src/app/events.rs` 头注释）：① 排空 channel（`MAX_EVENTS_PER_FRAME` 上限）② 回收 stderr EOF 与 exit 竞态的子进程 ③ 分类宽限期已过的延迟退出 ④ 触发到期重试 ⑤ 让临时状态消息过期。②–④ 各自还会产生状态变化，所以放在排空之后，不与排空交错。

`TunnelEvent`（`src/tunnel.rs`）只有 5 个变体——这是 app 与隧道线程之间的全部契约：

| 变体 | 何时 | app 侧的后果 |
|---|---|---|
| `Ready { id }` | 转发真的就绪 | `on_ready` → `Up`（唯一路径） |
| `Line { id, line }` | 一条进度/诊断行 | `on_line` → 内存日志环 + `DEBUG` 落盘 |
| `Exited { id, code, reason }` | 会话结束；`reason` 是 driver 已分类的失败 | `on_exit` → 分类归档（`was_up` 决定 WARN / ERROR） |
| `StderrClosed { id }` | 该次运行的行已全部送达 | 允许用完整日志做分类 |
| `SpawnFailed { id, error }` | 线程都起不来 | 直接失败态 |

两条必须守住的时序规则：

1. **失败原因随 `Exited` 一起传**，不靠"读最后几行日志"——退出事件与最后几行日志可能乱序。
2. **`was_up=true` 的掉线是 WARN，从未连上的失败是 ERROR。** 前者网络抖一下，系统会自愈；后者要用户动手。

`app.orphan_event`（`DEBUG`）表示收到了一条已删除隧道的事件——不是 bug，是删除与线程退出之间的正常竞态。

## 退出 / Shutdown

`App::shutdown()` → `Supervisor::stop_all()` → 逐条停止 → 记 `app.shutdown`（`tunnels_active` `running`）。

- 主循环退出后 `Drop` 依次停隧道；**不要用 `process::exit()`**，它会跳过这些 `Drop`。
- 信号只做原子写（置退出标志 + 记住是哪个信号），循环结束后记 `app.signal`、返回 `ExitCode::SUCCESS`。注册失败记 `app.signal_unavailable`（`ctrl+c` 不再走正常退出，但 `q` 仍可用）。
- 恢复启动：`restore_on_start` 或 `--no-restore`（后者记 `app.restore_skipped`）；`app.restore` 记录恢复了几条。

## 状态机 / Status transitions

```text
Stopped ──start──▶ Starting ──Ready──▶ Up ──Exited(was_up)──▶ Retrying / Failed
                      │                                        │
                      └──Exited(never up)──▶ Failed            └──retry 到点──▶ Starting
                                                               └──用户 stop──▶ Stopped
```

| 状态 | 符号 | 含义 | 自动离开 |
|---|---|---|---|
| `Stopped` | `○` | 用户意图为关 | 只有用户动作 |
| `Starting` | `◐` | 已 spawn，等就绪信号 | `Ready` 或 `Exited` |
| `Up` | `●` | 转发可用 | `Exited` |
| `Retrying{n}` | `◌` | 网络类失败，等第 n 次退避 | 定时器 → `Starting` |
| `Failed{reason}` | `✗` | 终态，改配置后手动 `r` | 只有用户动作 |

## 相关日志事件 / Events you will see

| 阶段 | 事件 |
|---|---|
| 启动 | `tunnel.start`、`tunnel.rejected`、`tunnel.spawn`、`tunnel.spawn_failed` |
| 就绪 | `tunnel.ready`（含 `elapsed_ms`） |
| 运行 | `tunnel.line`（DEBUG）、`tunnel.state`（DEBUG）、`forward.target_failed`、`socks5.rejected` |
| 探活 | `tunnel.health_failed`（WARN，失败的探活才记） |
| 结束 | `tunnel.exit`、`tunnel.failed`、`tunnel.retry_scheduled`、`tunnel.retry`、`tunnel.retry_abandoned` |
| 停止 | `tunnel.stop`、`tunnel.stop_timeout` |
| 异常 | `tunnel.thread_panic` |

字段与级别逐条见 [logging/events.md](logging/events.md#隧道生命周期--tunnel-lifecycle)。
