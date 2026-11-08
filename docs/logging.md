# 诊断日志 / Diagnostic log

> **这份文档回答**：日志写在哪、格式是什么、级别怎么定、崩溃怎么被记下来、出问题时怎么 grep。
> **不负责**：每个 `event=` 是什么（**完整事件目录在 [logging/events.md](logging/events.md)**）、安全政策（见 [security.md](security.md)）。
> **相关代码路径**：`src/logging/`（写入/轮转/崩溃）、`src/eventlog/`（事件词汇，`event=` 的唯一真相）。

## 为什么有第二份日志

TUI 的详情面板已经有一份日志（`model::LogRing`），但它活在内存里、进程一退就没了、且只覆盖单条隧道——这三个属性恰好都在**出问题之后**变成致命缺陷：崩溃时没有界面可读；要贴给别人必须是**文件**；"一小时前开始不对劲"需要跨隧道的时间线。

所以另开一条落盘通道。两者**不合并**：面板要"当下这条隧道发生了什么"，文件要"事后能不能还原现场"，容量、留存、格式要求不同。

## 位置与轮转

```
~/.local/state/sshpf/logs/sshpf.log        ← 当前文件
~/.local/state/sshpf/logs/sshpf.log.1      ← 上一次轮转（.1 最新）
~/.local/state/sshpf/logs/sshpf.log.2
~/.local/state/sshpf/logs/sshpf.log.3      ← 保留 3 个归档
```

| 属性 | 值 |
|---|---|
| 目录 / 文件权限 | `0700` / `0600`（日志里有主机名、端口、用户名） |
| 轮转阈值 | 单文件 2 MiB（`DEFAULT_MAX_BYTES`） |
| 归档数 | 3（`DEFAULT_KEEP`）⇒ 磁盘上限约 8 MiB |
| 写入方式 | **追加**，不覆盖历史；想清空直接删文件，下次运行重建 |
| 顺序 | **先轮转，再写**——所以一条记录**永远不会跨两个文件** |
| 归档重命名失败 | 一律忽略（丢历史好过丢当下） |
| `SSHPF_LOG_DIR` | 换目录 |
| `SSHPF_LOG_LEVEL` | `off\|error\|warn\|info\|debug`，默认 `info` |

`off` 只关常规记录，**崩溃记录照写**——想把日志关掉的人，往往正是最需要那一行崩溃信息的人。

**日志永远不许把程序搞挂**：打不开文件 ⇒ logger 降级为 disabled，所有写入变空操作；写入失败 ⇒ 吞掉（没有地方上报，重试只会死循环）。`init` 的失败原因留在 `init_error()` 里，TUI 起来后写进状态栏（"日志未启用: …"）并记 `log.unavailable`。**静默失败是可观测性功能最糟的失败模式**：用户以为有日志，出事时打开只有空文件。

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
| `seq` | 本次运行内递增；**轮转后从 1 重新计数**，跨文件比对请用时间戳 |
| `event` | 稳定事件名（[目录](logging/events.md)）。诊断从 `grep event=<名字>` 开始 |
| 隧道身份 | `tunnel` `name` `forward` `listen` `target` `host`，固定 6 个且在最前 ⇒ `grep 'tunnel=3 '` 就是完整时间线 |
| 其它字段 | `key=value` |

**不用 JSON**：这个文件的读者是"拿着终端排障的人"和 `grep`/`awk`，不是日志聚合器。JSON 每行重复键名，肉眼扫起来噪声占一半；`key=value` 用 `grep 'tunnel=3 '` 就能切出时间线。真需要机器消费时，这个格式解析也只比 JSON 多一行 `split`。

**值必须转义**：含空白/引号/反斜杠/控制字符的值一律加引号并转义（`\n` `\r` `\t` `\\` `\"` `\xNN`），键名与事件名把这类字符替换成 `_`。记录里塞进裸换行会把一条日志劈成两条，后面所有按行处理全部错位。有测试钉住"一条记录不跨行"。

## 级别策略

按**读的人要做什么**划分，不是按严重程度：

| 级别 | 用来记什么 | 例子 |
|---|---|---|
| `ERROR` | 用户要做的事没做成；隧道已死且不会自愈 | `tunnel.rejected`、`tunnel.failed`、`config.save_failed` |
| `WARN` | 降级但自愈：安排了重连、学到新主机密钥、配置被修正、权限被收紧、一帧超时 | `tunnel.retry_scheduled`、`knownhosts.learned`、`ui.slow_frame` |
| `INFO` | 预期内生命周期，足以在不看 UI 的情况下重建一次会话 | `app.start`、`tunnel.ready`、`action.applied` |
| `DEBUG` | 高频细节：每条转发行、每次 SOCKS5 失败 | `tunnel.line`、`socks5.rejected` |

