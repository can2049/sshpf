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

### 日志本身重入（`crash.log_reentrant`）

```
ts=1794000000 ERROR seq=0 event=crash.log_reentrant cause=crash.panic msg="日志写入重入，记录被跳过以避免死锁"
```

panic hook 也要写日志，而写日志要拿锁。**如果 panic 就发生在持锁的写入过程中**
（某个值的 `Display` 实现里 panic，或 panic 现场恰好停在 `record()` 中间），hook 再去
`lock()` 就是自己等自己：进程永久僵住，日志 0 字节，终端停在备用屏——比崩溃更难查，
因为它看起来像"卡死了"。

所以 hook 不靠"试着拿锁"，而是**事先知道自己在不在临界区**：用一个线程局部的标记，
进 `record()` 时置位（RAII，panic 展开时由 `Drop` 复位），重入时发现已置位就**放弃这条
常规记录**，改用启动时 `dup` 出来的 fd 裸 `write(2)` 写上面这一行，然后照常按原 panic
退出。这里不能用 `try_lock()` 代替：`Mutex` 不可重入，同线程重入不是返回错误而是**挂住**，
`try_lock()` 虽然在同线程会返回 `WouldBlock`，但"拿不到锁"与"自己在里面"是两回事——
把两者混为一谈，会让正常的跨线程竞争也被误判成重入而丢记录。标记法只识别真正的重入。

和 `crash.signal` 一样，这一行只有 `ts=` epoch 秒、没有格式化时间戳、不分配内存——它本身
已经在"记录机制出问题"的路径上，不能再依赖任何记录机制。

`cause` 是被跳过的那条记录的事件名（`crash.panic`）。看到它就说明：**日志里缺了一条
panic 记录，但进程确实是 panic 死的**，栈要去 stderr 或者 `RUST_BACKTRACE=1` 重跑一次拿。

## panic 加固

原则只有一句：**输入是任意的，进程不能死。** 一个能连上 `~/.ssh/config` 文本编辑器、
能被 `SSHPF_LOG_LEVEL=$(cat /dev/urandom)` 喂到的程序，不能因为某个字节落在一个
多字节字符中间就退出。

这一轮把可达的 panic 逐条消掉了。下表每一条都是真实复现过的崩溃（括号里是修复前的退出码），
不是"潜在的隐患"：

| 场景 | 修复前 | 修复后 |
|---|---|---|
| `~/.ssh/config` 首行是中文注释（或任何非 ASCII，如 `#中文注释`、`#é`、`#😀`） | panic（101）：`end byte index 5 is not a char boundary` | 正常解析，exit 0；主机照常列出 |
| 首行是裸的多字节行（`中文注释`，没有 `#`） | 同上 panic（101） | 解析失败，exit 1 + `sshconfig.load_failed` |
| 无 TTY 启动（CI、`ssh host sshpf` 不带 `-t`、stdin 重定向） | panic（101）：`failed to initialize terminal`，裸栈打给用户 | exit 1 + `app.fatal stage=tui` + stderr 一行「错误: …」 |
| 非 UTF-8 命令行参数（`-c` 后面跟非 UTF-8 路径） | panic（101）：`env.rs` 里 `Result::unwrap()` | 路径按字节原样接受，exit 0；作参数名时是 usage error，exit 2 |
| `--help` / `--version` / `--list-hosts` 的 stdout 写不出去（`\| head -1`、`> /dev/full`） | panic（101）：`failed printing to stdout` | exit 0 + `cli.stdout_failed` |
| 终端宽度 ≤ 4 列时打开确认框 | panic（101）：`attempt to divide by zero` | 正常渲染，该块降级为不画或收窄 |
| `config.toml` 手写 `id = 18446744073709551615` 后新建转发 | panic（101）：`attempt to add with overflow` | 回落到空闲 id + `app.id_space_exhausted` |
| 超长 filter / 字段值（64K 字符） | panic（101）：`attempt to add with overflow` | 正常显示，超宽时 `ui.render_degraded reason=width_overflow` |
| 极宽终端下的导入面板 | panic（101）：`attempt to multiply with overflow` | 正常渲染 |
| panic hook 在日志锁被持有时重入 | **永久死锁**：进程僵住、日志 0 字节、终端卡在备用屏 | 降级写 `crash.log_reentrant`，进程照常死 |
| 隧道线程 panic | 该行永远停在 `Starting`，无人知道为什么 | `tunnel.thread_panic` + `Exited{code:101}`，UI 继续跑 |

这不是"加了 catch_unwind 兜住"。逐条说明两件事的边界：

- **能修的修根因。** 多字节切片改成按字节比较（`as_bytes().get(..5)`）；除零的除数改成
  `.max(1)`；u16 加法先扩宽到 u32 再 `try_from`；`print!` 改成显式句柄的 `write_all`。
- **只剩不变量可依赖的地方，降级而不是断言。** 渲染层拿不到该有的 layout 分区时，
  这一块这一帧不画、记 `ui.render_degraded`，下一帧照常；`ForwardKind::all()[i]` 越界时
  回落到默认类型并记 `form.kind_index_invalid`。降级路径**一律进日志**，否则就是
  "程序没崩，但也不知道发生了什么"，那比崩溃更难查。

