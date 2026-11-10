# 事件目录 / Event catalogue

> **这份文档回答**：每个 `event=` 值是什么级别、带哪些字段、什么含义、什么时候该看它。这是日志的**唯一事件清单**。
> **不负责**：日志格式/轮转/排查配方（见 [../logging.md](../logging.md)）、崩溃机制（见 [../security.md](../security.md)）。
> **相关代码路径**：`src/eventlog/`（`event=` 值的唯一真相：`process.rs` / `server.rs` / `tunnel.rs` / `ssh.rs` / `user.rs`）、`src/logging/`（含 `crash.*`）。

## 约定 / Conventions

- `event=` 是**稳定 ASCII 契约**：函数名、字符串、级别、字段名与顺序逐字不变。改它们等于改日志契约。
- 事件家（family）= 源码里的文件：`app.*`/`config.*`/`state.*`/`log.*`/`cli.*` → `process.rs`；`server.*`/`client.*` → `server.rs`；`tunnel.*`/`forward.*`/`socks5.*` → `tunnel.rs`；`sshconfig.*`/`knownhosts.*` → `ssh.rs`；`action.*`/`import.*`/`portscan.*`/`ui.*`/`form.*` → `user.rs`；`crash.*` → `logging/`。
- **隧道身份字段**：所有隧道级记录的前 6 个字段固定是 `tunnel` `name` `forward` `listen` `target` `host`，所以 `grep 'tunnel=3 '` 就是一条隧道的完整时间线。下方"隧道字段"= 这 6 个字段，不再逐行重复列出。

## 进程与配置 / Process & config

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `app.start` | INFO | `version` `pid` `config` `log` `os` `arch` | 进程启动 |
| `app.cli_mode` | DEBUG | `mode` | 非 TUI 模式（`check` / `list_hosts`） |
| `app.fatal` | ERROR | `stage` `msg` | TUI 起不来，进程退出（此时已无界面可解释） |
| `app.quit` | INFO | `reason` | 用户确认退出 |
| `app.exit` | INFO | `reason` `uptime_ms` `tunnels_up` | 正常退出 |
| `app.signal` | INFO | `signal` | `SIGINT`/`SIGTERM`/`SIGHUP` 触发退出 |
| `app.signal_unavailable` | WARN | `signal` `msg` | 退出信号注册失败：该快捷键失效（`ctrl+c` 不再走正常退出），但 `q` 仍可用 |
| `app.restore` | INFO | `tunnels` | 开始恢复上次启用的条目 |
| `app.restore_skipped` | INFO | `reason` | `--no-restore` |
| `app.shutdown` | INFO | `tunnels_active` `running` | 退出前停掉所有隧道 |
| `app.orphan_event` | DEBUG | `event` `tunnel` | 收到已删除隧道的事件（删除与线程退出之间的正常竞态） |
| `app.id_space_exhausted` | ERROR | `max_id` | `u64` id 空间耗尽（手写了 `id = u64::MAX`），拒绝新建而不是复用已占用的 id |
| `config.loaded` | INFO | `path` `tunnels` `problems` | 配置读入成功 |
| `config.invalid` | WARN | `path` `msg` | 每条校验问题的明细 |
| `config.load_failed` | ERROR | `path` `msg` `fallback` | 配置读不进来，以空配置启动 |
| `config.repaired` | WARN | `path` `entries` | 载入时修正了重复 id / 缺失时间戳 |
| `config.deduped` | WARN | `kept` `dropped` | 载入时把「同一个转发」的多条已保存条目折成一条；`kept` 是留下的条目名，`dropped` 被移除的条数。每组一条（见 [../dedup.md](../dedup.md)） |
| `config.save_failed` | ERROR | `path` `msg` | 写盘失败：改动只在内存里 |
| `config.perms_tightened` | WARN / ERROR | `path` `ok` | 配置原先对同组/所有人可读，已收紧到 `0600`（`ok=false` 即收紧失败，记 ERROR） |
| `config.defaults_clamped` | WARN | `field` `original` `used` | 手写的 `retry_initial_secs` / `retry_max_secs` 越界或倒挂，已钳制到 [1s, 24h]；`health_check_secs` 越界也会记这一条（`0` = 关闭，保持不变） |
| `state.save_failed` | WARN | `path` `msg` | 运行状态文件写失败 |
| `log.unavailable` | ERROR | `msg` | 日志打不开（同时进状态栏） |
| `log.level_invalid` | WARN | `msg` | `SSHPF_LOG_LEVEL` 写错，回退到 `info` |
| `cli.bad_args` | ERROR | `msg` | 参数错误，进程以退出码 2 退出 |
| `cli.stdout_failed` | WARN | `mode` | 非 TUI 模式 stdout 写失败（管道关闭、磁盘满）；**退出码仍是该模式的真实结果**，这条只是说明输出没送达 |

