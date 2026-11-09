# 协议 / Wire protocol

> **这份文档回答**：client 与 server 之间走什么传输、帧怎么切、有哪些消息、字段是什么、
> 版本怎么协商、慢 client 怎么隔离、`Instant` 这类不可跨进程的值怎么表达。
> **不负责**：进程怎么起停与选举（见 [lifecycle.md](lifecycle.md)）、迁移步骤（见 [migration.md](migration.md)）、
> 方案取舍的背景（见 [../client-server.md](../client-server.md)）。
> **相关代码路径**：本 PR 不含实现；将来落在 `src/ipc/`（`frame.rs` / `msg.rs` / `snapshot.rs` /
> `codec.rs` / `endpoint.rs`）。可参照的既有转义纪律在 `src/logging/format.rs`，
> wire 化的源类型在 `src/model/tunnel.rs`、`src/model/status.rs`。

## 设计原则 / Principles

| 原则 | 含义 | 代价 |
|---|---|---|
| **状态用全量快照，不用增量** | 每次变更广播完整状态 | 带宽比 delta 大，但乱序/丢帧/重连**结构上不可能** desync |
| **命令由 server 重新校验** | client 的请求只是意图，不是事实 | server 侧多一遍校验；换来「过期 client 不能破坏状态」 |
| **wire 类型与内部类型分离** | `WireTunnelView` 独立于 `TunnelView` | 多约 150 行映射；换来协议可独立演进 |
| **不新增依赖** | 只用 std + 已有 `serde`/`toml` | 没有 `serde_json`，用 TOML 有 schema 限制（见 §6） |
| **连接即权限** | socket 文件权限即身份 | 不能区分同 uid 的不同进程（见 §1.3） |

## 1. 传输层 / Transport

### 1.1 端点推导

server 的**身份就是一份配置**：同一份 `config.toml` 只有一个 server。
路径全部由规范化的 `config_path` 派生：

```text
dir      = <runtime_dir>/sshpf/            # 0700
key      = hex(blake3_like(config_path))   # 见下方说明
socket   = dir/<key>.sock                  # 0600
lock     = dir/<key>.lock                  # 0600
```

- `runtime_dir` = `dirs::runtime_dir()`（Linux 为 `$XDG_RUNTIME_DIR`，如 `/run/user/1000`，本身已是 0700）；
  取不到时退到 `state_dir()`。**不用 `temp_dir()`**：`/tmp` 全局可写，socket 放那里等于把控制通道交给所有人。
- **hash 用什么**：不新增依赖 ⇒ 不能用 `blake3`/`sha2`（`sha1` 已在依赖里但不必用）。
  两个可选实现：
  - (a) 用已有 `hmac`/`sha1` 组合（依赖已在，但语义不匹配，属于"为了省依赖硬凑"）；
  - (b) **用规范化路径的转义字符串本身**做文件名（把 `/` 换成 `_`，长度可控），不用 hash。
  推荐 **(b)**：调试时能一眼看出是哪个配置，且完全无依赖。代价是路径很长时文件名过长——
  用「保留末段 + 前 8 字节摘要」的混合形式兜底。**本 PR 建议先 (b)，把 (a) 记为备选。**
- **规范化**：`config_path` 必须 `canonicalize()` 后参与派生，否则 `./config.toml` 与
  `/home/u/.config/sshpf/config.toml` 会得到两个不同的 server（这是最容易踩的坑）。
  canonicalize 失败（文件不存在）→ 用「父目录 canonicalize + 文件名」再失败就用原样路径。

### 1.2 为什么是 Unix domain socket，不是 TCP

| 维度 | UDS（选定） | loopback TCP | Linux abstract |
|---|---|---|---|
| 文件权限控制 | **可以**（0700 目录 + 0600 socket） | 不能（任何本机进程可连） | 不能 |
| 跨平台 | Unix 天然；Windows 无 | 都有 | 仅 Linux |
| 端口占用 | 无 | 需选端口，可能撞 | 无 |
| 进程死亡清理 | 残留文件需处理（见 lifecycle） | 端口自动释放 | **自动回收** |
| 定位调试 | 可 `ls` 看到 | 需知道端口 | 不可见 |

选 UDS 的决定性理由：**它让文件系统权限成为认证机制**。TCP 方案里任何本机进程都能连上
并停掉所有转发；UDS + 0700 目录把访问面收缩到同一 uid（且实际上需能读 `$XDG_RUNTIME_DIR`）。