关键判断：**掉线（`was_up=true`）是 WARN，从未连上的失败是 ERROR。** 前者网络抖了一下、系统会自己重连；后者要用户动手。同理 `tunnel.failed` 带 `retryable` 字段——读者不用去猜"这个错等一下会不会自己好"。

**高频路径不写**：`tunnel.line` / `socks5.rejected` / `forward.target_failed`（线程侧）只在 `DEBUG` 写，且**先查级别再拼字符串**（`logger.enabled(Level::Debug)` 短路），避免为了丢弃而格式化。

## 崩溃记录 / Crash records

两条独立通路。机制与 `unsafe` 边界见 [security.md](security.md#崩溃捕获--crash-capture)。

### panic

```
… ERROR seq=42 event=crash.panic thread=sshpf-tunnel-3 location=src/app.rs:112:9 msg="index out of bounds" backtrace=yes
… ERROR seq=43 event=crash.panic.trace msg="  | panic backtrace\n  |    0: sshpf::tunnel::run\n  | …"
```

- `thread` 是关键：隧道各自跑在自己的线程上（`sshpf-tunnel-<id>`），线程名能区分"UI 线程挂了"和"某条隧道的线程挂了"——后者在状态列上看起来只是"没连上"。
- 栈最多 25 帧，且走**单独一条记录**、每行 `  | ` 前缀（保持"一条记录一行"，让 `grep event=` 仍然有效）。

### 致命信号（`SIGSEGV` / `SIGBUS` / `SIGABRT` / `SIGILL` / `SIGFPE`）

```
ts=1794000000 ERROR seq=0 event=crash.signal signal=11 name=SIGSEGV pid=4242
```

这一类**没有格式化时间戳**，只有 `ts=<epoch 秒>`：信号处理器里只能用 async-signal-safe 调用，`localtime_r` 首次调用会分配内存。其余字段仍是 `key=value`，解析器不用开特例。

### 日志重入

```
ts=1794000000 ERROR seq=0 event=crash.log_reentrant cause=crash.panic msg="日志写入重入，记录被跳过以避免死锁"
```

看到它说明：**日志里缺了一条 panic 记录，但进程确实是 panic 死的**，栈要去 stderr 或 `RUST_BACKTRACE=1` 重跑一次拿。

## 怎么用

```bash
L=~/.local/state/sshpf/logs/sshpf.log

grep 'tunnel=3 ' "$L"                          # 一条隧道的完整时间线
grep -E ' ERROR | event=crash\.' "$L"          # 只看真正出事的地方
grep 'event=tunnel.retry_scheduled' "$L" | grep -c 'tunnel=3 '   # 某条隧道排了几次重连
grep -oP 'event=tunnel\.failed .*?kind=\K[a-z_]+' "$L" | sort | uniq -c  # 哪类失败最多
grep -n 'event=crash' "$L"                     # 定位崩溃
sed -n '/event=crash.panic.trace/p' "$L" | sed 's/\\n/\n/g'      # 把栈拉出来
tar czf sshpf-logs.tgz ~/.local/state/sshpf/logs/                # 报 bug 时附上
```

**提 issue 时请附上整个 `logs/` 目录**（含 `.1`~`.3`）。单独贴最后几行通常不够——重连风暴、配置被改、主机密钥被学到的证据都在更早的位置。

## 排查配方 / Recipes

按"你看到的症状"查，不用先记住事件名。完整事件字段见 [logging/events.md](logging/events.md)。

### 「界面没崩，但某块该显示的东西不见了」

```bash
grep 'event=ui.render_degraded' "$L"
```

这是渲染层**主动跳过了一块**：`what` 说哪块（`title_left`、`counters_filter`、`table_row`…），`reason` 说不变量哪里没满足。每帧都可能触发，所以**每进程每处只记一次**——看到一行就代表"这一处能降级的路径被打中了"，不是只发生了一次。

| `reason` | 含义 | 通常为什么 |
|---|---|---|
| `layout_band_missing` | `Layout::split` 少给了分区（`index`/`bands`） | ratatui 契约被破坏，或终端尺寸极端 |
| `width_overflow` | 某段文本显示宽度超过 `u16`（`need`） | 过滤值/字段值超长（粘贴了一整行日志） |
| `row_overflow` | 表格行号 + 表头顶出 `u16`（`row`/`top`） | 终端极高或行数极多 |

块没画出来不影响功能（键盘、隧道、日志都在），但"缺东西"的答案在这。

### 「进程是崩了还是卡死了」

```bash
grep -E 'event=crash\.|event=tunnel\.thread_panic' "$L"; tail -5 "$L"
```

| 看到 | 结论 |
|---|---|
| `crash.panic` | 真崩了，看 `thread=` 判断是 UI 线程还是某条隧道的线程 |
| `crash.log_reentrant` | 崩了，但那条 panic 记录因重入被跳过；栈要重跑一次拿 |
| `crash.signal` | 被信号杀（段错误 / abort），不是 Rust panic |
| **什么都没有、最后一条是正常记录** | 不是崩溃，是卡住或被 `SIGKILL`（`kill -9` 来不及写任何东西）。看 `tunnel.stop_timeout` / `ui.slow_frame` |

### 「`tunnel.retry_abandoned` 和 `tunnel.failed` 有什么区别」

| | `tunnel.failed` | `tunnel.retry_abandoned` |
|---|---|---|
| 断在 | 一次**会话**结束 | 一条**重试链**放弃（退避排了很多次，最后那次也修不好） |
| 还重试吗 | 看 `retryable` 字段 | 不重试了，到此为止 |
| 典型 `kind` | `connection_refused` `auth_failed` `timeout` | `port_in_use` `spawn_failed` `dns_failure` |
| 说明 | 这次连接失败了 | 为什么这一行不再自己忽上忽下 |

一句话：`tunnel.failed` 是"这次没成"，`tunnel.retry_abandoned` 是"别等了"。排查重连风暴时数 `tunnel.retry_scheduled`，看它最后落在哪个。

### 「配置被谁改了」

```bash
grep -E 'event=(config\.repaired|config\.defaults_clamped|config\.perms_tightened)' "$L"
```

- `config.repaired` → 载入时修了重复 id / 缺 `created_at`（`entries` 是条数）。你写的 id 与日志里的 id 不一致时先说这条。
- `config.defaults_clamped` → `retry_initial_secs` / `retry_max_secs` 被钳到 [1s, 24h]。`original` 是你写的值，`used` 是实际用的。**`retry_initial_secs = 0` 会变成每帧重连**，所以它被钳而不是被照做。
- `config.perms_tightened` → 文件原先对同组/所有人可读，已收紧到 `0600`（`ok=false` 说明收紧失败）。

### 「命令行没输出，退出码却是 0」

```bash
grep 'event=cli.stdout_failed' "$L"
```

stdout 写失败（`| head -1` 提前关管道、`> /dev/full`、磁盘满）不 panic，也**不改变退出码**：`--list-hosts` 确实列出来了，只是送不出去。`mode` 说是哪个模式。如果你在脚本里依赖输出，检查的是管道而不是 sshpf。

### 「导入面板少了几条转发」

```bash
grep -E 'event=(sshconfig\.include_capped|sshconfig\.load_failed)' "$L"
```

`Include` 的待读文件超过 256 个时不再继续展开（防生成式配置卡住 UI 线程），剩余文件不读，导入结果会少。`max` 是上限。收窄 `~/.ssh/config` 的 `Include` 通配即可。

### 「确认框里没有名字 / 点了没反应」

```bash
grep -E 'event=(action\.target_missing|import\.click_stale)' "$L"
```

- `action.target_missing` → 确认框的目标隧道已不存在（别处删掉了，或按过期 id 操作）。确认框仍会弹（不卡住你），但文案里没有名字，这一行解释了原因。
- `import.click_stale` → 鼠标点中的导入行已不在当前筛选视图里（hit 图比点击早一帧），这次点击被丢弃。重点一次即可。

## 隐私

写：主机名、端口、用户名、失败原因、路径、版本、pid、栈。
**从不写**：私钥内容、passphrase、`known_hosts` 内容、任何从 agent 读出的密钥材料。

主机名和用户名仍是敏感信息，报 bug 前如果不方便公开，可以先
`sed -E 's/(host|name)=[^ ]*/\1=REDACTED/g'` 处理一遍。权限与脱敏的完整边界见 [security.md](security.md#日志脱敏--redaction)。
