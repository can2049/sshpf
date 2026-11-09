# 协议 / Wire protocol

> **这份文档回答**：client 与 server 之间走什么传输、帧怎么切、有哪些消息、字段是什么、
> 版本怎么协商、慢 client 怎么隔离、`Instant` 这类不可跨进程的值怎么表达。
> **不负责**：进程怎么起停与选举（见 [lifecycle.md](lifecycle.md)）、迁移步骤（见 [migration.md](migration.md)）、
> 方案取舍的背景（见 [../client-server.md](../client-server.md)）。
> **相关代码路径**：本 PR 不含实现；将来落在 `src/ipc/`。可参照的既有转义纪律在 `src/logging/format.rs`，
> wire 化的源类型在 `src/model/tunnel.rs:57`、`src/model/status.rs:21-32`、`src/model/failure.rs:83`。
>
> 文中**实测**数字由本机探针得到（Rust 1.99.1、`toml 1.2.x`、Linux）；**未实测**项集中在 §11，不假装测过。

## 0. 一页结论

| 结论 | 一句话理由 |
|---|---|
| Unix socket，路径由**规范化 config 路径的 FNV-1a-64** 派生 | server 身份 = 「哪份配置」，否则同机两套配置共用一个 server 互相污染 |
| 帧 = `u32` 大端长度前缀 + TOML，tagged enum **顶层直接**编码 | 实测可序列化，不必包 `[op]`；TOML 是已有依赖，零新增 |
| **全量快照 + revision**，非 delta；快照只带**日志尾巴 15 行** | 50 隧道（15 行尾巴）实测 96 KB、编+解 1.9 ms；带整个 ring 则 5.2 MB，帧上限都过不去 |
| 命令 `Command{req_id, expect_rev, op}` + `Reply`；**权威校验一律在 server 重做** | client 快照可能过期，过期的视图不得决定现在的行为 |
| 慢消费者 = 每 client 有界队列，跟太慢则**断开** | 实测对端不读时 233152 B 后写阻塞；广播不得为任何 client 停住 |
| 最后一个 client 退出：`Bye{last_exit}` 只是**提议**，server 必须重新计数 | 过期的视图不得拥有破坏权（§9） |

## 1. 传输层 / Transport

### 1.1 `socket_path` 推导

```text
config_path = 规范化后的 Paths.config（§1.2）
server_id   = FNV-1a-64(config_path) → 16 位小写 hex
socket_dir  = dirs::runtime_dir().unwrap_or(temp_dir).join("sshpf")   // 本机实测 /run/user/1000
socket_path = socket_dir.join("{server_id}.sock")     // 0600
lock_path   = socket_dir.join("{server_id}.lock")     // 选举用，见 lifecycle
```

| 决策 | 理由 | 代价 |
|---|---|---|
| 目录用 `dirs::runtime_dir()` | 实测 0700，且**重启即清空**，残留 socket 不跨开机 | 无 systemd 的会话可能缺失 → 回退 `temp_dir()` 下自建 0700 子目录 |
| 用 FNV-1a，不用 `DefaultHasher` | std 源码原文（`hash/random.rs:90`）：*"The internal algorithm is not specified, and so it and its hashes should not be relied upon over releases."* 升级工具链会让新旧 client 算出不同路径 → 各拉一个 server | 自写 12 行常量；用途是**命名**不是防碰撞 |
| 碰撞兜底：`Welcome.config` 回传并比对 | 碰撞后果是「别人的隧道出现在你表里」，不可接受；一次字符串比较归零 | 一个字段 |
| 目录 0700、socket **显式 chmod** 0600 | `peer_cred()` 不可用，文件系统权限是唯一内核级边界。**实测 bind 后默认模式是 0o775（受 umask）**，不 chmod 则同组可连 | 同 uid 任何进程仍可连（§9 风险） |

### 1.2 「规范化」用词法消解，不用 `canonicalize`（实测）

