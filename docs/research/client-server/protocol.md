# 协议设计 / Wire protocol（调研稿）

> **这份文档回答**：帧怎么切、有哪些消息与命令、字段是什么、**哪些变更是兼容的（§8.2）**、TOML 编码有哪些限制、体积预算、`Instant` 这类不可跨进程的值怎么表达。
> **不负责**：进程启停与选举（见 [lifecycle.md](lifecycle.md)）、迁移步骤（见 [migration.md](migration.md)）、方案背景（见 [../client-server.md](../client-server.md)）。
> **相关代码路径**：`src/ipc/`（`msg` / `snapshot` / `codec` / `frame` / `endpoint` / `transport`）、`src/model/tunnel.rs`、`src/model/status.rs`、`src/model/failure.rs`。

> 历史设计稿（2026-11），写于 `src/ipc/` 落地之前。**§8.2 的兼容变更规则仍被 `src/ipc/mod.rs` 引用为契约**；
> 其余细节以 `src/ipc/` 代码为准。两处已知偏差：传输层最终是**回环 TCP + 令牌**而非 UDS；
> 端点用 hash 命名而非转义路径（见 [../../client-server.md](../../client-server.md) §9）。
> 文中"实测"数字来自 Linux 探针（Rust 1.99.1 / `toml 1.2.x`），是当时值。

## 0. 一页结论

| 结论 | 理由 |
|---|---|
| server 身份 = **哪份配置**；端点名由配置路径的 hash 派生 | 否则同机两套配置共用一个 server 互相污染 |
| 帧 = `u32` 大端长度前缀 + TOML，tagged enum **顶层直接编码** | 实测可序列化，不必包 `[op]`；TOML 是已有依赖，零新增 |
| **全量快照 + revision**，非 delta；快照只带**日志尾巴 15 行** | 50 隧道（15 行尾巴）实测 94 KB、编+解 1.9 ms；带整个 ring 则 5.2 MB，帧上限都过不去 |
| 命令 `Command{req_id, expect_rev, op}` + `Reply`；**权威校验一律在 server 重做** | client 快照可能过期，过期视图不得决定现在的行为 |
| 慢消费者 = 每 client 有界队列，跟太慢则**断开** | 实测对端不读时 233152 B 后写阻塞；广播不得为任何 client 停住 |
| 最后一个 client 退出：`Bye{last_exit}` 只是**提议** | 过期视图不得拥有破坏权（§9） |

## 1. 传输层

### 1.1 端点派生

```text
config_path = 规范化后的 Paths.config
server_id   = hash(config_path) → 小写 hex
endpoint_dir = 运行时目录 / "sshpf"（本机实测 XDG runtime dir 为 0700，重启即清空）
    文件名    = {server_id}.port / .token / .lock / (UDS 方案下的 .sock)
```

| 决策 | 理由 | 代价 |
|---|---|---|
| 用 FNV-1a，不用 `DefaultHasher` | std 源文档明说算法"is not specified … should not be relied upon over releases"；升级工具链会让新旧 client 算出不同路径 → 各拉一个 server | 自写 12 行常量；用途是**命名**不是防碰撞 |
| 碰撞兜底：握手回传 `config` 并比对 | 碰撞后果是"别人的隧道出现在你表里"，不可接受；一次字符串比较归零 | 一个字段 |
| 目录 0700、端点文件显式收紧 | 文件系统权限曾是唯一内核级边界；**实测 bind 后默认模式受 umask 影响**，不收紧则同组可读写 | 同 uid 任何进程仍可读（→ 需要令牌，见 §1.3） |

### 1.2 「规范化」用词法消解，不用 `canonicalize`

实测：`canonicalize` 对**尚未创建**的路径返回 `Err(NotFound)`，而首次运行时配置文件根本不存在，
且端点名必须在读文件**之前**算出 → 只能用 `std::path::absolute` + 词法消解 `.`/`..`
（实测 `absolute` 已消解 `.`，但**保留** `..`，须自行折叠）。

代价：不解析符号链接，`cfg.toml` 与 `link-to-cfg.toml` 是两个 server。可接受（用户显式给了不同路径），
但 `--server` 启动日志必须打 `config_path`，让"为什么我开了两个"可诊断。

### 1.3 为什么不用 abstract socket（UDS 方案时期的判断）

