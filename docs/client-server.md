# client-server 运行模型 / Client-server runtime

> **这份文档回答**：同时开多个 `sshpf` 时谁在真正持有转发、状态怎么同步、server 什么时候退出、传输层为什么两个平台用同一套。
> **不负责**：帧格式与消息字段（[research/client-server/protocol.md](research/client-server/protocol.md)）、隧道本身的启停与重连（[runtime.md](runtime.md)）、渲染（[tui.md](tui.md)）。
> **相关代码路径**：`src/server/`、`src/client/`、`src/ipc/`、`src/app/mirror.rs`。

**一句话：一个 server 持有全部隧道，多个 client 连上它并渲染同一份快照；最后一个 client 退出时 server 才退出。**

## 为什么不是"每个窗口自己转发"

隧道不是一份可以复制的东西：它有本地监听端口、一条 SSH 会话、一份重连退避状态。两个窗口各自持有就会出现两个进程抢同一个 `127.0.0.1:8080`，第二个必然失败。所以只有一个进程能拥有它。

## 角色 / Two roles, one `App`

| 启动方式 | 角色 | 拥有隧道？ | 恢复 `enabled`？ | 写配置文件？ |
|---|---|---|---|---|
| `sshpf` | client | 否 | **否**（server 负责） | 否 |
| `sshpf --server` | server（前台，无界面） | 是 | 是 | 是 |
| `sshpf --keep-server` | client，且告诉 server 别因它退出而退出 | 否 | 否 | 否 |
| `sshpf --check` / `--list-hosts` | 都不是，**完全不连 server** | — | — | 只读 |

两者跑的是**同一个 `App`**，差别只有一个字段 `app.mirror`：

| `app.is_mirror()` | 含义 | 一个变更操作的行为 |
|---|---|---|
| `false` | 本进程是权威 | 直接操作自己的 `Supervisor`，然后落盘 |
| `true` | 本进程只是视图 | 往队列里放一条 `Op`，等下一份快照 |

见 `src/app/mirror.rs`。这就是为什么 `ui`/`keys`/`form` 一行都不用改。

### 恢复 `enabled` 只在 server 里发生一次

`App::restore_enabled` 由 [`server::run`](../../src/server.rs) 在起 pump 之前调用**一次**；
client 的 `prepare_app` 遇到 mirror 会直接 return，所以两处不会抢。
`--no-restore` 由此也必须是 **server 侧**的开关：client 通过 `SpawnRequest` 的
`--no-restore` 参数把它传给拉起的 server（`src/client/spawn.rs`）。

> 这曾是实测缺陷：`--server` 从不调用 `restore_enabled`，于是配置写着
> `restore_on_start = true`、单进程版也照做，**只有 client-server 路径静默不恢复**——
> 服务端启动后每条转发都是 `Stopped`，直到用户按 `a`。

## 传输层为什么用回环 TCP / Why loopback TCP

| 方案 | Linux | Windows |
|---|---|---|
| Unix 域套接字 | 可用 | **`std` 没有 API** |
| named pipe | — | 需要第三方 crate（禁止新增依赖） |
| **回环 TCP** | 可用 | 可用 |

选回环 TCP 不是因为它更好，而是因为它是**唯一一条两端都只有一套代码**的路。代价是多一次 TCP 握手、多一个发现步骤；收益是没有第二套失败模式。

### `127.0.0.1` 不是权限边界

本机任何用户都能连任何回环端口。真正的边界是**令牌**：server 启动时生成 128-bit 令牌写入私有目录下的 `<hash>.token`（Unix `0600`，目录 `0700`，Windows 落在 per-user temp 继承 profile ACL）；client 必须把令牌放进 `Hello`，server 校验通过才发任何状态。所以「能连上端口」不等于「能被服务」——令牌错误回 `bad_token` 并立即断开，且**不回显密钥**。

### 端点 = 哪份配置 / Identity is the config

`config_path` 的 FNV-1a-64 → 三个文件：`<hash>.port`（server 绑定的端口，原子写：先写 `.tmp` 再 rename）、`<hash>.token`（本次运行的令牌）、`<hash>.lock`（单例锁，`File::try_lock`）。同一份配置的窗口连同一个 server；不同配置是两个 server，互不干扰。`SSHPF_IPC_DIR` 可整体改写目录（测试用它隔离）。

## 单例：`try_lock` 就是选举 / The lock is the election

1. 想当 server 的进程先 `try_lock` 这个配置文件对应的 lock；
2. 拿到 → 它就是 server，清掉上一个进程留下的 `port`/`token`（那是死掉的 server），绑端口、发布、开始服务；
3. 没拿到 → 已有 server（或正有一个在启动），本进程当 client。

**进程死亡即释放锁**，包括 `SIGKILL` 和 `TerminateProcess`——内核关闭句柄，所以不需要"心跳+超时判死"那一套。顺序是**先抢锁、后清文件**：反过来会让两个并发进程互相删掉对方刚发布的端口。

## 同步 / How state reaches every window

```text
隧道线程 ─► event_rx ─► pump（200ms，无 UI）─► ServerState.tunnels ─► revision+1
                                                          └─► 广播 Snapshot（全量）
client 线程 ─► 帧 ─► sync::drain（每帧一次）─► 丢弃旧 revision ─► App.tunnels ─► ui::draw
```

| 纪律 | 为什么 |
|---|---|
| 只发**全量**快照，不发增量 | 丢一帧、重一帧都不会让某个窗口永久落后 |
| `revision` 单调，旧的不覆盖新的 | 乱序到达不该让表格倒退 |
| 快照只替换**共享**部分 | 光标、浮层、正在填的表单属于这一屏，不能被别人的改动关掉 |

### 什么时候发 / When a snapshot goes out