实测：`canonicalize` 对**尚未创建**的路径返回 `Err(NotFound)`，而首次运行时配置文件根本不存在，
且 `socket_path` 必须在读文件**之前**算出 → 只能用 `std::path::absolute` + 词法消解 `.`/`..`
（实测 `absolute` 已消解 `.`，但**保留** `..`，须自行折叠）。
代价：不解析符号链接，`cfg.toml` 与 `link-to-cfg.toml` 是两个 server。可接受（用户显式给了不同路径），
但 `--server` 启动日志必须打 `config_path`，让「为什么我开了两个」可诊断。

### 1.3 为什么不用 abstract socket

| 维度 | 文件系统 socket（选用） | Linux abstract |
|---|---|---|
| 权限控制 | 目录 0700 + 文件 0600，内核强制 | **无**：无文件可 chmod，任何进程可连 |
| 残留 | 留 socket 文件，靠 ECONNREFUSED + 抢锁识别 | 自动回收，零残留 |
| 可移植性 | Unix 通用（macOS 亦可） | **仅 Linux**（实测可用） |

决定性理由：`peer_cred()` unstable 之后，abstract socket **等于完全没有访问控制**。
残留问题由 lifecycle 的「ECONNREFUSED + 抢到锁 = 残留」覆盖。（Windows 见 §11。）

## 2. 帧格式 / Framing

```text
+---------------------+----------------------------------+
| len: u32 大端        | payload: TOML (UTF-8), len 字节   |
+---------------------+----------------------------------+
```

| 规则 | 值 / 做法 | 理由 |
|---|---|---|
| 帧上限 | `max_frame_bytes = 256 * 1024`，超限断连并记 `ipc.frame_too_large` | 有上限才能拒「声明 4 GiB」的坏长度，避免按声明分配 |
| 解码失败必须不 panic | `toml::from_str` 对空串/坏字节/类型错误实测全返 `Err`（多组坏输入无一 panic） | 生产代码禁 panic；对端是不受信输入 |
| 读超时 | 双方 `set_read_timeout(2000ms)` | 实测静默对端 **86.3 ms** 返回 `WouldBlock`；无超时则半开连接永久占线程 |

### 2.1 前向兼容（实测）

| 情形 | 实测 | 协议含义 |
|---|---|---|
| 给已有 variant **加字段** | **OK**，新字段被忽略 | 前向兼容天然成立 |
| **加 `op` 变体** | `Err: unknown variant` | 不兼容，但有救：**信封先解析**拿回 `req_id` 再回可操作错误（§2.2） |
| **加帧类型 `t`** | `Err`，信封也失败 | 唯一不可拦的变更；靠 `proto_version` 提前挡 |
| 加 `deny_unknown_fields` | 会让第一种情形**变成错误** | **绝不加**：等于每加一个字段就破一次旧 client |

### 2.2 信封先解析 / envelope-first

对**已知帧 + 未知 `op`**，server 先只解析 `{t: String, req_id: u64}` 最小信封，拿到 `req_id` 后回
`Error{code:"unknown_op", req_id}` 而不是直接断连——client 能收尾那个 `req_id` 并给用户一句人话。
实测：`t="SnapshotV2"`、`t="Ping"`+类型错误都能从信封取回 `t`；只有负载**根本不是 TOML** 时才连信封都失败，
那时也确无 `req_id` 可回，直接断连记日志。

## 3. 消息全集 / Messages

约定：字段 `snake_case`；`Option` 只用**一层**（§6.2）；`*_ms` 是 `i64` epoch 毫秒；`tunnel_id: u64`。

### 3.1 握手