| 维度 | 文件系统 socket | Linux abstract |
|---|---|---|
| 权限控制 | 目录 0700 + 文件 0600，内核强制 | **无**：无文件可 chmod，任何进程可连 |
| 残留 | 留文件，靠 `ECONNREFUSED` + 抢锁识别 | 自动回收 |
| 可移植性 | Unix 通用 | **仅 Linux** |

决定性理由：`peer_cred()` 在 std 仍是 unstable 之后，abstract socket **等于完全没有访问控制**。

> **实际落地放弃了 UDS 全线**：Windows 上 `std` 没有 UDS API，最终用回环 TCP + 令牌
> （端口可被任何人连，但读不到令牌就不被服务）。

## 2. 帧格式

```text
+---------------------+----------------------------------+
| len: u32 大端        | payload: TOML (UTF-8), len 字节   |
+---------------------+----------------------------------+
```

| 规则 | 值 / 做法 | 理由 |
|---|---|---|
| 帧上限 | `MAX_FRAME = 256 * 1024`，超限断连并记事件 | 有上限才能拒"声明 4 GiB"的坏长度，避免按声明分配 |
| 解码失败必须不 panic | `toml::from_str` 对空串/坏字节/类型错误实测全返 `Err` | 生产代码禁 panic；对端是不受信输入 |
| 读超时 | 双方 `set_read_timeout(2000ms)` | 实测静默对端 **86.3 ms** 返回 `WouldBlock`；无超时则半开连接永久占线程 |

### 2.1 前向兼容（实测）

| 情形 | 实测 | 含义 |
|---|---|---|
| 给已有 variant **加字段** | **OK**，新字段被忽略 | 前向兼容天然成立 |
| **加 `op` 变体** | `Err: unknown variant` | 不兼容，但有救：**信封先解析**拿回 `req_id` 再回可操作错误 |
| **加帧类型 `t`** | `Err`，信封也失败 | 唯一不可拦的变更；靠 `proto_version` 提前挡 |
| 加 `deny_unknown_fields` | 会让第一种情形**变成错误** | **绝不加**：等于每加一个字段就破一次旧 client |

### 2.2 信封先解析 / envelope-first

对**已知帧 + 未知 `op`**，server 先只解析 `{t, req_id}` 最小信封，拿到 `req_id` 后回
`Error{code:"unknown_op", req_id}` 而不是直接断连——client 能收尾那个 `req_id` 并给用户一句人话。
只有负载**根本不是 TOML** 时才连信封都失败，那时也确无 `req_id` 可回，直接断连记日志。

## 3. 消息

约定：字段 `snake_case`；`Option` 只用**一层**（§6）；`*_ms` 是 `i64` epoch 毫秒；`tunnel_id: u64`。

### 3.1 握手

| 消息 | 字段 | 语义 |
|---|---|---|
| `Hello`（C→S，必须首帧） | `proto_version` / `client_version` / `config_path` / `host_key_policy`（+ 实际的 `token`） | client 认为在连哪份配置；`host_key_policy` 来自 `--ignore-host-key-changes`；`token` 是回环 TCP 方案的接入凭证 |
| `Welcome`（S→C） | `proto_version` / `server_version` / `config` / `server_pid` / `started_at_ms` / `snapshot` | `config` 是规范化后的**真实**路径（UI 页脚 + 碰撞比对）；首个快照省一次往返 |

版本不匹配 → `Error` 后断开（**不**降级，§8）。

### 3.2 `Snapshot` —— 状态同步的唯一途径

| 字段 | 类型 | 语义 |
|---|---|---|
| `revision` | u64 | **单调递增**；client 丢弃 `revision <= local` |
| `server_now_ms` | i64 | 发帧时刻 server 墙钟，算 age 的基准（§5） |
| `client_count` | u32 | 当前连接数（server 自己数的）；**最后 client 提示的唯一依据** |
| `keep_server` | bool | server 是否"不因 client 归零而退出"。**必须让用户看见** |
| `shutting_down` | bool | 已进入关服宽限 → client 提示"即将停止" |
| `tunnels[]` | array | **全量**，顺序 = 表格行序；无隧道发 `[]` |

`tunnels[i]`：`cfg`（复用 `TunnelConfig` 的 serde 定义，**一字段不改**）、`status`、`retries`、
`log_tail`（上限 `SNAPSHOT_LOG_TAIL = 15`，与详情面板一致）。