## client-server 运行时 / client-server runtime

> 两个进程替代了一个，所以「为什么和我想的不一样」多了一个候选答案：*是哪个进程、它们见上面了吗*。这一组事件就是为了只靠日志能回答这个问题。运行模型见 [../client-server.md](../client-server.md)。

| `event=` | 级别 | 字段 | 含义 |
|---|---|---|---|
| `server.start` | INFO | `pid` `config` `socket` `keep_server` | 服务端拿到选举锁并在监听 |
| `server.stopped` | INFO | `socket` | 主循环已退出并清理完毕 |
| `server.election_lost` | DEBUG | `lock` | 另一进程已在服务这份配置：**正常选举结果**，本进程退出码 3 |
| `server.stale_socket_removed` | INFO | `socket` | 认出并清掉 `SIGKILL` 残留的 socket（抢到锁之后才做） |
| `server.client_connected` | INFO | `count` | 某 client 完成握手；`count` 含它 |
| `server.client_disconnected` | INFO | `count` `reason` | 某 client 离开；`reason=bye`（主动告别）/`eof`（崩溃、`kill -9`、终端被杀） |
| `server.client_dropped` | WARN | `session` `reason` | 某 client 收帧队列满（读不动），断开它以免拖住其他人；`reason=queue_full` |
| `server.grace_started` | INFO | `grace_ms` `trigger` | 计数归零，开始宽限倒计时；`trigger=last_exit`(500ms)/`eof`(5000ms) |
| `server.grace_cancelled` | INFO | `count` | 宽限期内新 client 连入，关服取消 |
| `server.shutdown_begin` | INFO | `count` `active` `trigger` | 开始关服，`active` 是即将停掉的隧道数 |
| `server.lock_failed` / `server.lock_open_failed` | ERROR | `msg` | 选举锁的 `flock` / `open` 真出错（不是「被别人持有」，那是 DEBUG 的 `server.election_lost`） |
| `server.bind_failed` | ERROR | `msg` | 拿不到锁却仍无法 bind：socket 目录不可写、路径被占等 |
| `server.socket_chmod_failed` | WARN | `msg` | socket 建成后没能收紧到 `0600`：同组用户可能连上它 |
| `server.socket_unlink_failed` | WARN | `msg` | 关服时 socket 没删掉，下次启动会走残留恢复 |
| `server.signal_unavailable` | WARN | `signal` | 服务端某个退出信号注册失败 |
| `client.attached` | INFO | `server_pid` `config` | 本进程连上了服务端（而不是自己拥有隧道） |
| `client.attach` | INFO | `config` | 本进程进入 client 模式 |
| `client.server_spawned` | INFO | `server_pid` | 本进程拉起了它随后连上的服务端 |
| `client.disconnected` | WARN | `reason` | 与服务端的连接断了：界面不再更新，必须让用户知道 |
| `client.attach_failed` | ERROR | `msg` | 连不上也拉不起服务端：**不静默降级成单进程**，以非 0 退出 |