| 消息 | 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|---|
| `Hello`（C→S，必须首帧） | `proto_version` | u32 | 是 | 协议版本，当前 `1` |
| | `client_version` | string | 是 | `CARGO_PKG_VERSION`，仅日志/错误文案 |
| | `config_path` | string | 是 | client 认为在连哪份配置；server 比对（§1.1 碰撞兜底） |
| | `host_key_policy` | string | 是 | `default`/`accept_new`/`ignore_changes`，来自 `--ignore-host-key-changes` |
| `Welcome`（S→C） | `proto_version` / `server_version` | u32 / string | 是 | server 侧版本 |
| | `config` | string | 是 | 规范化后的**真实**路径；UI 页脚显示 + client 比对 |
| | `server_pid` / `started_at_ms` | u32 / i64 | 是 | 排障 / uptime |
| | `snapshot` | table | 是 | 首个快照（省一次往返，client 立刻有内容可画） |

版本不匹配 → `Error` 后断开（**不**降级，理由见 §8）。

### 3.2 `Snapshot` —— 状态同步的唯一途径

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `revision` | u64 | 是 | **单调递增**；client 丢弃 `revision <= local` |
| `server_now_ms` | i64 | 是 | 发帧时刻 server 墙钟，算 `age` 的基准（§5） |
| `client_count` | u32 | 是 | 当前连接数；**最后一个 client 退出提示**的唯一依据（§9） |
| `keep_server` | bool | 是 | server 是否「不因 client 归零而退出」。**必须让用户看见**，否则「关窗=全停」的直觉与实际不符 |
| `shutting_down` | bool | 是 | server 已进入关服宽限 → client 提示「即将停止」 |
| `tunnels[]` | array | 是 | **全量**，顺序 = server `Vec` 顺序 = 表格行序；无隧道发 `tunnels = []` |

`tunnels[i]`（`WireTunnelView`）：

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `cfg` | table | 是 | 直接复用 `TunnelConfig` 的 serde 定义（`src/model/tunnel.rs:57`），**一字段不改** |
| `status` | tagged enum | 是 | 见下 |
| `retries` | u32 | 是 | 对应 `TunnelView.retries` |
| `log_tail` | array | 是（可为空） | **日志尾巴**，上限 `SNAPSHOT_LOG_TAIL = 15`（= `ui/layout.rs:55` 的 `DETAIL_LOG_TAIL`）。理由见 §7 |

`status` 的 wire 形态（`Instant` → `*_ms`，其余逐字保留 `src/model/status.rs:21-32`）：

| 变体 | 字段 |
|---|---|
| `Stopped` | — |
| `Starting` / `Up` | `since_ms: i64` |
| `Retrying` | `attempt: u32`, `next_at_ms: i64`, `kind: String`, `short: String`, `detail: String` |
| `Failed` | `kind: String`, `short: String`, `detail: String` |

失败分类的 wire 标识用既有 `FailureKind::name()`（`src/model/failure.rs:83`，如 `connection_refused`），
**不**把中文 `short()` 当 wire 标识 —— 源码注释明说它 *may be reworded*。
`short` 传中文短句、`detail` 传原始 stderr；英文半边由 client 用 `kind.short_en()` 现场合成——
多语言文案不占带宽、不随版本漂移。

### 3.3 `LogLine` —— 只追加的日志流

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `tunnel_id` | u64 | 是 | 一条隧道一批；不跨隧道混批，client 才能追加到正确的 ring |
| `dropped` | u32 | **是**（省略=0） | 本条之前因队列满被丢弃的行数；client 插「⚠ 丢弃 N 行」 |
| `lines[]` | array | 是 | `{time, level, text}`；`level ∈ {debug,info,warn,error}`；`time` 是**已格式化的 `HH:MM:SS`**（沿用 `LogLine::new` 形态，client 不本地取时钟） |

| 参数 | 值 | 理由 |
|---|---|---|
| `LOG_BATCH_MAX` | 64 行/帧 | 实测 64 行 = **6354 B**（≈99 B/行），一次 syscall 摊薄 |
| `LOG_FLUSH_MAX_MS` | 100 ms | 小于 UI 一帧 200 ms（`main.rs` 的 `TICK`），一帧内必达又够并批 |

**为什么与快照分开**：快照「整体替换」、日志「只追加」。混在一起会让每条日志触发一次全量替换；
两者丢弃语义也不同（旧快照丢了自愈，中间日志丢了是永久断档）。

