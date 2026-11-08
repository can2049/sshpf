# 诊断日志

`sshpf` 每次运行都会向一个追加写的文件写结构化日志。目的很具体：**出问题之后，
用户手上得有一份能贴给我们的东西**。TUI 的详情面板只活在进程里，进程一退就没了；
崩溃时更没有界面可看。

```
~/.local/state/sshpf/logs/sshpf.log        ← 当前文件
~/.local/state/sshpf/logs/sshpf.log.1      ← 上一次轮转（.1 最新）
~/.local/state/sshpf/logs/sshpf.log.2
~/.local/state/sshpf/logs/sshpf.log.3      ← 保留 3 个归档
```

- 目录 `0700`、文件 `0600`（日志里有主机名、端口、用户名）。
- 单文件超过 2 MiB 轮转，保留 3 个归档 ⇒ 磁盘占用上限约 8 MiB。
- **追加写**，不覆盖历史。想清空就直接删文件，下次运行会重新创建。
- `SSHPF_LOG_DIR` 换目录，`SSHPF_LOG_LEVEL=off|error|warn|info|debug` 换级别（默认 `info`）。

## 格式

一行一条记录，列顺序固定：

```
2026-11-08T18:55:42.957+08:00 INFO  seq=12 event=tunnel.ready tunnel=3 name=cloud-pc forward=local listen=127.0.0.1:19711 target=127.0.0.1:22 host=cloud_pc elapsed_ms=812
└─────────────┬────────────┘ └─┬──┘ └─┬───┘ └─────┬──────┘ └───────────────────┬──────────────────────────────┘ └───┬────┘
        RFC 3339 本地时间      级别   序号       事件名              隧道身份字段（固定 6 个）                  调用方字段
```

| 部分 | 说明 |
|---|---|
| 时间戳 | RFC 3339、毫秒精度、带时区偏移，可直接与 `journalctl` / sshd 日志对齐 |
| 级别 | `ERROR` / `WARN` / `INFO` / `DEBUG`，定宽 5 列 |
| `seq` | 本次运行内的递增序号；**轮转后从 1 重新计数**，跨文件比对请用时间戳 |
| `event` | 稳定的事件名，见下方目录。诊断从 `grep event=<名字>` 开始 |
| 字段 | `key=value`，含空格 / 引号 / 反斜杠 / 控制字符时加引号并转义（`\\` `\"` `\n` `\r` `\t` `\xNN`） |

一条记录不会跨行、也不会跨文件：值里的换行会被转义，轮转发生在写入之前，
所以 `grep`/`awk` 按行处理永远安全。

## 级别策略

| 级别 | 用来记什么 | 例子 |
|---|---|---|
| `ERROR` | 用户要做的事没做成；隧道已死且不会自己重试 | `tunnel.rejected`、`tunnel.failed`、`config.save_failed` |
| `WARN` | 降级但自愈：安排了重连、学到新主机密钥、配置被修正、权限被收紧 | `tunnel.retry_scheduled`、`knownhosts.learned`、`ui.slow_frame` |
| `INFO` | 预期内的生命周期：启动、隧道状态变化、用户动作 | `app.start`、`tunnel.ready`、`action.applied` |
| `DEBUG` | 高频细节：每条转发进度行、每次 SOCKS5 失败、状态机每一步 | `tunnel.line`、`socks5.rejected`、`tunnel.state` |

`SSHPF_LOG_LEVEL=off` 只关掉常规记录，**崩溃记录仍然会写**——想把日志彻底关掉的人，
往往正是最需要那一行崩溃信息的。

## 崩溃记录

两种机制，覆盖不同的崩法：

### panic（Rust 层崩溃）

```
… ERROR seq=42 event=crash.panic thread=sshpf-tunnel-3 location=src/app.rs:112:9 msg="index out of bounds" backtrace=yes
… ERROR seq=43 event=crash.panic.trace msg="  | panic backtrace\n  |    0: sshpf::tunnel::run\n  |              at ./src/tunnel.rs:88:13\n  | … (更深的栈帧已省略)"
```

- `thread` 是关键：隧道各自跑在自己的线程上（`sshpf-tunnel-<id>`），
  线程名能区分"UI 线程挂了"和"某条隧道的线程挂了"。
- 栈最多保留 25 帧。debug 构建里前 30 帧大多是 `std::panicking` 的管道代码，
  真正有用的是栈顶几帧。
- 上游 hook 会被链式调用，所以 `ratatui` 的"恢复终端"逻辑照常生效。

### 致命信号（SIGSEGV / SIGBUS / SIGABRT / SIGILL / SIGFPE）

```
ts=1794000000 ERROR seq=0 event=crash.signal signal=11 name=SIGSEGV pid=4242
```