**abstract socket 为什么不用**：它没有文件权限、名字全局可见（可被同机任意进程抢占同名地址）、
仅 Linux、且调试时不可见。它的唯一优势（进程死亡自动回收）用「残留识别」流程就能补上，
不足以抵消安全与可移植性的损失。

### 1.3 安全边界（诚实说明）

| 威胁 | 是否防住 | 说明 |
|---|---|---|
| 其它 uid 的进程 | **是** | 0700 目录 + 0600 socket（`create_private_file` 既有能力，见 `src/sys/`） |
| **同 uid 的其它进程** | **否** | `peer_cred()` / `SO_PEERCRED` 在 std 仍是 unstable（实测编译失败），本方案无内核级 uid 校验 |
| 恶意 client 停掉所有转发 | **否**（同 uid） | 同 uid 的进程本来就能改 `config.toml`、`kill` 你的进程；这不是新增的攻击面 |

**结论**：本机同 uid 是既有信任边界，不额外引入认证。若将来要防同 uid（多用户共享机器上的
非法提权场景），必须用 `libc` 直调 `getsockopt(SO_PEERCRED)` —— 那会引入 `unsafe`，
超出本仓库「unsafe 只在 crash.rs」的硬约束，因此明确列为**不做**。

## 2. 帧格式 / Framing

```text
+--------+-------------------+
| len:u32|  payload: len B   |
| 大端   |  UTF-8 TOML 文档   |
+--------+-------------------+
```

| 规则 | 值 | 理由 |
|---|---|---|
| 长度前缀 | `u32` 大端 | 网络序天然大端；与平台无关 |
| 最大帧长 | **1 MiB**（`MAX_FRAME = 1 << 20`） | 50 条隧道实测快照约 **19 KB**（见 §7），1 MiB 留足余量且能挡住"长度字段被伪造"的 OOM |
| 超长帧 | **立即断开该连接**，记 `client.protocol_violation` | 不尝试读掉，因为无法信任后续字节边界 |
| `len == 0` | 视为协议违规 | 空帧无意义 |
| 编码 | UTF-8 TOML | 见 §6 |
| 大小端可协商？ | 否 | 固定，减少一个协商维度 |

**每帧一个完整的 TOML 文档**（不是一个 TOML 数组里的元素）。这样每帧可独立解析，
坏帧只影响自己。

## 3. 消息全集 / Messages

方向标记：C→S = 仅 client 发，S→C = 仅 server 发，双向 = 两边都可能。

### 3.1 握手

**`Hello`**（C→S，必须是第一帧）

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | 恒为 `"hello"` |
| `proto_version` | u32 | 是 | 协议版本，当前 `1` |
| `client_version` | string | 是 | `CARGO_PKG_VERSION`，仅用于日志与错误提示 |
| `client_pid` | u32 | 是 | 排障用 |
| `host_key_policy` | string | 是 | `"default"` / `"accept_new"` / `"ignore_changes"`，来自 `--ignore-host-key-changes` |

**`Welcome`**（S→C）

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | `"welcome"` |
| `proto_version` | u32 | 是 | server 的版本 |
| `server_pid` | u32 | 是 | 排障 |
| `server_version` | string | 是 | — |
| `config` | string | 是 | 供 UI 页脚显示"我在看哪份配置" |
| `snapshot` | table | 是 | 首个快照（省一次往返，client 立刻有内容可画） |

版本不匹配：server 发 **`Error`** 后断开（**不**尝试降级，理由见 §8）。

### 3.2 状态

**`Snapshot`**（S→C）

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | `"snapshot"` |
| `revision` | u64 | 是 | 严格单调递增；每次状态变更 +1 |
| `server_now_ms` | u64 | 是 | server 的墙钟（Unix epoch ms），用于算 age |
| `client_count` | u32 | 是 | **驱动"最后一个客户端退出"的风险提示**（见 §9） |
| `shutting_down` | bool | 是 | server 已进入关服宽限，client 应提示"即将停止" |
| `defaults` | table | 是 | `Defaults` 的只读副本（新表单的默认绑定等） |
| `tunnels` | array\<table\> | 是 | 见下；**放最后**（TOML 序列化器会把标量前置，但声明顺序仍按数据习惯放末尾） |