### 3.4 `Command` / `Reply` / `Stale` / `Bye` / `Error`

| 消息 | 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|---|
| `Command` | `req_id` | u64 | 是 | client 自增、**仅本连接内**唯一；配对 `Reply` |
| | `expect_rev` | u64 | **否** | 用户看到并据此操作的 revision；省略 = 无条件执行（仅限幂等命令，§4.2） |
| | `op` | tagged enum | 是 | §4.1 |
| `Reply` | `req_id` | u64 | 是 | 回填 |
| | `ok` | bool | 是 | 成功与否 |
| | `rev` | u64 | 是 | 应用后的 revision（失败 = 未变的 revision） |
| | `error` | table | 否 | `{code, zh, en}`，双语同信息量 |
| | `remaining` | u32 | 否 | 仅应答 `Bye`：server 复核后仍连着的 client 数。补上它而不是新增 `ByeAck` 消息（见 §9）。`0` 时 client 不必再等 |
| | `rows` | array | 否 | 仅 `port_scan`：`{port, process, proto}` |
| `Stale` | `req_id` / `rev` | u64 | 是 | server 当前 revision；client 刷新后由用户决定是否重试 |
| `Bye` | `last_exit` | bool | 是 | 语义 = **「用户已确认过后果」的提议**，不是命令（§9） |
| `Error` | `code` | string | 是 | ASCII 稳定码（§8.1） |
| | `zh` / `en` | string | 是 | 中/英可操作文案，同一信息量 |
| | `req_id` | u64 | **否** | 与某条命令相关时带上；握手期错误省略 |

`Stale` 独立成消息而非 `Reply{ok=false}`：它是**正常控制流**（静默采用新 rev + 一句提示），
与真正失败（弹错误）的 client 分支不同，独立消息可少一次按 `code` 字符串分流。

## 4. 命令与并发

### 4.1 命令枚举

覆盖现有 `ConfirmAction`（`src/app.rs:100-119`）与 `BatchAction` 能触发的全部变更：

| `op` | 字段 | 幂等 | `expect_rev` | server 侧**必须重做**的校验 |
|---|---|---|---|---|
| `start` | `tunnel_id` | 是 | 可选 | `validate_tunnel` + `preflight_bind` + `endpoint_for`（既有 `preflight()`）；已 up 直接 `ok` |
| `stop` | `tunnel_id` | 是 | 可选 | 存在性；已 stopped 则 `ok` |
| `restart` | `tunnel_id` | 否 | 建议 | 同 `start`，且**重读配置**（期间可能被别的 client 改过） |
| `delete` | `tunnel_id` | 否 | **必须** | 存在性；**先停再删** —— 静默丢掉正在跑的转发是数据丢失 |
| `update` | `tunnel_id`, `cfg` | 否 | **必须** | `validate_tunnel(cfg)`；**`cfg.id` 必须等于 `tunnel_id`**（否则可借一次更新改掉别的隧道）；端口冲突 |
| `create` | `cfg` | 否 | 不建议 | server **自己分配 id**（沿用 `id_space_exhausted`），忽略 client 的；`validate_tunnel` + **去重**（`duplicate_of`） |
| `start_all` | — | 是 | 可选 | 逐条走 `start` 校验；**部分失败逐条回报**，不整批回滚 |
| `stop_all` | — | 是 | 可选 | — |
| `port_scan` | `tunnel_id` | 否 | 可选 | 隧道存在且 `up`（扫描要活会话）；用 **server 自己的** ssh config 与 host key 策略 |

**`delete`/`update` 必须带 `expect_rev`**：这两个是「基于某个具体版本的定义做决定」—— A 看到 `revision=42`
的第 3 条，B 在 A 点确认前改了它（`rev=43`），没有 `expect_rev` 就是静默覆盖。
`start`/`stop` 可不带：它们是幂等的**意图**，执行两次与一次同结果；强制带会让「A、B 同时按空格」
其中一个白报 `Stale`。