这一类**没有格式化时间戳**，只有 `ts=<epoch 秒>`：信号处理器里只能用
async-signal-safe 调用，`localtime_r` 首次调用会分配内存，正好会在我们最需要它的
时候再挂一次。所以它用 epoch，其余字段仍是 `key=value`，解析器不用特殊处理。

处理器只做四件事，全是安全的：

1. 若 TUI 持有终端，先写 `\x1b[?25h\x1b[?1049l` 退出备用屏（否则用户会卡在一片
   残留画面里，连光标都看不见）；
2. `write(2)` 一条手工拼好的记录到预先 `dup` 出来的 fd（不分配、不加锁、不格式化）；
3. 往 stderr 写一行"崩溃信息已写入 …（终端错乱请执行 reset）"；
4. 恢复默认处理器 → **解除内核对该信号的屏蔽** → 重新 `raise`。第 4 步的解除屏蔽
   是必须的：内核在处理器执行期间屏蔽该信号，不解除的话 `raise` 只是排队，
   随后 `_exit(139)` 抢先返回，进程就从"被 SIGSEGV 杀死"变成了"以 139 退出"，
   core dump 也不会产生。这一点有测试钉住（`a_fatal_signal_keeps_the_signal_exit_status`）。

递归崩溃（处理器自己再挂）由 `SA_RESETHAND` 兜住：第二次触发走默认动作，不会死循环。

## 事件目录

隧道级记录的前 6 个字段永远是 `tunnel` / `name` / `forward` / `listen` / `target` / `host`，
所以 `grep 'tunnel=3 '` 就是一条隧道的完整时间线。

### 进程与配置

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `app.start` | INFO | `version` `pid` `config` `log` `os` `arch` | 进程启动 |
| `app.cli_mode` | DEBUG | `mode` | `--check` / `--list-hosts` |
| `app.fatal` | ERROR | `stage` `msg` | TUI 起不来，进程退出 |
| `app.quit` | INFO | `reason` | 用户确认退出 |
| `app.exit` | INFO | `reason` `uptime_ms` | 正常退出 |
| `app.signal` | INFO | `signal` | SIGINT/SIGTERM/SIGHUP 触发退出 |
| `app.restore` | INFO | `tunnels` | 开始恢复上次启用的条目 |
| `app.restore_skipped` | INFO | `reason` | `--no-restore` |
| `app.shutdown` | INFO | `tunnels_active` `running` | 退出前停掉所有隧道 |
| `app.orphan_event` | DEBUG | `event` `tunnel` | 收到已删除隧道的事件 |
| `config.loaded` | INFO | `path` `tunnels` `problems` | 配置读入成功 |
| `config.invalid` | WARN | `path` `msg` | 每条校验问题的明细 |
| `config.load_failed` | ERROR | `path` `msg` `fallback` | 配置读不进来，以空配置启动 |
| `config.repaired` | WARN | `path` `entries` | 载入时修正了重复 id / 缺失时间戳 |
| `config.save_failed` | ERROR | `path` `msg` | 写盘失败：改动只在内存里 |
| `config.perms_tightened` | WARN/ERROR | `path` `ok` | 配置原先对同组/所有人可读，已收紧 |
| `state.save_failed` | WARN | `path` `msg` | 运行状态文件写失败 |
| `log.unavailable` | ERROR | `msg` | 日志打不开（同时会进状态栏） |
| `log.level_invalid` | WARN | `msg` | `SSHPF_LOG_LEVEL` 写错，回退到 info |

### SSH 配置与主机密钥

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `sshconfig.loaded` | INFO | `hosts` | `~/.ssh/config` 读到的主机数 |
| `sshconfig.load_failed` | WARN | `msg` | 读 / 解析失败 |
| `sshconfig.resolve_failed` | ERROR | 隧道字段 + `msg` | 某个 `ssh_host` 解析不出 endpoint |
| `knownhosts.learned` | WARN | `tunnel` `host` `port` `verdict` | `accept-new` 下学到新主机密钥 |
| `knownhosts.rejected` | ERROR | `tunnel` `host` `port` `policy` `msg` | 密钥变了 / 被吊销 / `strict` 下未知 |

### 隧道生命周期