`tunnels[i]`（`WireTunnelView`）：

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `id` | u64 | 是 | `TunnelId` |
| `cfg` | table | 是 | 完整 `TunnelConfig`（已 `serde`，含 `forward_identity` 所需字段） |
| `state` | string | 是 | `"stopped"` / `"starting"` / `"up"` / `"retrying"` / `"failed"` |
| `since_ms` | u64 | state≠stopped/failed 时必填 | 该状态开始时刻（server 墙钟 epoch ms） |
| `attempt` | u32 | 否（仅 retrying） | 第几次重连 |
| `next_at_ms` | u64 | 否（仅 retrying） | 下次重连时刻（server 墙钟） |
| `reason` | table | 否（failed/retrying） | `FailureInfo` 的 `kind` / `short` / `detail` |
| `retries` | u32 | 是 | 累计重连次数 |

**`LogLine`**（S→C）—— **与快照分离的高频通道**

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | `"log"` |
| `tunnel` | u64 | 否 | 缺省 = 进程级日志 |
| `revision` | u64 | 是 | 产生该行的 revision（client 可用它判断"这行比我的快照老"） |
| `lines` | array\<table\> | 是 | 批量，每项 `{level, text, at_ms}` |

分开的理由：一条 `tunnel.line` 是 DEBUG 级、可能每帧几十条；若混进 `Snapshot` 会让状态更新
被日志量拖慢。批量 + 上限（每帧最多 200 行）避免刷屏型隧道把控制通道打满。

### 3.3 命令与应答

**`Command`**（C→S）

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | `"cmd"` |
| `req_id` | u64 | 是 | 客户端自增；用于配对 `Reply` |
| `expect_rev` | u64 | 否 | 乐观并发：server 当前 `revision` ≠ 此值 → 回 `Stale` |
| `op` | table | 是 | 见下（`#[serde(tag="op")]` 内部标记） |

`op` 变体（`#[serde(tag = "op")]`）：

| `op` | 字段 | 前置校验（server 侧重新做） |
|---|---|---|
| `start` | `id` | 隧道存在；`config::validate_tunnel` 过；端口预检（`-L`/`-D`）；host 可解析 |
| `stop` | `id` | 隧道存在 |
| `restart` | `id` | 同 `start` |
| `delete` | `id` | 隧道存在 |
| `start_all` | — | — |
| `stop_all` | — | — |
| `create` | `cfg` | 完整校验 + **去重**（`duplicate_of`，见 `src/app/duplicate.rs`）+ 分配 id |
| `update` | `id`, `cfg` | 同 `create` + id 必须存在于 server 的当前配置 |
| `port_scan` | `id` | 隧道存在且处于 `up`（扫描需要活会话） |

**`Reply`**（S→C）

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | `"reply"` |
| `req_id` | u64 | 是 | 回显 |
| `ok` | bool | 是 | 成功与否 |
| `rev` | u64 | 是 | 应用后的 revision（失败时 = 未变的 revision） |
| `error` | table | 否 | `{kind, short, detail}`，复用 `FailureInfo` 的既有词汇 |
| `rows` | array\<table\> | 否 | 仅 `port_scan`：`{port, process, proto}` 列表 |

**`Stale`**（S→C）

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | `"stale"` |
| `req_id` | u64 | 是 | 回显 |
| `rev` | u64 | 是 | server 的当前 revision |

client 收到后必须**刷新快照再让用户重试**，不能自动重放——自动重放会让"两个人同时改"变成
"后者静默覆盖"。

**`Bye`**（C→S）

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | `"bye"` |
| `last_exit` | bool | 是 | `true` = 用户已被告知"最后客户端退出将停止全部转发"并确认 |