**`port_scan` 归 server**：扫描要建真实 SSH 会话、用 server 的 ssh config 与 host key 策略；归 client 则
两个 client 各扫一次、各弹 picker，而用户要的正是「一处修改各处相同」。代价：一次扫描可能几百 ms 到数秒，
**不得**阻塞命令处理路径 ⇒ server 回 `Reply{ok}` 表示「已受理」，结果另帧返回。
不建议把结果塞进 `LogLine`：那会混掉「只追加日志」的语义。

### 4.2 校验位置：server 是唯一权威

| 层 | 做 | 不做 |
|---|---|---|
| client | 让用户少走一步：表单字段校验（复用 `config::validate_tunnel`）、`expect_rev` 取自己快照的 `revision`、按 `status` 灰掉不可用命令 | **不做**授权/冲突/资源判定，也**不**预写配置 |
| server | 重跑 `validate_tunnel`、`preflight_bind`、`endpoint_for`、端口冲突、`id` 分配、`expect_rev` 比对，然后才改内存 + `persist()` | 不信任 client 传来的任何 `id`/`enabled`/状态 |

理由：client 快照可能过期到任何程度（网络慢、用户盯着屏幕想了两分钟）；把校验放 client 等于让
「一小时前的世界」决定现在的行为。两个 client 是同 uid 的进程，但**状态视图不同**，
真正共享的那个（server）才是唯一能回答「现在能不能启动」的地方。代价：错误要在 client 优雅呈现
（`Reply.error` 直接进状态条），不能指望「我早拦住了」。

## 5. 时钟与时长 / Clocks

`Instant` 不能跨进程（`src/model/status.rs` 的 `since`/`next_at` 都是 `Instant`），wire 上一律换 `i64` epoch ms。

| 项 | wire 表达 | client 的算法 |
|---|---|---|
| `Starting`/`Up` | `since_ms`（server 墙钟） | `age = server_now_ms.saturating_sub(since_ms)` |
| `Retrying` | `next_at_ms` | `countdown = next_at_ms.saturating_sub(server_now_ms)` |
| 快照 | `server_now_ms` | **基准；client 从不读自己的墙钟** |
| 两帧之间 | — | 把本地 `Instant` 累积的 elapsed 加到 `age` 上 |

**为什么免疫时钟偏移**：client 只用「本地经过多少毫秒」这一个量（`Instant` 差值）；`server_now_ms` 与
`since_ms` 来自**同一个时钟**，相减时任何固定偏移都抵消。client 自己的墙钟**一次都不参与**，
所以 server↔client 差 5 分钟、或 client 被 NTP 猛拽，都不会让「已运行时间」跳变。
**未实测**：没有跑真实双进程注入 ±5 分钟偏移的实验（那是 spike 的断言）；此处论证是纯代数的。

**边界**：`server_now_ms` 是 `SystemTime`，NTP 向后调整可能让 `age` 瞬时偏小（`saturating_sub` 不会偏大）；
本地 `Instant` 累积不受影响，下一次快照即自愈。

## 6. TOML 编码限制与规避 / TOML limits

**这是本协议唯一的实现约束来源**（无 `serde_json`，且不许新增依赖）。全部本机实测：