泵每 200ms 问一次「客户端需要被告知吗」，有**两个独立的触发条件**，缺一个就会出
「明明连上了却一直显示连接中」：

| 触发 | 由谁产生 | 漏掉它的后果 |
|---|---|---|
| `revision` 变了 | 一条**命令**被应用（[`commands`](../src/server/commands.rs)） | 客户端拿不到新的 `expect_rev` |
| **内容签名**变了 | 事件泵自己做的任何事：`Ready`、安排重试、进度行、会话结束 | **转发已经通了，所有窗口却一直停在 `◐ 连接中`** |

第二行是曾经的实测缺陷。`revision` 只在命令路径上自增，而 `Ready` 来自隧道线程、
根本不经过命令，于是泵认为「没变化」、什么都不发——用户看到界面一直转圈，
但在同一个终端里 `ssh <host>` 却是通的。判据必须是**客户端会看到什么**，
不能是**谁改的**。

签名（[`signature`](../src/server/signature.rs)）只保留读者能感知的部分：
每条隧道的 `TunnelStatus`（含 `Instant`）、`retries`、日志环的**写入计数**
（不是 `len()`——环满时压入会逐出最旧的，长度不变而内容已变），外加 `client_count`。
时钟读数一律不入签名：快照的时间戳每次都要重新取，带上它们会让「无变化」永不成立，
从而每 200ms 全量广播一次。

**client 从不读自己的墙钟**：`Instant` 是进程内单调钟，跨进程无意义，所以 wire 上一律是 server 时钟的毫秒戳，client 用「与 `server_now_ms` 的差值」重建本地 `Instant`。两台机器的时钟差 5 分钟也不影响"已运行 3m"的显示。

## 最后一个 client 退出 / Last client exit

**这是本特性唯一会让人意外的地方**：关掉最后一个窗口 = 停掉所有转发，包括别的（已关掉的）窗口启动的。

| 环节 | 做法 |
|---|---|
| 事前提示 | `client_count <= 1` 且确有转发在跑时，确认框明说「这是最后一个客户端：退出后所有 N 条端口转发都会停止」 |
| 主动退出 | client 发 `Bye{last_exit:true}`，**只是提议** |
| server 复核 | 收到 `Bye` 后先减计数：`> 0` → 不退出，回 `Reply{remaining}`；`== 0` → 进关服 |
| 异常断连 | 没有 `Bye` 也照样减计数，只是走更长的宽限 |

**为什么 `last_exit` 不能直接触发关服**：client 的计数来自快照，可能已经过期。它以为自己是最后一个，而另一个窗口刚连上——若信任 `last_exit` 就会把别人刚连上的转发全部掐掉。破坏权只在 server 手里。

### 宽限窗口 / Grace window

| 触发 | 窗口 | 理由 |
|---|---|---|
| 主动 `Bye{last_exit:true}` | **0.5s** | 用户已经确认过文案，只需覆盖"Bye 与同时连入擦肩而过" |
| 异常断连（socket EOF） | **5s** | 事故：留出重开终端、重新连上的时间 |
| `--keep-server` | 不退出 | 逃生阀：明确文档化为与需求相反的可选项 |

窗口内**任何** client 连上都会取消关服（`server.grace_cancelled`）。计数在关服判定前再复核一次，所以"最后一个刚走、又有人连上"不会误关。

## 命令是提议，server 是权威 / Commands are proposals

client 发的是**意图**（启动这条、删那条），不是指令。server 收到后一律重做校验：

| client 传来的 | server 怎么做 |
|---|---|
| `cfg` 里的 `id` | **忽略**，由 server 分配（两个 client 同时新建才不会撞号） |
| "这个定义合法" | 重跑 `validate_tunnel` |
| "这个端口空着" | 重跑监听冲突检查 |
| "这条是新的" | 重跑 `duplicate_of`（过期视图可能漏判） |
| `expect_rev` | 与当前 revision 比对，不一致回 `Stale`，让用户决定是否重试 |

端口扫描（`p`）在 server 模式**暂不支持**：它需要往表单里装结果，而 server 没有表单。回 `unsupported` 明确拒绝，而不是静默无反应。

## 客户端不会因为断线而重启 server / A reconnect never re-spawns

第一次连不上时，client 会拉起一个分离的 `--server` 子进程（`DETACHED_PROCESS` / `process_group(0)`，与拉起它的终端脱钩）。**之后只重连，绝不再 spawn**。理由：spawn 会跑 `restore_enabled`，把 `enabled` 的隧道全部拉起来——因为一次网络抖动就自动复活用户刚停掉的转发，是在"制造监听端口"方向上的副作用，不能由一次连接失败决定。

## 排障 / Recipes

| 症状 | 先看 |
|---|---|
| 两个窗口内容不一样 | `grep 'event=server.client_connected\|server.grace' 日志`（同一个日志文件，两个角色都写） |
| **界面一直显示「连接中」，但端口其实在监听** | 这是「泵不发快照」的特征。先确认端口：`netstat -ano \| findstr :<端口>`。若确实在监听而界面不动，检查 `server::signature` 是否漏了某个可观察字段 |
| 关了窗口转发还在 | 是不是 `--keep-server`？看 `server.shutdown_begin` 有没有出现 |
| 关了窗口转发全停了，但还有别的窗口 | 那个窗口是否已崩溃/被 kill；`server.client_disconnected` 的 `reason` |
| 「已有 server 在运行该配置」 | 正常：`--server` 是单例，第二个以 0 退出 |
| 某个窗口连不上 | `server.connection_refused` 的 `reason`：令牌/版本/配置哪个不对 |

事件目录见 [logging/events.md](logging/events.md) 的 `server.*` / `client.*` 两节。