`kind` 字段是 ASCII 稳定值（`port_in_use`、`auth_failed`、`timeout`、
`host_key_mismatch`、`dns_failure`、`connection_refused`、`permission_denied`、
`remote_forward_failed`、`target_unreachable`、`spawn_failed`、`exited`、
`unsupported`、`unknown`），旁边配一个 `short` 放界面上的中文短句——前者给 `grep`，
后者给人读。

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `tunnel.start` | INFO | 隧道字段 + `trigger` | 请求启动（`manual`/`restore`/`retry`/`batch`/`created`/`edited`） |
| `tunnel.rejected` | ERROR | `stage` `kind` `short` `msg` | 启动被拒；`stage` = `validate`/`preflight_bind`/`resolve_endpoint`，`kind` = `port_in_use`/`auth_failed`/… |
| `tunnel.spawn` | INFO | `endpoint` `via` `hostkey_policy` | 后台线程开始建连 |
| `tunnel.spawn_failed` | ERROR | `msg` | 线程都起不来 |
| `tunnel.ready` | INFO | `elapsed_ms` | 转发真正就绪（`bind` 成功 / `tcpip_forward` 被接受） |
| `tunnel.line` | DEBUG | `msg` | 驱动的一条进度行 |
| `tunnel.exit` | WARN / ERROR | `was_up` `code` `kind` `short` `msg` | 会话结束；`was_up=true` 是掉线（WARN），否则 ERROR |
| `tunnel.retry_scheduled` | WARN | `attempt` `delay_ms` `kind` `short` | 安排了退避重连 |
| `tunnel.retry` | INFO | `attempt` | 重连到点，开始重试 |
| `tunnel.failed` | ERROR | `kind` `short` `retryable` `msg` | 终态失败；`retryable` 说明等待是否有用 |
| `tunnel.stop` | INFO | `trigger` | 用户停止（`manual`/`stop_all`/`batch`） |
| `tunnel.created` | INFO | `source` | 新定义（`form`/`import`） |
| `tunnel.updated` | INFO | — | 定义被编辑 |
| `tunnel.deleted` | INFO | — | 定义被删除 |
| `tunnel.state` | DEBUG | `state` | 状态机每一步（`starting`/`up`/`retrying`/…） |
| `forward.target_failed` | WARN | `tunnel` `target` `msg` | 隧道通但目标端口连不上（隧道本身不挂） |
| `socks5.rejected` | DEBUG | `tunnel` `msg` | SOCKS5 握手/请求失败 |

### 用户动作

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `action.applied` | INFO | `action` `detail` | 确认闸放行 |
| `action.cancelled` | INFO | `action` | 在确认框上取消（"为什么它还在跑"的答案） |
| `action.batch` | INFO | `action` `marked` `changed` | 批量操作完成；`changed` 是真正生效的条数 |
| `action.requested` | DEBUG | `action` `detail` | 弹出了确认框 |
| `import.committed` | INFO | `host` `created` | 从 `~/.ssh/config` 导入成功 |
| `import.rejected` | WARN | `problems` `msg` | 导入校验失败，一条都没建 |
| `ui.slow_frame` | WARN | `elapsed_ms` | 一帧超过 500 ms（UI 卡顿的第一手证据） |
| `ui.no_mouse` | DEBUG | — | 终端不支持鼠标捕获，键盘仍可用 |

## 怎么用

```bash
# 一条隧道的完整时间线
grep 'tunnel=3 ' ~/.local/state/sshpf/logs/sshpf.log

# 只看真正出事的地方
grep -E ' ERROR | event=crash\.' ~/.local/state/sshpf/logs/sshpf.log

# 掉线/重连风暴：数一数某个隧道排了几次重连
grep 'event=tunnel.retry_scheduled' ~/.local/state/sshpf/logs/sshpf.log | grep -c 'tunnel=3 '

# 哪类失败最多（kind 是 ASCII 稳定值，short 是界面中文）
grep -oP 'event=tunnel\.failed .*?kind=\K[a-z_]+' ~/.local/state/sshpf/logs/sshpf.log | sort | uniq -c

# 崩溃前后各 20 行
grep -n 'event=crash' ~/.local/state/sshpf/logs/sshpf.log
sed -n '<行号-20>,<行号+20>p' ~/.local/state/sshpf/logs/sshpf.log

# 单独把 panic 的栈拉出来
sed -n '/event=crash.panic.trace/p' ~/.local/state/sshpf/logs/sshpf.log | sed 's/\\n/\n/g'

# 报 bug 时附上（含归档，才是完整历史）
tar czf sshpf-logs.tgz ~/.local/state/sshpf/logs/
```

**提 issue 时请附上整个 `logs/` 目录。** 单纯贴最后几行通常不够——重连风暴、
配置被改、主机密钥被学到的证据都在更早的位置。

## 隐私

写进日志的：主机名、端口、用户名、失败原因、路径、版本、pid、栈。
**从不写**：私钥内容、passphrase、`known_hosts` 内容、任何从 agent 读出的密钥材料。
日志文件 `0600`、目录 `0700`，与配置文件同级别。

主机名和用户名仍然是敏感信息，报 bug 前如果不方便公开，可以先
`sed -E 's/(host|name)=[^ ]*/\1=REDACTED/g'` 处理一遍。