| # | 限制 | 实测 | 规避规则 |
|---|---|---|---|
| 1 | 表/数组顺序 | 序列化器**总是**把标量排在表之前；手拼「标量在表后」→ `TOML parse error` | **禁止手拼 TOML**，两端都用 `to_string`/`from_str`。顺带消灭注入面 |
| 2 | `Option<T> = None` | 字段**整体省略**（不是 `null`），反序列化回 `None` | wire 语义：**「字段缺失」= None**，无需哨兵 |
| 3 | `Option<Option<T>>` | **有损**：`Some(None)` 与 `None` 都编码成空，无法区分 | **禁止嵌套 Option**；真要三态用显式枚举 `enum Tri { Absent, Clear, Set{v} }` |
| 4 | tagged enum 顶层编码 | `t = "Command"` + 字段，round-trip 相等 | 帧负载**不加**外层包装：省一层表、少 3–5 字节 |
| 5 | 未知字段 | 加字段后老解析器 `Ok`（新字段丢弃） | 前向兼容天然成立；**禁止 `deny_unknown_fields`**。反向：**字段改名 = 静默丢值**，故只许加不许改 |
| 6 | 根必须是表 | `Vec<u32>` 顶层 → `Err(UnsupportedType)` | 每帧负载必须是表（tagged enum 满足） |
| 7 | 字符串换行/控制字符 | 自动用 `"""`；`\r`/NUL 转义；含 `"""` 降级单引号多行 | 日志 `text` **不需**额外转义。**不要**复用 `logging/format.rs` 的 `token()`：那套的理由是「一条记录必须一行」，TOML 是结构化容器，套上去会把日志里的换行吃掉 |
| 8 | 空数组 / 空字符串 | `tunnels = []`、`text = ""` 均可往返 | 「没有」用空数组/空串，不用省略字段（否则与「字段忘发」不可区分） |
| 9 | 超大 `u64` / 负 `i64` | `u64::MAX` 原样往返；`-5` 正常 | `revision` 用 `u64`，别与 `i64` 混算；`server_now_ms` 可为负，client 用 `saturating_sub` |

**备选**（若 TOML 吞吐成为瓶颈）：行式 `key=value` 手写 codec，复用 `logging/format.rs` 的转义纪律。
**本 PR 选 TOML**：实测 50 隧道快照编码 85 µs、解码 427 µs，远小于一帧 200 ms 的预算。

## 7. 体积预算 / Size budget

实测（单位字节，**不含** 4 字节长度前缀）：

| 场景 | 字节数 | 编码 | 解码 |
|---|---|---|---|
| 0 隧道 | **127** | 0.5 µs | 3.2 µs |
| 50 隧道，`log_tail = []` | **16946**（≈17 KB） | 85 µs | 427 µs |
| 50 隧道，日志尾巴 15 行/条 | **96246**（≈94 KB） | 312 µs | 1.6 ms |
| 50 隧道，整个 ring 1000 行/条 | **5410996**（5.2 MB） | 17.9 ms | 84.1 ms |
| 200 隧道，日志尾巴 15 行/条 | **384898** | 1.5 ms | 6.3 ms |

派生量：单隧道固定成本 **336 B** = `(16946−127)/50`；单日志行 **97 B**；单隧道 + 15 行尾巴 **1922 B**。
50×15、10 rev/s 时 CPU ≈ **1.9 %** 单核（`(312µs+1.6ms)×10 = 19.1 ms/s`）。

**结论：全量快照成立，前提是日志必须截断 —— 这是本节唯一的硬约束。**
带整个 ring（1000 行/条）时 50 条隧道 **5.2 MB**、单帧编解 102 ms：超 256 KiB 帧上限 20 倍，根本发不出去。
按 `DETAIL_LOG_TAIL = 15` 截断后 94 KB / 1.9 ms，即便 10 rev/s 也只占约 2 % 单核，
而这已是「50 条隧道全在疯狂输出」的极端假设。

**为什么 15 行够**：`src/ui/detail.rs:150` 只渲染 `view.log.tail(DETAIL_LOG_TAIL)`，
面板滚动上限由 `detail_lines(...).len()` 决定，与 ring 长度无关。**超出 15 行的部分本来就不显示**，
传整个 ring 是纯浪费。真正的「历史日志」由 `LogLine` 流承担（§3.3）：client 自己维护 ring，
订阅期间一条不少；只有「client 连上**之前**的历史」受 15 行限制 —— 与既有滚动窗口行为一致。

规模化上限：1000 条隧道约 340 KB，仍在 256 KiB 之上；**带日志尾巴时会撞帧上限** ⇒ 那时应改 delta
或缩小 `log_tail`。在当前工具的规模（几十条）完全不必要。