`status` 的 wire 形态（`Instant` → `*_ms`，其余逐字保留）：

| 变体 | 字段 |
|---|---|
| `Stopped` | — |
| `Starting` / `Up` | `since_ms` |
| `Retrying` | `attempt`, `next_at_ms`, `kind`, `short`, `detail` |
| `Failed` | `kind`, `short`, `detail` |

失败分类的 wire 标识用既有 `FailureKind` 的稳定 ASCII 名（如 `connection_refused`），
**不**把中文 `short()` 当标识——源码注释明说它 *may be reworded*。
`short` 传中文短句、`detail` 传原始 stderr；英文半边由 client 现场合成——多语言文案不占带宽。

### 3.3 `LogLine` —— 只追加的日志流

| 字段 | 语义 |
|---|---|
| `tunnel_id` | 一条隧道一批；不跨隧道混批，client 才能追加到正确的 ring |
| `dropped` | 本条之前因队列满被丢弃的行数；client 插"⚠ 丢弃 N 行" |
| `lines[]` | `{time, level, text}`；`level ∈ {debug,info,warn,error}`；`time` 是**已格式化的 `HH:MM:SS`**（client 不本地取时钟） |

`LOG_BATCH_MAX = 64` 行/帧（实测 64 行 = 6354 B，≈99 B/行）；`LOG_FLUSH_MAX_MS = 100`
（小于 UI 一帧，一帧内必达又够并批）。

**为什么与快照分开**：快照"整体替换"、日志"只追加"。混在一起会让每条日志触发一次全量替换；
两者丢弃语义也不同（旧快照丢了自愈，中间日志丢了是永久断档）。

### 3.4 `Command` / `Reply` / `Stale` / `Bye` / `Error`

| 消息 | 字段 | 语义 |
|---|---|---|
| `Command` | `req_id` / `expect_rev`（可选） / `op` | `req_id` 仅本连接内唯一；`expect_rev` 省略 = 无条件执行（仅限幂等命令） |
| `Reply` | `req_id` / `ok` / `rev` / `error{code,zh,en}`（可选） / `remaining`（可选，仅应答 `Bye`） / `rows`（可选，仅 `port_scan`） | 应用后的 revision（失败 = 未变） |
| `Stale` | `req_id` / `rev` | server 当前 revision；client 刷新后由用户决定是否重试 |
| `Bye` | `last_exit` | 语义 = **"用户已确认过后果"的提议**，不是命令（§9） |
| `Error` | `code`（ASCII 稳定码） / `zh` / `en` / `req_id`（可选） | 双语同信息量 |

`Stale` 独立成消息而非 `Reply{ok=false}`：它是**正常控制流**（静默采用新 rev + 一句提示），
与真正失败（弹错误）的 client 分支不同，独立消息可少一次按 `code` 分流。

## 4. 命令与并发

### 4.1 命令枚举

| `op` | 字段 | 幂等 | `expect_rev` | server 侧**必须重做**的校验 |
|---|---|---|---|---|
| `start` | `tunnel_id` | 是 | 可选 | 定义校验 + 端口预检；已 up 直接 `ok` |
| `stop` | `tunnel_id` | 是 | 可选 | 存在性；已 stopped 则 `ok` |
| `restart` | `tunnel_id` | 否 | 建议 | 同 `start`，且**重读配置**（期间可能被别的 client 改过） |
| `delete` | `tunnel_id` | 否 | **必须** | 存在性；**先停再删**——静默丢掉正在跑的转发是数据丢失 |
| `update` | `tunnel_id`, `cfg` | 否 | **必须** | 定义校验；**`cfg.id` 必须等于 `tunnel_id`**；端口冲突 |
| `create` | `cfg` | 否 | 不建议 | server **自己分配 id**，忽略 client 的；定义校验 + 去重 |
| `start_all` / `stop_all` | — | 是 | 可选 | 逐条走单个命令的校验；**部分失败逐条回报**，不整批回滚 |
| `port_scan` | `tunnel_id` | 否 | 可选 | 隧道存在且 `up`；用 **server 自己的** ssh config 与 host key 策略 |