**两条独立通路，日志系统自己是最后防线。** 上面这张表里有一行不是"某处 panic"而是
"崩溃处理机制本身会挂"：panic hook 会调 `record()`，而 `record()` 要拿日志锁——如果 panic
现场正好持有那把锁（例如某个 `Display` 实现里 panic），hook 在同一个线程上再 `lock()`
永远不会返回（`std::sync::Mutex` 不可重入，而且这里不是"中毒"而是**挂住**），进程就僵在
那里，连栈都写不出来。现在用一个线程局部的"已在临界区"标记识别这种情况：已经在里面就
跳过这条记录，改走降级通路（stderr 一行 + 预 `dup` 的 fd 裸 `write(2)` 写
`crash.log_reentrant`，带 `ts=` epoch 秒，和 `crash.signal` 同一风格，原因见上一节）。

**panicking 的线程不能带走进程。** 每条隧道跑在独立线程上，线程 panic 由
`catch_unwind` 兜住并转成 `Event::Exited{code:101}` + `tunnel.thread_panic`，
该行从 `Starting` 变成失败态，UI 和其余隧道不受影响。这一条和上一条合起来才是
"进程不会死"——只修输入不修崩溃处理机制，等于把最后一条出路也堵上。

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
| `cli.stdout_failed` | WARN | `mode` | 非 TUI 模式 stdout 写失败（管道关闭、磁盘满）；**退出码仍是该模式的真实结果**，这条只是说明输出没送达 |
| `app.signal_unavailable` | WARN | `signal` `msg` | 退出信号注册失败：该快捷键失效（Ctrl-C 不再走正常退出，隧道不会被 `Drop` 停掉），但 TUI 仍可用 `q` 退出 |
| `config.defaults_clamped` | WARN | `field` `original` `used` | 手写的 `retry_initial_secs` / `retry_max_secs` 越界或倒挂，已钳制到 [1s, 24h] |

### 防御与降级

这一组都不代表"出事了"，而是"本来会 panic 的地方被降级了"——程序继续跑，但它走的路
和你以为的不一样。看到它们时的第一反应是"为什么走到了这里"，而不是"赶紧修"。

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `ui.render_degraded` | WARN | `what` `reason`，并按 `reason` 附 `index`/`bands`/`need`/`row`/`top`/`x`/`y` | 渲染层某处边界条件被降级：该块这一帧不画、或光标不设，TUI 继续跑。`reason` 是稳定值（`layout_band_missing` / `width_overflow` / `row_overflow` 等）。**每进程每处只记一次**（限流），因为它每帧都可能触发 |
| `app.id_space_exhausted` | ERROR | `max_id` | `u64` id 空间耗尽（`config.toml` 里手写了 `id = u64::MAX`），拒绝新建而不是复用已占用的 id |
| `action.target_missing` | WARN | `action` | 确认框的目标隧道已不存在：删除被判过期、或按 id 操作时条目已被删。确认框仍会弹，但里面没有名字 |
| `form.kind_index_invalid` | WARN | `index` `len` | 表单的 `kind_idx` 越出 `ForwardKind::all()`，已回落默认类型（`-L`）。只有直接给 index 赋值才可能触发 |
| `import.click_stale` | DEBUG | `row` | 鼠标命中的导入行已不在当前筛选视图内（hit 图比点击早一帧），本次点击丢弃 |

### SSH 配置与主机密钥

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `sshconfig.loaded` | INFO | `hosts` | `~/.ssh/config` 读到的主机数 |
| `sshconfig.load_failed` | WARN | `msg` | 读 / 解析失败 |
| `sshconfig.include_capped` | WARN | `max` `msg` | `Include` 数量超过上限（256），剩余文件未读；说明导入结果可能不全 |
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
| `tunnel.retry_abandoned` | WARN | 隧道字段 + `attempt` `stage` `kind` `short` | **重试链放弃**：重试在 `stage`（`validate`/`preflight_bind`/`resolve_endpoint`/`spawn`）被拒且重试也修不好，退避到此为止 |
| `tunnel.thread_panic` | ERROR | `tunnel` `msg` | 隧道线程 panic 已被兜住并转为失败退出；栈另见同时写出的 `crash.panic` |
| `tunnel.stop_timeout` | WARN | `tunnel` `grace_ms` `msg` | 隧道线程未在宽限期内退出：UI 已把它判为停止，但线程可能仍在占端口 |
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
| `ui.render_degraded` | WARN | `what` `reason` + 各 `reason` 专属字段 | 渲染层边界条件被降级（详见「防御与降级」表）；每进程每处只记一次 |
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

## 排查配方

按"你看到的症状"查，不用先记住事件名。

### 「界面没崩，但某块该显示的东西不见了」

```bash
grep 'event=ui.render_degraded' ~/.local/state/sshpf/logs/sshpf.log
```

这条出现说明**渲染层主动跳过了一块**：`what` 说是哪块（`title_left`、`counters`、
`table_row`…），`reason` 说是哪条不变量没满足。它每帧都可能触发，所以每进程每处只记一次——
看到一行就代表"这一处能降级的路径被打中了"，不是只发生了一次。`reason` 的含义：