**`last_exit` 是提议，不是命令**：server 收到后**必须重新计数**，本次离开后 `client_count > 0`
就不许关服。详见 [lifecycle.md](lifecycle.md#最后一个客户端退出--last-client-exit)。

**`Error`**（S→C）

| 字段 | 类型 | 必填 | 语义 |
|---|---|---|---|
| `t` | string | 是 | `"error"` |
| `code` | string | 是 | 稳定 ASCII：`proto_mismatch` / `bad_frame` / `unauthorized` / `server_shutting_down` |
| `message_zh` | string | 是 | 中文可操作说明 |
| `message_en` | string | 是 | 英文对应 |

发完 `Error` 后 server 断开该连接（除 `server_shutting_down`，那是通知性的）。

## 4. 命令校验位置 / Where validation happens

| 校验 | client | server | 理由 |
|---|---|---|---|
| 表单字段格式（端口范围等） | **做**（即时反馈） | **重做** | client 侧是为了手感，不能作为信任依据 |
| 端口占用 | 不做（client 没 `Supervisor`） | **做** | 只有 server 知道真实监听状态 |
| 重复转发 | client 尽力（本地快照可见的） | **做** | 权威判据是 `duplicate_of`，须对完整配置跑 |
| 权限（能否绑 <1024） | 不做 | **做** | 由 server 的实际 `bind` 结果决定 |
| `expect_rev` | 填 | **查** | 见 §3.3 |

**一句话**：client 的校验是 UX，server 的校验是正确性。两者都要，不能只留一个。

## 5. 时钟与时长 / Clocks

`TunnelStatus` 含 `Instant { since }`（`src/model/status.rs:21-32`），
而 **`Instant` 不能跨进程**——它是单调钟，两个进程的基准点不同。

| 概念 | wire 表达 | client 怎么用 |
|---|---|---|
| 某状态开始时刻 | `since_ms`（server 墙钟 epoch ms） | `age = server_now_ms - since_ms`，之后本地累加 |
| 下次重连时刻 | `next_at_ms`（server 墙钟） | `remaining = next_at_ms - server_now_ms` |
| 快照产生时刻 | `server_now_ms` | age 的计算基准 |

**client 永不使用自己的绝对墙钟。** 具体做法：收到快照时记下
`local_anchor = Instant::now()` 与 `server_now_ms`；之后渲染用
`server_now_ms + (Instant::now() - local_anchor)` 作为"当前 server 时间"。
这样：

- 两台机器的时钟差多少都不影响（只用了差值，且都来自同一个 server）；
- client 本机时钟被 `ntpdate` 跳变也不影响（用的是 `Instant`，单调）；
- 代价：若 client 与 server 之间的传输延迟很大，age 会略有滞后（毫秒级，可忽略）。

**server 墙钟回拨**（系统时间被往回调）：`server_now_ms` 可能小于上一帧，导致 `age` 变成负数。
处理：`age = age.max(0)`，并记 `server.clock_went_backwards`（WARN）。不试图修正，只保证不显示负数。

## 6. TOML 编码限制与规避 / TOML limits

**这是本协议唯一的实现约束来源**（无 `serde_json`，且不许新增依赖）。
以下均已在本机实测（Rust 1.99.1 + `toml` 1.x）：

| # | 现象 | 实测结论 | 协议对策 |
|---|---|---|---|
| 1 | TOML 无 `null` | `Option<T> = None` 时**字段整体省略**；反序列化回 `None` | 「字段缺失」= None，语义写进 §3；不要用 `null` |
| 2 | 嵌套 `Option<Option<T>>` | **有损**：`Some(None)` 与 `None` 都编码成空串，无法区分 | **协议禁止嵌套 Option**，Option 只用一层 |
| 3 | 标量在表之后 | **手写** TOML 把标量写在表后面 → `TOML parse error`；但**序列化器会自动把标量排到表前** | 两端都走序列化器，**绝不手拼 TOML 文本** |
| 4 | 数组 vs 表混排 | 序列化器自动排序，round-trip 安全 | 同上，声明顺序不影响 |
| 5 | 未知字段 | 默认**被忽略**（正向兼容天然成立）；`deny_unknown_fields` 才会拒绝 | 默认忽略 = 新 server 加字段不炸旧 client；**不加** `deny_unknown_fields` |
| 6 | 顶层内部标记枚举 | `#[serde(tag="t")]` 可**直接**序列化到顶层（`t = "hello"` + 字段），无需再包一层 | 帧负载 = 直接序列化消息枚举 |

**因此协议 schema 的三条硬规则**：
1. 不用嵌套 `Option`；
2. 不用 `null` 语义（缺失即 None）；
3. 不手拼 TOML（编码只用 `toml::to_string`，解码只用 `toml::from_str`）。

**备选方案**（若 TOML 吞吐成为瓶颈）：行式 `key=value` 手写 codec，复用 `src/logging/format.rs`
已验证的转义纪律（`token()` / `push_value()`，注意它们是 `pub(super)`，生产化需提升可见性）。
**本 PR 选 TOML**：19 KB 快照在 UDS 上编码耗时远小于一帧 200ms 的预算，先简单优先。

## 7. 体积预算 / Size budget

实测（spike，50 条隧道、完整字段）：

| 项 | 字节 |
|---|---|
| 极简 50 条（仅 `id`） | 1052 |
| **完整 `Snapshot` 50 条** | **≈19 444**（约 19 KB） |
| 单条 `LogLine` 批量帧 | < 2 KB |

**结论**：全量快照可行。一条变更广播约 19 KB，即使每秒 10 次变更也只有 190 KB/s 走本机 UDS
（实测本机 UDS 吞吐在 GB/s 量级），远低于任何瓶颈。**用全量换来"不会 desync"是划算的。**

规模化上限：按线性外推，1000 条隧道约 390 KB，仍在 1 MiB 帧上限内但接近；
超过约 2500 条会撞上限 → 那时应改用 delta。**在当前工具的规模（几十条）完全不必要。**

## 8. 版本协商与错误 / Versioning & errors

| 情况 | server 行为 |
|---|---|
| `proto_version` 不匹配 | 发 `Error{code:"proto_mismatch"}` → 断开。**不降级** |
| 首帧不是 `Hello` | 发 `Error{code:"bad_frame"}` → 断开 |
| 帧长 > 1 MiB 或 `len == 0` | 直接断开（**不发** `Error`：无法信任后续字节边界） |
| TOML 解析失败 | 发 `Error{code:"bad_frame"}` → 断开 |
| `expect_rev` 不符 | 发 `Stale`（不断开） |

**为什么不降级**：client 与 server 是同一台机器上的同一个二进制（`sshpf` 既是 client 也是 server），
版本不一致只可能发生在"升级过程中旧 client 还活着"。此时**拒绝并说清楚**比"尽量兼容"安全得多——
半兼容的协议会产生"某些操作静默无效"这类最难查的故障。错误文案要能指导动作：

```text
客户端与后台服务协议版本不一致（客户端 1，服务 1 未满 2）。请退出所有 sshpf 后重新启动。
The client and the background server speak different protocol versions (client 1, server 2).
Quit every sshpf instance, then start again.
```

## 9. 与「最后一个客户端退出」的关系

协议侧的职责很小但关键：

| 消息 | 作用 |
|---|---|
| `Snapshot.client_count` | client 用它决定**要不要弹风险提示**（`==1` 且存在运行中转发才弹） |
| `Bye.last_exit` | 告诉 server「用户已确认过后果」，**只是建议** |
| `Snapshot.shutting_down` | server 已进入宽限期 → 所有 client 都能看到「即将停止」 |

**协议必须保证的事**：`client_count` 来自 server 自己的计数，不是 client 上报的数字。
client 上报的计数不可信（可过期、可伪造），而"谁能停止所有人的转发"必须由 server 判断。

## 10. 慢消费者 / Slow consumers

| 机制 | 值 | 说明 |
|---|---|---|
| 每会话发送队列 | **有界，容量 8 帧** | 队列满即判定为慢 |
| 队列满的处理 | 断开该 client，记 `client.slow_consumer`（含 `dropped` 计数） | **丢弃最新帧**：宁可断也不无限缓冲 |
| 广播是否阻塞 | **绝不** | 广播只 `try_send`；失败即标记断开，绝不 `send().await` |
| 断开后 | server 继续服务其它 client | 一条慢连接不能拖死全局 |

**为什么断开而不是丢帧**：丢帧会让该 client 的快照永久落后于 `revision`
（因为快照是全量的，丢一帧就等于丢一次状态转移）；它再收到下一帧又会觉得自己"跳过了"。
成员设计上：既然全量快照能自愈，理论上丢帧也能恢复（下一帧就是完整状态）——
**但前提是它还在读**。队列满说明它**根本读不动**，丢帧只会让队列继续满，最后必然要断。
所以直接断，让 client 重连（重连会重新拿全量快照，自然自愈）。

## 11. 未决 / Open

| 问题 | 状态 |
|---|---|
| 路径派生用「转义路径」还是 hash | 建议先转义路径（可读、无依赖），见 §1.1 |
| 1 MiB 帧上限是否足够 | 几十条隧道够；上千条需改 delta |
| TOML 编码吞吐 | 未实测到瓶颈；若成为瓶颈改行式 codec |
| Windows | 无 UDS；需 named pipe（std 无 API）或 loopback TCP + token，本 PR 不做 |
| 同 uid 进程的认证 | 明确不做（需 `unsafe` 调 `getsockopt`，违反仓库硬约束） |