**`delete`/`update` 必须带 `expect_rev`**：这两个是"基于某个具体版本的定义做决定"——
A 看到 `revision=42` 的第 3 条，B 在 A 点确认前改了它（`rev=43`），没有 `expect_rev` 就是静默覆盖。
`start`/`stop` 可不带：它们是幂等的**意图**；强制带会让"A、B 同时按空格"其中一个白报 `Stale`。

**`port_scan` 归 server**：扫描要建真实 SSH 会话、用 server 的 ssh config 与 host key 策略。
代价：一次扫描可能几百 ms 到数秒，**不得**阻塞命令处理路径 ⇒ server 回 `Reply{ok}` 表示"已受理"，
结果另帧返回。

### 4.2 校验位置：server 是唯一权威

| 层 | 做 | 不做 |
|---|---|---|
| client | 让用户少走一步：表单字段校验、`expect_rev` 取自己快照的 `revision`、按 `status` 灰掉不可用命令 | **不做**授权/冲突/资源判定，也**不**预写配置 |
| server | 重跑定义校验、端口预检、端点派生、端口冲突、`id` 分配、`expect_rev` 比对，然后才改内存 + `persist()` | 不信任 client 传来的任何 `id`/`enabled`/状态 |

理由：client 快照可能过期到任何程度（用户盯着屏幕想了两分钟）；把校验放 client 等于让"一小时前的世界"
决定现在的行为。两个 client 是同 uid 的进程，但**状态视图不同**，真正共享的那个才是唯一能回答
"现在能不能启动"的地方。

## 5. 时钟与时长

`Instant` 不能跨进程，wire 上一律换 `i64` epoch ms。

| 项 | wire 表达 | client 的算法 |
|---|---|---|
| `Starting`/`Up` | `since_ms`（server 墙钟） | `age = server_now_ms.saturating_sub(since_ms)` |
| `Retrying` | `next_at_ms` | `countdown = next_at_ms.saturating_sub(server_now_ms)` |
| 快照 | `server_now_ms` | **基准；client 从不读自己的墙钟** |
| 两帧之间 | — | 把本地 `Instant` 累积的 elapsed 加到 `age` 上 |

**为什么免疫时钟偏移**：client 只用"本地经过多少毫秒"这一个量；`server_now_ms` 与 `since_ms`
来自**同一个时钟**，相减时任何固定偏移都抵消。server↔client 差 5 分钟、或 client 被 NTP 猛拽，
都不会让"已运行时间"跳变（spike 的 `clock` 断言：三种偏移都算出同一 age）。

**边界**：`server_now_ms` 是 `SystemTime`，NTP 向后调整可能让 `age` 瞬时偏小
（`saturating_sub` 不会偏大）；下一次快照即自愈。

## 6. TOML 编码限制与规避

**这是本协议唯一的实现约束来源**（无 `serde_json`，且不许新增依赖）。全部本机实测：

| # | 限制 | 实测 | 规避规则 |
|---|---|---|---|
| 1 | 表/数组顺序 | 序列化器**总是**把标量排在表之前；手拼"标量在表后"→ parse error | **禁止手拼 TOML**，两端都用 `to_string`/`from_str`。顺带消灭注入面 |
| 2 | `Option<T> = None` | 字段**整体省略**，反序列化回 `None` | wire 语义：**"字段缺失" = None**，无需哨兵 |
| 3 | `Option<Option<T>>` | **有损**：`Some(None)` 与 `None` 都编码成空 | **禁止嵌套 Option**；三态用显式枚举 |
| 4 | tagged enum 顶层编码 | `t = "Command"` + 字段，round-trip 相等 | 帧负载**不加**外层包装 |
| 5 | 未知字段 | 加字段后老解析器 `Ok` | 前向兼容天然成立；**禁止 `deny_unknown_fields`**。反向：**字段改名 = 静默丢值**，故只许加不许改 |
| 6 | 根必须是表 | `Vec<u32>` 顶层 → `Err(UnsupportedType)` | 每帧负载必须是表（tagged enum 满足） |
| 7 | 字符串换行/控制字符 | 自动用 `"""`；`\r`/NUL 转义 | 日志 `text` **不需**额外转义。**不要**复用手写的行式转义：那套的理由是"一条记录必须一行" |
| 8 | 空数组 / 空字符串 | 均可往返 | "没有"用空数组/空串，不用省略字段（否则与"字段忘发"不可区分） |
| 9 | 超大 `u64` / 负 `i64` | `u64::MAX` 原样往返；`-5` 正常 | `revision` 用 `u64`，别与 `i64` 混算 |