| `reason` | 含义 | 通常为什么 |
|---|---|---|
| `layout_band_missing` | `Layout::split` 少给了分区（`index`/`bands`） | ratatui 契约被破坏，或终端尺寸极端 |
| `width_overflow` | 某段文本的显示宽度超过 `u16`（`need`） | 过滤器/字段值超长（粘贴了一整行日志） |
| `row_overflow` | 表格行号 + 表头顶出 `u16`（`row`/`top`） | 终端极高或行数极多 |

块没画出来不影响功能（键盘、隧道、日志都在），但如果你觉得"缺东西"，答案在这。

### 「进程是崩了还是卡死了」

```bash
grep -E 'event=crash\.|event=tunnel\.thread_panic' ~/.local/state/sshpf/logs/sshpf.log
tail -5 ~/.local/state/sshpf/logs/sshpf.log
```

- 有 `crash.panic` → 真崩了，看 `thread=` 判断是 UI 线程还是某条隧道的线程；
- 有 `crash.log_reentrant` → 崩了，但那条 panic 记录因为重入被跳过（见上一节），
  栈要重跑一次拿；
- 有 `crash.signal` → 被信号杀（段错误 / abort），不是 Rust panic；
- **什么都没有、最后一条是正常记录** → 不是崩溃，是卡住或被 `SIGKILL`（`kill -9`
  来不及写任何东西）。这时看 `event=tunnel.stop_timeout` 和 `ui.slow_frame`。

### 「`tunnel.retry_abandoned` 和 `tunnel.failed` 有什么区别」

两个都代表"不会自己好了"，但断的位置不同：

| | `tunnel.failed` | `tunnel.retry_abandoned` |
|---|---|---|
| 断在 | 一次**会话**结束（连上了又断，或从没连上） | 一条**重试链**放弃（退避排了很多次，最后那次也修不好） |
| 还重试吗 | 看 `retryable` 字段 | 不重试了，到此为止 |
| 典型 `kind` | `connection_refused` `auth_failed` `timeout` | `port_in_use` `spawn_failed` `dns_failure` |
| 说明 | 这次连接失败了 | 为什么这一行不再自己忽上忽下 |

一句话：`tunnel.failed` 是"这次没成"，`tunnel.retry_abandoned` 是"别等了".
排查重连风暴时数 `tunnel.retry_scheduled`，看它最后落在 `failed` 还是 `retry_abandoned`。

### 「配置被谁改了」

```bash
grep -E 'event=(config\.repaired|config\.defaults_clamped|config\.perms_tightened)' \
  ~/.local/state/sshpf/logs/sshpf.log
```

- `config.repaired` → 载入时修了重复 id / 缺 `created_at`（`entries` 是条数）。
  你写的 id 和日志里的 id 不一致时先说这条。
- `config.defaults_clamped` → `retry_initial_secs` / `retry_max_secs` 被钳到 [1s, 24h]。
  `original` 是你写的值，`used` 是实际用的。**`retry_initial_secs = 0` 会变成每帧重连**，
  所以它被钳而不是被照做。
- `config.perms_tightened` → 文件原先对同组/所有人可读，已收紧到 `0600`。

### 「命令行没输出，退出码却是 0」

```bash
grep 'event=cli.stdout_failed' ~/.local/state/sshpf/logs/sshpf.log
```

stdout 写失败（`| head -1` 提前关管道、`> /dev/full`、磁盘满）不再 panic，也**不改变退出码**：
`--list-hosts` 确实列出来了，只是送不出去。`mode` 说是哪个模式。
如果你在脚本里依赖输出，检查的是管道而不是 sshpf。

### 「导入面板少了几条转发」

```bash
grep 'event=sshconfig.include_capped' ~/.local/state/sshpf/logs/sshpf.log
```

`Include` 超过 256 个时不再无限展开（防生成式配置把 UI 线程卡住），剩余文件不读，
导入结果会少。`max` 是上限值。此时把 `~/.ssh/config` 的 `Include` 收敛成通配符即可。

### 「确认框里没有名字 / 点了没反应」

```bash
grep -E 'event=(action\.target_missing|import\.click_stale)' ~/.local/state/sshpf/logs/sshpf.log
```

- `action.target_missing` → 确认框的目标隧道已经不存在（别处删掉了，或按过期 id 操作）。
  确认框仍会弹出（不会卡住你），但文案里没有名字，这一行解释了原因。
- `import.click_stale` → 鼠标点中的导入行已经不在当前筛选视图里（hit 图比点击早一帧），
  这次点击被丢弃。重新点一次即可。

## 隐私

写进日志的：主机名、端口、用户名、失败原因、路径、版本、pid、栈。
**从不写**：私钥内容、passphrase、`known_hosts` 内容、任何从 agent 读出的密钥材料。
日志文件 `0600`、目录 `0700`，与配置文件同级别。

主机名和用户名仍然是敏感信息，报 bug 前如果不方便公开，可以先
`sed -E 's/(host|name)=[^ ]*/\1=REDACTED/g'` 处理一遍。