## 8. 版本协商与错误

### 8.1 情况与行为

| 情况 | server 行为 |
|---|---|
| `proto_version` 不匹配 | 发 `Error{code:"proto_version_mismatch"}` → 断开。**不降级** |
| 首帧不是 `Hello` / TOML 解析失败 / `config` 不匹配 | 发 `Error{code:"bad_frame"}` → 断开 |
| 帧长超限或 `len == 0` | 直接断开（**不发** `Error`：无法信任后续字节边界） |
| `op` 无法识别（信封可读） | 发 `Error{code:"unknown_op", req_id}` → **不**断开 |
| `expect_rev` 不符 | 发 `Stale`（不断开） |

**为什么不降级**：client 与 server 是同一台机器上的同一个二进制（`sshpf` 既是 client 也是 server），
版本不一致只可能发生在「升级过程中旧 client 还活着」。此时**拒绝并说清楚**比「尽量兼容」安全得多——
半兼容的协议会产生「某些操作静默无效」这类最难查的故障。错误文案要能指导动作：

```text
客户端与后台服务协议版本不一致（客户端 1，服务 2）。请退出所有 sshpf 后重新启动。
The client and the background server speak different protocol versions (client 1, server 2).
Quit every sshpf instance, then start again.
```

### 8.2 兼容变更规则（由 §6 实测推出，不只是惯例）

| 变更 | 兼容 | 要求 |
|---|---|---|
| 加可选字段 | **是**（老解析器忽略） | 无 |
| 加必填字段 / 加 `op` 变体 / 加 `t` 帧类型 | 否 | bump `proto_version`；`op` 变体另需回 `unknown_op` 而非断连 |
| **改字段名** | **否，且最危险**（老解析器静默取默认值、不报错） | 永不；只能加新字段，旧字段隔版本再删 |

## 9. 与「最后一个客户端退出」的关系

| 消息 | 作用 |
|---|---|
| `Snapshot.client_count` | client 用它决定**要不要弹风险提示**（`==1` 且存在运行中转发才弹） |
| `Bye.last_exit` | 告诉 server「用户已确认过后果」，**只是建议** |
| `Snapshot.shutting_down` / `keep_server` | 让 client 显示「即将停止」/「本 server 不随客户端退出而停止」 |

**协议必须保证**：`client_count` 来自 server 自己的计数，不是 client 上报的数字。
client 上报的计数不可信（可过期、可伪造），而「谁能停止所有人的转发」必须由 server 判断。

**为什么 `last_exit` 只能是建议（关键竞态）**：client 的依据是**上一帧快照**的 `client_count`，
在它弹框、等用户读文案的几秒里完全可能已变：

```text
client A 快照: client_count=1 → 弹框「你是最后一个」
                                     │  用户正在读文案
client B ───────────── connect ──────┘  server: 1 → 2
client A 按 y → Bye{last_exit=true} ───▶ server 复核 2-1=1 > 0 → 不退出 ✅
```

若 server 信任 `last_exit`，这里会**把 B 的转发全部掐掉**——一个刚连上的用户在自己没有任何操作的
情况下失去所有隧道，原因在另一个窗口的确认框里。这就是「**过期的视图不得拥有破坏权**」的具体形态。
反方向安全：client 以为还有别人而实际 B 已断 → A 发普通 `Bye`，server 复核 `==0` 仍正常关服。
即 **`last_exit` 只影响「用多长的宽限」，不影响「退不退」**。

**`Bye` 的应答不新增消息类型**：server 用 `Reply{req_id, ok:true, rev, remaining}` 回应，
`remaining > 0` 即「还有 N 个 client，服务继续」。单个可选字段比为「再见」专门造一个
`ByeAck` 更小，也让 `req_id` 配对规则保持唯一（见 [migration.md](migration.md) §11.3）。