## SSH 配置与主机密钥 / SSH config & host keys

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `sshconfig.loaded` | INFO | `hosts` | `~/.ssh/config` 读到的主机数 |
| `sshconfig.load_failed` | WARN | `msg` | 读 / 解析失败 |
| `sshconfig.include_capped` | WARN | `max` `msg` | `Include` 的待读文件超过上限（256），剩余文件未读；说明导入结果可能不全 |
| `sshconfig.resolve_failed` | ERROR | 隧道字段 + `msg` | 某个 `ssh_host` 解析不出 endpoint |
| `knownhosts.learned` | WARN | `tunnel` `host` `port` `verdict` | `accept-new` 下学到新主机密钥并写入 `known_hosts`。**只在真的写盘时才记**：`ignore` 下未知主机虽被接受但不持久化，因此不产生此事件 |
| `knownhosts.rejected` | ERROR | `tunnel` `host` `port` `policy` `msg` | 密钥变了 / 被吊销 / `strict` 下未知 |
| `knownhosts.ignored_changed` | WARN | `tunnel` `host` `port` `line` `fingerprint` | `ignore` 策略下接受了已改变的主机密钥；`known_hosts` 未被改写，`fingerprint` 是服务器实际呈现的 SHA256 指纹 |

## 隧道生命周期 / Tunnel lifecycle

`kind` 字段是 ASCII 稳定值，旁边配一个 `short` 放界面上的中文短句——前者给 `grep`，后者给人读。**`kind()` 值是日志契约，不许改。**

| `kind` 稳定值 | 界面短句 | 可重试 |
|---|---|---|
| `port_in_use` | 本地端口被占用 | ✗ |
| `permission_denied` | 端口权限不足(<1024) | ✗ |
| `auth_failed` | 认证失败(公钥/agent) | ✗ |
| `dns_failure` | 域名解析失败 | ✓ |
| `connection_refused` | 连接被拒绝 | ✓ |
| `timeout` | 连接超时 | ✓ |
| `host_key_mismatch` | 主机密钥校验失败 | ✗ |
| `remote_forward_failed` | 远端转发失败 | ✗ |
| `target_unreachable` | 目标不可达 | ✓ |
| `stale_connection` | 连接已失效 | ✓ |
| `spawn_failed` | 无法启动 ssh | ✗ |
| `exited` | ssh 异常退出 | ✓ |
| `unsupported` | 该类型暂未支持 | ✗ |
| `unknown` | 未知错误 | ✓ |

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `tunnel.start` | INFO | 隧道字段 + `trigger` | 请求启动（`manual`/`restore`/`retry`/`batch`/`created`/`edited`） |
| `tunnel.rejected` | ERROR | 隧道字段 + `stage` `kind` `short` `msg` | 启动被拒；`stage` = `validate`/`preflight_bind`/`resolve_endpoint` |
| `tunnel.spawn` | INFO | 隧道字段 + `endpoint` `via` `hostkey_policy` | 后台线程开始建连；`via` 是 ProxyJump 链（逗号分隔） |
| `tunnel.spawn_failed` | ERROR | 隧道字段 + `msg` | 线程都起不来 |
| `tunnel.ready` | INFO | 隧道字段 + `elapsed_ms` | 转发真正就绪（`bind` 成功 / `tcpip_forward` 被接受） |
| `tunnel.line` | DEBUG | 隧道字段 + `msg` | 驱动的一条进度行（只在这条隧道活着时有意义） |
| `tunnel.health_failed` | WARN | 隧道字段 + `cause` `detail` `interval_ms` | 探活判定 SSH 会话已失效，本次运行随即以 `stale_connection` 结束。`cause` 是稳定 ASCII 值：`ping_timeout` / `ping_error` / `session_closed`。**探活成功不记日志**，所以没有这条事件不代表探活没跑 |
| `tunnel.exit` | WARN / ERROR / INFO | 隧道字段 + `was_up` `code` `kind` `short` `msg`；干净退出是 `clean=true` | 会话结束；`was_up=true` 是掉线（WARN），否则 ERROR；用户主动停是 INFO + `clean=true` |
| `tunnel.retry_scheduled` | WARN | 隧道字段 + `attempt` `delay_ms` `kind` `short` | 安排了退避重连 |
| `tunnel.retry` | INFO | 隧道字段 + `attempt` | 重连到点，开始重试 |
| `tunnel.failed` | ERROR | 隧道字段 + `kind` `short` `retryable` `msg` | 终态失败；`retryable` 说明等待是否有用 |
| `tunnel.retry_abandoned` | WARN | 隧道字段 + `attempt` `stage` `kind` `short` | **重试链放弃**：在 `stage`（`validate`/`preflight_bind`/`resolve_endpoint`/`spawn`）被拒且重试也修不好，退避到此为止 |
| `tunnel.thread_panic` | ERROR | `tunnel` `msg` | 隧道线程 panic 已被兜住并转为失败退出；栈另见同时写出的 `crash.panic` |
| `tunnel.stop_timeout` | WARN | `tunnel` `grace_ms` `msg` | 隧道线程未在宽限期（2s）内退出：UI 已判停止，但线程可能仍占着端口 |
| `tunnel.stop` | INFO | 隧道字段 + `trigger` | 用户停止（`manual`/`stop_all`/`batch`） |
| `tunnel.created` | INFO | 隧道字段 + `source` | 新定义（`form`/`import`） |
| `tunnel.updated` | INFO | 隧道字段 | 定义被编辑 |
| `tunnel.deleted` | INFO | 隧道字段 | 定义被删除 |
| `forward.target_failed` | WARN | `tunnel` `target` `msg` | 隧道通但目标端口连不上（隧道本身不挂） |
| `socks5.rejected` | DEBUG | `tunnel` `msg` | SOCKS5 握手/请求失败 |