**备选**（若 TOML 吞吐成为瓶颈）：行式 `key=value` 手写 codec。当时选 TOML：50 隧道快照编码 85 µs、
解码 427 µs，远小于一帧 200 ms 的预算。

## 7. 体积预算

实测（单位字节，**不含** 4 字节长度前缀）：

| 场景 | 字节数 | 编码 | 解码 |
|---|---|---|---|
| 0 隧道 | **127** | 0.5 µs | 3.2 µs |
| 50 隧道，`log_tail = []` | **16946**（≈17 KB） | 85 µs | 427 µs |
| 50 隧道，日志尾巴 15 行/条 | **96246**（≈94 KB） | 312 µs | 1.6 ms |
| 50 隧道，整个 ring 1000 行/条 | **5410996**（5.2 MB） | 17.9 ms | 84.1 ms |
| 200 隧道，日志尾巴 15 行/条 | **384898** | 1.5 ms | 6.3 ms |

派生量：单隧道固定成本 **336 B**；单日志行 **97 B**；单隧道 + 15 行尾巴 **1922 B**。
50×15、10 rev/s 时 CPU ≈ **1.9 %** 单核。

**结论：全量快照成立，前提是日志必须截断 —— 这是本节唯一的硬约束。**
带整个 ring 时 50 条隧道 **5.2 MB**、单帧编解 102 ms：超 256 KiB 帧上限 20 倍，根本发不出去。

**为什么 15 行够**：详情面板只渲染 `log.tail(15)`，滚动上限由可见行数决定，与 ring 长度无关——
**超出部分本来就不显示**，传整个 ring 是纯浪费。真正的"历史日志"由 `LogLine` 流承担（§3.3）：
client 自己维护 ring，订阅期间一条不少；只有"client 连上**之前**的历史"受 15 行限制。

规模化上限：1000 条隧道约 340 KB，仍在 256 KiB 之上；带日志尾巴时会撞帧上限 ⇒ 那时应改 delta
或缩小 `log_tail`。在当前工具的规模（几十条）完全不必要。

## 8. 版本协商与错误

### 8.1 情况与行为

| 情况 | server 行为 |
|---|---|
| `proto_version` 不匹配 | 发 `Error{code:"proto_version_mismatch"}` → 断开。**不降级** |
| 首帧不是 `Hello` / TOML 解析失败 / `config` 不匹配 | 发 `Error{code:"bad_frame"}` → 断开 |
| 令牌错误（回环 TCP 方案） | 发 `Error{code:"bad_token"}` → 断开，**不回显密钥** |
| 帧长超限或 `len == 0` | 直接断开（**不发** `Error`：无法信任后续字节边界） |
| `op` 无法识别（信封可读） | 发 `Error{code:"unknown_op", req_id}` → **不**断开 |
| `expect_rev` 不符 | 发 `Stale`（不断开） |

**为什么不降级**：client 与 server 是同一台机器上的同一个二进制，版本不一致只可能发生在
"升级过程中旧 client 还活着"。此时**拒绝并说清楚**比"尽量兼容"安全得多——半兼容的协议会产生
"某些操作静默无效"这类最难查的故障。错误文案要能指导动作：

```text
客户端与后台服务协议版本不一致（客户端 1，服务 2）。请退出所有 sshpf 后重新启动。
The client and the background server speak different protocol versions (client 1, server 2).
Quit every sshpf instance, then start again.
```

### 8.2 兼容变更规则（**这是 `src/ipc/mod.rs` 引用的契约**）

由 §6 实测推出，不只是惯例：

| 变更 | 兼容 | 要求 |
|---|---|---|
| 加可选字段 | **是**（老解析器忽略） | 无 |
| 加必填字段 / 加 `op` 变体 / 加 `t` 帧类型 | 否 | bump `proto_version`；`op` 变体另需回 `unknown_op` 而非断连 |
| **改字段名** | **否，且最危险**（老解析器静默取默认值、不报错） | 永不；只能加新字段，旧字段隔版本再删 |
| 快照里加字段 | 是 | 只许加；重命名是破坏性变更 |

## 9. 与"最后一个客户端退出"的关系