`client_count` = 「已过 `Hello` 握手且未 EOF」的连接数。**不**含正在 connect 但还没发 `Hello` 的 socket
（它们还不是 client，不该阻止关服）。因此竞态窗口内 `client_count` 可能**偏小** —— 这是
「server 必须复核」的第二个理由。

**异常退出没有协商机会**：崩溃 / `SIGKILL` / 关终端 → **没有 `Bye`、没有确认框，链接直接 EOF**。
必须承认的不对称：**「最后一个终端崩溃 = 所有端口转发静默停止」**。协议侧只能靠 `client_count`
让任何 client **随时**提示「当前只有你一个客户端，关掉窗口会让所有转发停止」，
把风险从「退出那一刻」前移到「随时可见」；宽限窗口与 `--keep-server` 的取舍见 lifecycle。

## 10. 慢消费者 / Slow consumers

**实测的阻塞点**：对端 connect 后完全不读，server 连续写会在 **233152 B（0.22 MiB）** 后 `WouldBlock`
（socket 缓冲满）；加 `set_write_timeout(50ms)` 后仍在同位置失败、耗时 111.9 ms。
含义：若在广播循环里阻塞写，**一个卡住的 client 会让所有 client 停止收到快照**。

| 机制 | 做法 | 理由 |
|---|---|---|
| 每 client **写线程 + 有界队列** | `sync_channel`：快照队列 2、日志队列 256 帧 | 广播方只 `try_send`，**永不阻塞**在 socket 上 |
| 快照队列满 → **丢旧留新** | 先扔队头再放队尾 | 快照幂等，旧帧无价值；留最新一帧即可追上 |
| 日志队列满 → **丢弃并计数** | 累计进下次 `LogLine.dropped` | 日志有损可接受，但**必须让用户看见丢了多少** |
| 连续 `SLOW_CLIENT_DISCONNECT_MS = 5000` 写不出去 → 断开 | 写线程带 `set_write_timeout`，累计超阈值即 `shutdown` | 「活着但不读」会永远占队列；断开比无限缓冲诚实 |
| 断开记 `ipc.client_dropped{reason, pending_bytes}` | 见 lifecycle 事件清单 | 静默断开会让「我的界面为什么不更新了」无法回答 |

**为什么不是「阻塞写 + 加大 socket 缓冲」**：缓冲由内核 `net.core.wmem_default` 决定，我们控制不了；
调大只把「多久后卡死」推迟。**为什么不是「一 client 一线程直接阻塞写」**：线程无法被丢弃 ——
有界队列让「跟不上」在**入队时**就暴露。

**为什么快照断开而日志只丢不连**：快照是状态，队列满说明该 client **根本读不动**，
丢帧只会让队列继续满，最后必然要断；断开让它重连、重连即拿全量新快照，自然自愈。
日志丢的是历史而非状态，故单独有界、攒 `dropped` 计数后继续发。

## 11. 未决与未实测

| 问题 | 状态 |
|---|---|
| 1 MiB / 256 KiB 帧上限是否足够 | 几十条隧道够（带日志尾巴 94 KB）；上千条需改 delta |
| TOML 编码吞吐 | 实测 50 隧道 85 µs 编码，远未到瓶颈；若成为瓶颈改行式 codec |
| Windows | 无 UDS；named pipe 需新依赖或 `unsafe`，loopback TCP 则安全边界更差 → **本 PR 不做**，Windows 退回单进程模式（即当前行为） |
| 同 uid 进程的认证 | 明确不做（需 `unsafe` 调 `getsockopt(SO_PEERCRED)`，违反「unsafe 只在 crash.rs」） |
| **真实双进程端到端** | 属 spike 职责；本文件只定协议 |
| **±5 分钟时钟偏移下 age 不变** | **未实测**：§5 论证纯代数（client 无第三处用本地墙钟） |
| **慢 client 断开的真实内存曲线** | **未实测**：只测到 0.22 MiB 后写阻塞这一点，队列上限需压测 |