> 注：v0.2.1 的文档列过一个 `tunnel.state`（状态机每一步），**代码里从未 emit 过它**。状态变化由 `tunnel.exit` / `tunnel.ready` / `tunnel.retry_scheduled` / `tunnel.failed` 表达，本表不再列不存在的事件。

## 用户动作 / User actions

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `action.applied` | INFO | `action` `detail` | 确认闸放行 |
| `action.cancelled` | INFO | `action` | 在确认框上取消（"为什么它还在跑"的答案） |
| `action.batch` | INFO | `action` `marked` `changed` | 批量操作完成；`changed` 是**真正生效**的条数，不是标记数 |
| `action.requested` | DEBUG | `action` `detail` | 弹出了确认框 |
| `action.target_missing` | WARN | `action` | 确认框的目标隧道已不存在：删除被判过期、或按 id 操作时条目已被删。确认框仍会弹，但里面没有名字 |
| `action.duplicate_skipped` | INFO | `source` `name` `existing` | 一次创建被跳过：候选转发与 `existing` 是同一个转发（见 [../dedup.md](../dedup.md)）。`source` 是 `form` / `import` / `port-scan`。INFO 而非 WARN：跳过冗余正是预期结果，但这行解释了「勾了 3 条怎么只建了 2 条」 |
| `import.committed` | INFO | `host` `created` | 从 `~/.ssh/config` 导入成功 |
| `import.rejected` | WARN | `problems` `msg` | 导入校验失败，一条都没建 |
| `import.click_stale` | DEBUG | `row` | 鼠标命中的导入行已不在当前筛选视图内（hit 图比点击早一帧），本次点击丢弃 |
| `form.save_attempted` | INFO | `route` `pane_open` `rows` `checked` `touched` `complete` | `ctrl+s`/`f10` **到达了表单**并进入某条保存分支。`route` 是稳定 ASCII 值：`import` / `port-batch` / `typed` / `edit` / `refused`。这是「我的保存到底做了什么」唯一的正面证据——**没有这一行就说明保存键根本没到表单** |
| `form.save_key_swallowed` | WARN | `focus` | 保存键被某个子焦点吃掉，表单没收到。`focus` 是 `host_dropdown` / `import_filter`。**这两条是一对**：有 `save_attempted` 无隧道 ⇒ 保存被拒（看 `route`）；两条都没有 ⇒ 键没到表单；只有 `save_key_swallowed` ⇒ 就是它吃掉的那一次 |
| `portscan.started` | INFO | `host` `multi` | 请求扫描对端监听端口（`multi` = 勾选可批量建隧道）。它**另外开一条 SSH 连接**，所以给对端的认证日志一个本地解释 |
| `portscan.finished` | INFO | `host` `found` `excluded` | 扫描成功；`found` 是可选的端口数，`excluded` 是因 `~/.ssh/config` 已列出而隐藏的个数 |
| `portscan.failed` | WARN | `host` `msg` | 扫描失败（连不上、没有 `ss`/`netstat`、输出无法解析），`msg` 是面板上显示的那句话 |
| `portscan.closed` | DEBUG | — | 端口列表被收起且没有采纳任何一项 |
| `portscan.stale` | DEBUG | `seq` `current` | 结果属于已被替换的代次：用户换主机/重扫时旧探测才返回，答案丢弃而不是套到新列表上 |
| `portscan.dropped` | DEBUG | `from` `to` | 主机或转发类型变了，旧列表被丢弃（`to` 是当前字段里的主机） |
| `portscan.batch_rejected` | WARN | `problems` `msg` | 勾选端口的批量校验失败，一条都没建 |