| 消息 | 作用 |
|---|---|
| `Snapshot.client_count` | client 用它决定**要不要弹风险提示**（`==1` 且存在运行中转发才弹） |
| `Bye.last_exit` | 告诉 server"用户已确认过后果"，**只是建议** |
| `Snapshot.shutting_down` / `keep_server` | 让 client 显示"即将停止" / "本 server 不随客户端退出而停止" |

**协议必须保证**：`client_count` 来自 server 自己的计数，不是 client 上报的数字。

**为什么 `last_exit` 只能是建议**：client 的依据是上一帧快照的 `client_count`，在它弹框、等用户
读文案的几秒里完全可能已变（spike `e2e` 用三个真实进程构造过这个交错）：

```text
client A 快照: client_count=1 → 弹框「你是最后一个」
                                     │  用户正在读文案
client B ────────── connect ────────┘  server: 1 → 2
client A 按 y → Bye{last_exit=true} ──▶ server 复核 2-1=1 > 0 → 不退出 ✅
```

若 server 信任 `last_exit`，这里会**把 B 的转发全部掐掉**。这就是"**过期的视图不得拥有破坏权**"。
反方向安全：client 以为还有别人而实际 B 已断 → A 发普通 `Bye`，server 复核 `==0` 仍正常关服。
即 **`last_exit` 只影响"用多长的宽限"，不影响"退不退"**。

**`Bye` 的应答不新增消息类型**：server 用 `Reply{req_id, ok, rev, remaining}` 回应，
`remaining > 0` 即"还有 N 个 client，服务继续"。单个可选字段比为"再见"专造一个 `ByeAck` 更小，
也让 `req_id` 配对规则保持唯一。

`client_count` = "已完成 `Hello` 握手且未 EOF"的连接数，**不含**正在 connect 还没发 `Hello` 的 socket。
因此竞态窗口内 `client_count` 可能**偏小**——这是"server 必须复核"的第二个理由。

**异常退出没有协商机会**：崩溃 / `SIGKILL` / 关终端 → 没有 `Bye`、没有确认框，直接 EOF。
协议侧只能靠 `client_count` 让任何 client **随时**提示风险，把风险从"退出那一刻"前移到"随时可见"。

## 10. 慢消费者

**实测的阻塞点**：对端 connect 后完全不读，server 连续写会在 **233152 B（0.22 MiB）** 后
`WouldBlock`；加 `set_write_timeout(50ms)` 后仍在同位置失败。含义：若在广播循环里阻塞写，
**一个卡住的 client 会让所有 client 停止收到快照**。

| 机制 | 做法 | 理由 |
|---|---|---|
| 每 client **写线程 + 有界队列** | 快照队列 2、日志队列 256 帧 | 广播方只 `try_send`，**永不阻塞**在 socket 上 |
| 快照队列满 → **丢旧留新** | 先扔队头再放队尾 | 快照幂等，旧帧无价值 |
| 日志队列满 → **丢弃并计数** | 累计进下次 `LogLine.dropped` | 日志有损可接受，但**必须让用户看见丢了多少** |
| 连续 `SLOW_CLIENT_DISCONNECT_MS = 5000` 写不出去 → 断开 | 写线程带 `set_write_timeout` | "活着但不读"会永远占队列；断开比无限缓冲诚实 |
| 断开记 `reason` + `pending_bytes` | — | 静默断开会让"我的界面为什么不更新了"无法回答 |

**为什么不是"阻塞写 + 加大 socket 缓冲"**：缓冲由内核决定，我们控制不了，调大只把"多久后卡死"推迟。
**为什么快照断开而日志只丢不连**：快照是状态，队列满说明该 client **根本读不动**，丢帧只会让队列
继续满；断开让它重连、重连即拿全量新快照，自然自愈。日志丢的是历史而非状态。

## 11. 未决与未实测

| 问题 | 状态 |
|---|---|
| 256 KiB 帧上限是否足够 | 几十条隧道够（带日志尾巴 94 KB）；上千条需改 delta |
| TOML 编码吞吐 | 实测 50 隧道 85 µs 编码，远未到瓶颈 |
| 同 uid 进程的认证 | 回环 TCP 方案用令牌解决（§1.3）；内核级 uid 校验仍需 `unsafe`，不做 |
| 慢 client 断开的真实内存曲线 | **未实测**：只测到 0.22 MiB 后写阻塞这一点，队列上限需压测 |
| 长时间运行的句柄/内存增长 | 未做 soak |