## 界面降级 / UI degradation

这一组都不代表"出事了"，而是"本来会 panic 的地方被降级了"——程序继续跑，但它走的路和你以为的不一样。看到它们的第一反应是"为什么走到了这里"，而不是"赶紧修"。

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `ui.render_degraded` | WARN | `what` `reason`，并按 `reason` 附 `index`/`bands`/`need`/`row`/`top`/`x`/`y` | 渲染层某处边界条件被降级：该块这一帧不画、或光标不设，TUI 继续跑。`reason` 是稳定值（`layout_band_missing` / `width_overflow` / `row_overflow`）。**每进程每处只记一次**（限流），因为它每帧都可能触发 |
| `ui.slow_frame` | WARN | `elapsed_ms` | 一帧超过 500 ms（tick 的 2.5 倍）——"TUI 感觉卡住了"的第一手证据 |
| `ui.no_mouse` | DEBUG | — | 终端不支持鼠标捕获，键盘仍可用 |
| `form.kind_index_invalid` | WARN | `index` `len` | 表单的 `kind_idx` 越出 `ForwardKind::all()`，已回落默认类型（`-L`）。只有直接给 index 赋值才可能触发 |

## 崩溃 / Crashes

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `crash.panic` | ERROR | `thread` `location` `msg` `backtrace` | Rust panic。`thread` 区分"UI 线程挂了"与"某条隧道的线程挂了" |
| `crash.panic.trace` | ERROR | `msg` | 栈块，单独一条记录，每行 `  \| ` 前缀，最多 25 帧 |
| `crash.signal` | ERROR | `signal` `name` `pid`，时间戳是 `ts=<epoch 秒>` | 致命信号（SIGSEGV/SIGBUS/SIGABRT/SIGILL/SIGFPE），无格式化时间戳（handler 内禁止分配） |
| `crash.log_reentrant` | ERROR | `cause` `msg`，时间戳是 `ts=<epoch 秒>` | panic 发生在日志临界区内：跳过常规记录、不拿锁，避免自死锁。看到它说明**缺了一条 panic 记录但进程确实 panic 死了** |

## 加一个事件 / Add an event

1. 在对应族的 `src/eventlog/<族>.rs` 加一个函数（不要在内联调用点直接拼 `event=`）。
2. 本页加一行（事件名 | 级别 | 关键字段 | 含义）。
3. 若它有排查价值，[../logging.md](../logging.md#排查配方--recipes) 加一条配方。
4. 级别按"读的人要做什么"定，见 [../logging.md](../logging.md#级别策略)。
