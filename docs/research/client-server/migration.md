# 迁移实施计划 / Migration plan

> **这份文档回答**：把现在的单进程 `sshpf` 改成 client-server，**具体动哪些文件、按什么顺序落地、
> 每步怎么验证不改行为、出问题怎么回滚**。这是可执行的施工图，不是愿景。
> **不负责**：协议字段（见 [protocol.md](protocol.md)）、进程生命周期（见 [lifecycle.md](lifecycle.md)）、
> 方案为什么这样（见 [../client-server.md](../client-server.md)）。
> **相关代码路径**：本 PR 只写文档与 spike，**不改下面提到的任何 `src/` 文件**；
> 表格里的路径是将来施工时要动的目标。

## 0. 前提与不变量

| 不变量 | 来源 | 本计划如何守住 |
|---|---|---|
| 生产代码不许 `.unwrap()/.expect()/panic!` | `AGENTS.md` 规则 2 | 新文件一律返回 `Result`/`Option`；`safety_guards` 自动扫描 |
| `unsafe` 只在 `src/logging/crash.rs` | 规则 3 | IPC 层不需要 `unsafe`（`File::lock`/`UnixStream` 都在安全 API 内） |
| 依赖只许向下 | `docs/architecture.md` | §3 逐条核对；`ipc` 插在 `tunnel` 与 `app` 之间 |
| 单文件 ≤400 行、单函数 ≤60 行 | `tests/safety_guards.rs` 断言 | §1 每个新文件给预估行数，超 400 必拆 |
| 不新增依赖 | `docs/agents.md` | 只用 std + 已有 `serde`/`toml`/`tokio`/`anyhow` |
| 不改变可观察行为 | 规则 6 | §7 逐项核对；分阶段落地，每阶段 CI 绿 |
| 不让 `process::exit` 跳过 `Drop` | `docs/runtime.md` | §4 明确 return 路径 |

## 1. 新增模块地图 / New module map

新增三层：`ipc`（纯协议，无业务）、`server`（状态所有者）、`client`（视图与交互）。

### `src/ipc/` — 纯协议层（不 import `app`/`tunnel`/`ui`）

| 文件 | 放什么 | 谁在用 | 预估行数 |
|---|---|---|---|
| `ipc.rs` | 模块地图 + `pub use` + `PROTO_VERSION` | 根 `lib.rs` | ~40 |
| `ipc/frame.rs` | 长度前缀读写、最大帧长上限、超限即断 | `server`、`client` | ~120 |
| `ipc/msg.rs` | `ClientMsg` / `ServerMsg` 枚举（`#[serde(tag="t")]`） | 两端 | ~180 |
| `ipc/snapshot.rs` | wire 版快照类型（`WireTunnelView` 等，`Instant`→`ms`） | 两端 | ~150 |
| `ipc/codec.rs` | 编码/解码 + 版本协商 + 可操作错误文案 | 两端 | ~140 |
| `ipc/endpoint.rs` | `config_path` → `socket_path`/`lock_path` 推导（**转义路径而非 hash**）、目录 0700、socket 0600 | `server`、`client` | ~130 |

**与基线的一处偏差**：`/tmp/cs-contract.md` §4.2 写的是 `hash(config_path)`，`protocol.md` §1.1 的结论是
**用转义路径当文件名**（可读、无依赖、无需 hash 实现）。本计划采用后者；差异已报 Lead。

**为什么单独一层**：`ipc` 被 server 与 client **双向**依赖；若它依赖 `app`/`tunnel` 就会成环，
违反"只许向下"。它只能依赖叶子层的**纯数据**，或自带 wire 类型（取舍见 §3）。

### `src/server/` — 状态所有者

| 文件 | 放什么 | 预估行数 |
|---|---|---|
| `server.rs` | 模块地图 + `run()` 主循环骨架 | ~90 |
| `server/election.rs` | 抢锁、残留 socket 识别、detached spawn 协调 | ~160 |
| `server/session.rs` | 单 client 会话：握手、读命令、有界发送队列 | ~200 |
| `server/broadcast.rs` | revision 递增、向所有会话广播快照、慢消费者断开 | ~170 |
| `server/commands.rs` | 命令执行：**server 侧重新校验** + 调 `App` 既有方法 | ~250 |
| `server/pump.rs` | 复用现有 `App::drain_events` 逻辑，产出 `Snapshot` 而非直接更新 UI | ~180 |
| `server/lifecycle.rs` | `client_count`、宽限窗口、关服顺序 | ~160 |
| `server/state.rs` | `ServerState`：原 `App` 的"权威半边"字段 | ~120 |

`server/state.rs` 若超 400 行，按「隧道相关 / 连接相关」再拆两文件。

### `src/client/` — 视图与交互

| 文件 | 放什么 | 预估行数 |
|---|---|---|
| `client.rs` | 模块地图 + `run()` + 重连状态机 | ~150 |
| `client/sync.rs` | 接收快照、revision 去重、`Instant` 重建 | ~160 |
| `client/commands.rs` | UI 动作 → `ClientMsg` 映射 + `expect_rev` 填充 | ~150 |
| `client/confirm.rs` | **最后一个 client 退出的风险提示**判定与 `Bye{last_exit}` | ~90 |

### 改动既有文件

| 文件 | 改动 | 风险 |
|---|---|---|
| `src/main.rs` | 分派模式：client / `--server` / `--check` / `--list-hosts` | 中（入口） |
| `src/cli.rs` | 新增 `--server`、`--keep-server`，同步 `USAGE` 双语 | 低 |
| `src/app.rs` | **拆分**：`App` 保留交互态；权威态搬到 `ServerState` | **高（1394 行）** |
| `src/app/runtime.rs` | `restore_enabled` 只在 server 侧调用 | 中 |
| `src/app/events.rs` | 事件泵产出 `Snapshot` 的转换点 | 中 |
| `src/app/confirm.rs` | `Quit` 文案区分"最后一个客户端"（§11.2） | 低 |
| `src/help/dialogs.rs`（`CONFIRM`）+ `src/help/notes.rs`（`SAFETY`）+ `docs/keymap.md` | 同步确认框说明与安全规则 | 低，`help_coverage` 守着 |
| `src/lib.rs` | 登记 `ipc`/`server`/`client` | 低 |
| `docs/architecture.md`、`docs/index.md` | 分层图与索引更新 | 低 |

## 2. `App` 拆分清单（逐字段）

对着 `src/app.rs:129-186` 的**全部**字段。判定标准：**它是"转发的事实"还是"我这一屏的样子"**。

| 字段 | 归属 | 理由 |
|---|---|---|
| `cfg: ConfigFile` | **server** | 隧道定义的唯一真相；client 只读快照副本 |
| `paths: Paths` | **server** | 只有 server 持久化；client 不需要写盘 |
| `tunnels: Vec<TunnelView>` | **两边** | server 持有权威；client 持有"最近一次快照的镜像"（同类型，便于复用 `ui`） |
| `selected: usize` | client | 光标是每屏各自的 |
| `overlay: Overlay` | client | 浮层是本地交互态 |
| `form: Option<FormState>` | client | 两个 client 可同时开不同表单（需求明确） |
| `picker: Option<HostPicker>` | client | 表单子态 |
| `hosts: Vec<SshHost>` | **两边** | server 读 `~/.ssh/config` 用于校验；client 需要列表渲染下拉 |
| `hosts_error: Option<String>` | client | 渲染用 |
| `status: Option<StatusMsg>` | client | 状态条是本地瞬时消息 |
| `should_quit: bool` | client | 本地意图 |
| `help_scroll` / `detail_scroll` | client | 滚动位置 |
| `filter: String` | client | 每屏各自的过滤 |
| `started_at: Instant` | client | 本地会话时长 |
| `supervisor: Supervisor` | **server** | 隧道线程的所有者，绝不能在 client |
| `event_tx` / `event_rx` | **server** | 隧道事件通道 |
| `retry_state` | **server** | 重连退避是转发事实 |
| `host_key_policy` | **两边** | server 用于建连；client 从 `--ignore-host-key-changes` 透传给 server（`Hello` 字段） |
| `pending_form` | client | 待确认的编辑 |
| `pending_exit` / `pending_reason` | **server** | 退出分类是转发事实 |
| `saw_listening` | **server** | 与 `pending_*` 同源 |
| `select_mode: MultiSelect` | client | 多选标记是本地交互态（但**标记的 id 集合**会被发进命令） |
| `hits: HitMap` | client | 渲染命中图 |
| `import_error: Option<String>` | client | 表单附属 |
| `ports: Option<PortPick>` | client（状态）/**server**（扫描执行） | 见下 |
| `port_rx` | **server** | 扫描线程在 server 才有效（见下） |
| `port_seq` | **server** | 代次防过期结果 |
| `pending_port_skipped` | client | 提交过程中的本地临时量 |

### 端口扫描（`p`）归属：server 执行，client 渲染

扫描需要**已建立的 SSH 会话**（`src/ssh/probe.rs` 走隧道执行 `ss`/`netstat`），client 没有 `Supervisor`，
所以**必须在 server 执行**：client 发 `PortScan{tunnel_id}`，server 回 `PortRow` 列表。这是**单 client
定向应答**，不进广播快照——端口列表是"我正在填这张表单"的中间结果，别人不该看到。
`pending_port_skipped` 留在 client：本次提交被跳过的重复项，属本地上下文。

### 拆分后的结构草案

```rust
// src/server/state.rs —— 权威半边
pub struct ServerState {
    pub cfg: ConfigFile,
    pub paths: Paths,
    pub tunnels: Vec<TunnelView>,        // 权威
    pub supervisor: Supervisor,
    pub event_tx: Sender<TunnelEvent>,
    pub event_rx: Receiver<TunnelEvent>,
    pub retry_state: HashMap<TunnelId, RetryState>,
    pub pending_exit: HashMap<TunnelId, (Option<i32>, Instant)>,
    pub pending_reason: HashMap<TunnelId, Option<FailureInfo>>,
    pub saw_listening: HashSet<TunnelId>,
    pub host_key_policy: HostKeyPolicy,
    pub revision: u64,
    pub port_seq: u64,
    pub port_rx: Option<Receiver<PortScanMsg>>,
}

// src/app.rs —— 交互半边（既有字段名不变，减少 ui/keys 改动）
pub struct App {
    pub tunnels: Vec<TunnelView>,        // 镜像：来自最近一次快照，ui 读取点不用改
    pub client_count: usize,             // 来自快照，驱动 §11 退出提示
    pub connection: ConnectionState,     // Connected / Disconnected / Reconnecting
    pub selected: usize, pub overlay: Overlay, pub form: Option<FormState>,
    pub picker: Option<HostPicker>, pub hosts: Vec<SshHost>,
    pub hosts_error: Option<String>, pub status: Option<StatusMsg>,
    pub should_quit: bool, pub help_scroll: u16, pub detail_scroll: u16,
    pub filter: String, pub started_at: Instant,
    pub select_mode: MultiSelect, pub hits: HitMap,
    pub import_error: Option<String>, pub ports: Option<PortPick>,
    pub pending_port_skipped: Vec<String>, pub host_key_policy: HostKeyPolicy,
    pub pending_form: Option<TunnelConfig>,
}
```

**关键取舍**：`App` 保留 `tunnels` 这个名字还是改成 `view`？
**建议保留 `tunnels: Vec<TunnelView>`**，内容来自快照——这样 `src/ui/**`（约 20 个文件、纯读）
与 `src/keys/**` 的读取点**完全不用改**，把改动面压到最小。改名是纯粹的审美收益，却要动整个渲染层。

## 3. 分层与依赖方向

`ipc` 必须插在 `tunnel` 与 `app` **之间**，且**不向上依赖**：

```text
   main / cli
     ├── app   ui   keys        交互态（本地，读快照）
     └── server   client        权威态 / 同步
            └──────┬──────┘
                 ipc            纯协议：只依赖 model 纯数据 + 叶子层
                   │
             tunnel   config
                   │
                  ssh
                   │
       model text logging sys eventlog
```

逐条核对：

| 新依赖 | 允许？ | 说明 |
|---|---|---|
| `ipc` → `model`（仅纯数据） | 是 | `TunnelConfig`/`TunnelStatus` 的 wire 化副本；**不 import `app`/`tunnel`/`ui`** |
| `server` → `ipc`, `tunnel`, `config`, `ssh`, 叶子 | 是 | server 是"新的 app"，可依赖下层 |
| `client` → `ipc`, `config`, 叶子 | 是 | client **不**依赖 `tunnel`/`ssh`（它不建连） |
| `app`/`ui`/`keys` → `client` 的数据结构 | 需谨慎 | 建议 `client` 只提供 `Snapshot`，`App` 直接持有它，避免 `ui` 依赖 `client` |
| `ipc` → `app`/`tunnel`/`ui` | **禁止** | 反向依赖，且会造成 server↔client 的共享层反向调用 |

**`ipc/snapshot.rs` 的取舍**：建议**另写** `WireTunnelView` 而非复用 `TunnelView`——`TunnelStatus` 含
`Instant` 无法序列化，且 wire 一旦等于内部结构，任何内部重构都会变成协议破坏。约 150 行映射换协议独立演进。

## 4. 状态同步接线

### 改造前（单进程）

```text
隧道线程 ──TunnelEvent──► event_rx ──drain_events──► App.tunnels ──► ui::draw
```

### 改造后

```text
[server 进程] 隧道线程 ─► event_rx ─► pump ─► ServerState.tunnels ─► revision+=1
                                              └► broadcast(Snapshot) ─► 每会话有界队列 ─► UDS 帧
[client 进程] UDS 帧 ─► client/sync ─► 丢弃 revision<=本地 ─► App.tunnels（镜像）─► ui::draw
```

**要点：`pump` 复用既有 `drain_events` 的时序纪律**（① 排空 → ② 回收退出竞态 → ③ 分类延迟退出 →
④ 触发到期重试 → ⑤ 状态消息过期），只是把"直接改 UI 状态"改成"改 `ServerState` 并递增 revision"。
这是迁移中最需小心的一处：`docs/runtime.md#事件泵--event-pump` 的 5 步顺序与 2 条时序规则
（`reason` 随 `Exited` 传、`was_up` 决定 WARN/ERROR）必须逐条保留，不能因为加了一层广播就打乱。

## 5. 分阶段落地 / Phased rollout

每阶段独立可合并、CI 必绿、行为可验证。

### 阶段 1：抽出 `ipc` 层 + 协议 codec（行为零变化）

- 新增 `src/ipc/*`，`src/lib.rs` 登记；**没有任何业务代码使用它**。
- 测试：协议 round-trip、帧长度边界、超长帧拒绝、版本不匹配错误文案。
- 验证：`cargo test --lib`、`safety_guards` 绿；行为完全未变（只是新增未使用模块）。
- 回滚：删掉 `src/ipc/` 与 `lib.rs` 一行。

### 阶段 2：`App` 拆分（仍是单进程）

- 引入 `ServerState`，把 §2 标 **server** 的字段搬过去；`App` 仍在本进程内直接持有它。
- 关键：**不引入 socket**，先只做结构拆分。`ui`/`keys` 读取路径不变。
- 测试：核心验收是**测试数量守恒**——`cargo test --lib -- --list | wc -l` 前后只增不减。
- 回滚：还原 `App` 字段（一次提交的范围）。

### 阶段 3：server 进程 + 单 client（真 IPC）

- `src/server/*` + `src/client/sync.rs`；`main` 分派；client 变成"连接 → 收快照 → 渲染"。
- 测试：真起 server + 单 client 进程；断言"client 显示的状态等于 server 的状态"。
- 验证：`tests/e2e.rs` 既有场景（需 sshd）仍通过；`--check`/`--list-hosts` 行为不变。
- 回滚：保留**临时** `--legacy-single-process`，阶段 5 删除。

### 阶段 4：多 client + 选举 + 生命周期

- `server/election.rs`、`broadcast.rs`、`lifecycle.rs`、`client/confirm.rs`。
- **独立交付项 D4-a：最后一个客户端退出提示（§11）**，单独验收、单独回滚：
  `client_count==1` 时文案增强 + `Bye{last_exit}` + server 复核计数；验收 = §11.5 的 7 条测试。
  回滚 = 只撤回 `count_prompt` 的新分支，退回今天"还有 N 条转发在运行"的文案（server 复核保留）。
- 其余测试：8 进程选举唯一性、多 client 一致性、`Bye` 竞态（自认最后但别人刚连上 → 不关服）、慢消费者断开。
- 验证：`src/help/dialogs.rs` + `src/help/notes.rs` + `docs/keymap.md` 已同步；`help_coverage` 绿。
- 回滚：revert 本阶段其余提交。

### 阶段 5：清理与文档

- 删除 `--legacy-single-process`；更新 `docs/architecture.md`、`docs/index.md`、`docs/runtime.md`、
  `README.md`、`CHANGELOG.md`。
- 验证：全量 CI（`fmt` / `clippy -D warnings` / 4 个 test job）。

## 6. 测试策略

| 类型 | 放哪 | 覆盖什么 |
|---|---|---|
| 协议单测 | `src/ipc/*` 内 `#[cfg(test)]` | round-trip、边界长度、未知字段、版本不匹配 |
| 快照合并单测 | `src/client/sync.rs` | revision 单调、旧快照丢弃、乱序到达 |
| 选举集成 | `tests/client_server.rs`（新） | 8 进程并发抢锁唯一性；`SIGKILL` 后残留恢复 |
| 多 client 一致性 | `tests/client_server.rs` | 2 client 收同一 revision；最终快照逐字段相等 |
| 生命周期 | `tests/client_server.rs` | 最后 client 退出 → server 退出；宽限内新 client → 取消 |
| 退出提示 | `src/app/confirm.rs` 单测 + `tests/client_server.rs` | 文案含"最后一个客户端"与条数；`client_count>=2` 时不提示 |
| 既有回归 | 原样保留 | `--lib`、`help_coverage`、`safety_guards`、`logging`、`panic_hardening`、`e2e` |

**复用既有手法**：`tests/logging.rs` 用 `env!("CARGO_BIN_EXE_sshpf")` 起真实二进制、用 `SSHPF_*`
环境变量隔离。新集成测试同样用 `SSHPF_CONFIG` 指到 temp 目录，另加 `SSHPF_SOCKET_DIR` 把 socket
也隔离到 temp——**否则测试会互相抢锁**（全局 `$XDG_RUNTIME_DIR` 只有一个）。

**竞态可复现**：不靠 `sleep` 猜时序。让 server 在测试构建下暴露暂停点（`SSHPF_TEST_PAUSE_AT=before_bye_apply`），
测试等它到达再注入第二个 client——这与 `tests/logging.rs` 用环境变量驱动"故意崩溃的子进程"是同一手法。

## 7. 不改变可观察行为 / Observable behaviour

| 项 | 是否变 | 说明 |
|---|---|---|
| `event=` 名 | **不变** | 既有事件一个不改；新增只在 `server.*` / `client.*` 命名空间 |
| 键位 | **不变** | `q`/`ctrl+c`/`space`/`r`/`n`/`e`/`d`/`a`/`x`/`v`/`?`/`/` 全保留 |
| 帮助文案 | 少数新增 | 新增 `--server`/`--keep-server` 说明；Safety 节加一句"最后一个 client" |
| 渲染文本 | **不变** | 表格列、状态符号、页脚提示逐字保留（`ui` 几乎不动是 §2 的目的） |
| 退出确认 | **变（需求要求）** | `Quit` 在"最后一个 client 且有运行中转发"时文案增强；其它情况逐字不变 |
| `--check`/`--list-hosts` | **不变** | 不连 server |
| 配置文件格式 | **不变** | `config.toml` / `state.toml` 版本与字段不变 |

## 8. 风险与回滚

| # | 风险 | 影响 | 缓解 | 回滚 |
|---|---|---|---|---|
| 1 | `App` 拆分裂伤 1394 行的既有逻辑 | 高（回归面大） | 阶段 2 只搬字段不搬逻辑；`cargo test --lib` 数量守恒；`ui`/`keys` 不动 | revert 阶段 2 单提交 |
| 2 | 多一层 IPC 让排障变难 | 中 | `server.*`/`client.*` 事件；`--server` 前台可见日志；详情面板保留完整日志环 | 临时开关 `--legacy-single-process` |
| 3 | 最后 client 退出 = 全停，用户困惑 | **高** | 确认框明说"最后一个客户端"；`--keep-server`；宽限窗口；文档 | 无需回滚（纯 UI/文案） |
| 4 | Windows 无 UDS | 中 | 本 PR 不做；文档登记；`cfg` 隔离，Unix 路径不泄漏到跨平台代码 | 不涉及（未实现） |
| 5 | 慢 client 拖死 server | 中 | 每会话有界队列 + 超阈值断开（spike 实测） | 断开即可，无需回滚 |
| 6 | 测试互相抢锁导致 flaky CI | 中 | 全部路径经 `SSHPF_CONFIG`/socket 目录隔离到 temp | — |
| 7 | 端口扫描挪到 server 后表单体验变化 | 低 | 定向应答不走广播，延迟可控；失败文案沿用既有分类 | 回退到"不支持扫描"降级 |
| 8 | `Instant` 语义在 wire 上失真的年龄显示 | 低 | `since_ms + server_now_ms` 预算 age（spike 实测时钟偏置不影响） | — |

## 9. 工作量估算

| 阶段 | 规模 | 依据 |
|---|---|---|
| 1 `ipc` 层 | **M** | 6 文件约 760 行（§1 各行相加）+ 单测；无业务逻辑，但 codec 边界易错 |
| 2 `App` 拆分 | **L** | 触及 1394 行文件与全部测试夹具；纯结构改动但回归面最大 |
| 3 server + 单 client | **L** | 新 `server/*` 约 1330 行（§1 相加）+ client 同步；真 IPC 调试成本高 |
| 4 多 client/选举/生命周期 | **L** | 竞态密集；集成测试难写（本计划最不确定的一块），含 §11 退出提示 |
| 5 清理文档 | **S** | 删代码 + 改文档 |

**诚实判断**：这不是一个 PR 能安全完成的事。建议按阶段 1/2/3/4/5 分 5 个 PR 合并。
本 PR 只交付**调研 + spike + 计划**，不包含任何 `src/` 改动。

## 10. 不做 / Out of scope

| 不做 | 原因 |
|---|---|
| Windows（named pipe / loopback TCP） | `std` 无 named pipe API；需单独设计认证与发现 |
| 后台 daemon / systemd user service | 与"最后 client 退出即停"的需求冲突，另立需求 |
| 跨主机 / 远端 TCP 监听 | 会引入认证与加密需求，安全边界完全不同 |
| UDS 加密 | 文件权限（0700/0600）已是本机同 uid 的边界；加密收益低、复杂度高 |
| 多 server 间同步 | 身份按 `config_path` 隔离，多个 server 是不同配置，不是同一份状态 |
| 替换 `russh` / 改 SSH 层 | 与本改动正交，零收益 |

## 11. 最后一个客户端退出 / last client exit

需求原文：「最后一个 client 退出时，给出风险提示，所有端口转发都要停止啦！」语义上的要害是
**关掉一个终端 = 停掉全部转发（含别的历史 client 启动的）**，而用户以为自己只关了一个窗口。

### 11.1 判定条件（三条同时成立才提示）

| 条件 | 取值来源 | 不成立时 |
|---|---|---|
| `client_count == 1`（本次退出后归零） | client 本地快照的 `client_count` | 仍有别的 client → **不提示**，D11 的「确认疲劳」教训 |
| 存在运行中转发（`Up`/`Starting`/`Retrying` 计数 > 0） | 快照 `tunnels` 的状态字段 | 全停 → 不额外提示 |
| 用户按的是 `q` / `ctrl+c` | `src/keys/main.rs:26`、`src/keys.rs:81` | 崩溃 / `SIGKILL` 根本无法提示，见 11.4 |

### 11.2 确认文案落点

**`ConfirmAction::Quit` 的文案在 `src/app/confirm.rs` 的 `count_prompt`（约 147-157 行）**，
不在 `src/help/dialogs.rs`——后者是 `?` 帮助浮层里「确认框」一节的**说明文字**，两处都必须改：

| 文件 | 现在 | 改成 |
|---|---|---|
| `src/app/confirm.rs` `count_prompt` 的 `Quit` 分支 | `还有 {running} 条转发在运行，退出将全部停止。` | 新增"最后一个客户端"分支（仅 `client_count==1` 时命中），见下 |
| `src/help/dialogs.rs` `CONFIRM` | 只有 y/n 两行 + "其它按键不生效" | 加 `HelpItem::note` 说明退出最后一个客户端会停止全部转发 |
| `src/help/notes.rs` `SAFETY`（111-113 行） | "需要确认：…、退出。" | 补一句"最后一个客户端退出会停止所有转发" |
| `docs/keymap.md` §安全规则（371-374 行） | 同上列表 | 同步同一句 |

文案草案（**中文在前、英文在后**，走 `help::bi`；逐字可测）：

- zh: `这是最后一个客户端：退出后所有端口转发都会停止（{n} 条运行中）。确定退出？`
- en: `Last client: quitting stops every port forward ({n} running). Quit anyway?`

`{n}` = 快照里 `Up+Starting+Retrying` 的计数（复用 `App::counts()`，`src/app/query.rs:93`）。
**不能用 `self.supervisor.running_count()`**——那正是现在 `count_prompt` 的写法（`confirm.rs:148`）；client
已不再持有 `Supervisor`，这是 §2 拆分后必须一起改的点，漏了会在编译期暴露（好）或静默显示 0（坏）。

### 11.3 server 侧复核（权威性，绝不许 client 定夺）

client 的 `client_count` 来自快照，可能已过期：它以为自己是最后一个，期间另一个 client 刚连上。因此：

1. client 确认后发 `Bye { last_exit: true }`——语义只是「用户已被告知后果并确认」，**是提议不是命令**。
2. server 收到 `Bye` 后**先减计数再判定**：`client_count > 0` → 不退出，只回应答；`== 0` → 才进关服流程。
3. **禁止**用 `last_exit` 直接触发关服。理由：过期视图拿到破坏权，等于把"信息一致"这个前提取消。
4. server 若判定"还有别人"，回 `Reply{req_id, ok:true, rev, remaining}`，client 只写 `eventlog`
   （不弹框）—— 用户已明确表达退出意图，不该被扣住。**已裁决：不新增 `ByeAck`**，
   `Reply` 增加可选 `remaining: u32`（[protocol.md](protocol.md) §3.4 与 §9）。

### 11.4 异常退出：无法提示，走宽限窗口

崩溃 / 被 `SIGKILL` / 终端被杀时没有任何机会弹框。**必须承认这个不对称：最后一个终端崩溃 =
全部转发静默停止**。结论（两项都做，不是二选一）：

| 措施 | 说明 |
|---|---|
| 宽限窗口从 2000ms 提到 **5000ms 可配**（`Defaults` 新字段 `shutdown_grace_ms`，本 PR 只设计不实现） | 崩溃后仍有 5s 重开终端、连回 server；期间新 client 连入即取消计时 |
| `--keep-server` 逃生阀 | 显式文档化为「与需求相反的可选项」：最后 client 退出后 server 常驻，需手动 `sshpf --stop-server` 或 `kill` |

两条路径的宽限语义必须分开：

| 路径 | 触发 | 宽限 |
|---|---|---|
| 主动确认退出 | `Bye{last_exit:true}` + server 复核 `count==0` | **短**（约 500ms，仅覆盖手滑；同 `lifecycle.md`） |
| 异常断开 | socket EOF，无 `Bye` | **完整** 5000ms（`shutdown_grace_ms` 可配），期间 `client_count>0` 即取消 |

### 11.5 该交付项的测试与验证（阶段 4 的独立验收点）

| 测试 | 位置 | 断言 |
|---|---|---|
| 文案精确性 | `src/app/confirm.rs` `#[cfg(test)]` | `client_count==1` 且 2 条运行中 → prompt 同时含"最后一个客户端"与 `2`；`client_count==2` → 文案与今天逐字相同 |
| 无运行中转发 | 同上 | `client_count==1` 且全停 → 不含"最后一个客户端" |
| 竞态：自认最后 | `tests/client_server.rs` | client A 收到 `client_count==1` 快照 → B 在 `Bye` 到达前连上 → **server 不退出**（用 §6 的 `SSHPF_TEST_PAUSE_AT` 构造交错） |
| 2 退 1 | `tests/client_server.rs` | 2 client，退 1 → server 仍在；最后 1 个 `Bye` → server 退出并 unlink socket |
| 宽限取消 | `tests/client_server.rs` | 最后 client `SIGKILL` → 宽限内新 client 连入 → server 不退出 |
| 帮助同步 | `tests/help_coverage.rs`（既有，不改） | 键位未变则仍绿；`dialogs.rs`/`notes.rs` 只加 note 行，不影响键位双向比对 |
| 事件可观测 | 日志断言（`tests/logging.rs` 手法） | 关服前后有 `client.bye` / `server.shutdown` 记录，字段带 `remaining=`、`grace_ms=` |

**不变量**：本项**不新增任何键位**，`q`/`ctrl+c` 的 dispatch 不变，`help_coverage` 双向比对不受影响；改动只在文案与判定条件。这也是"能否安全落地"的判据：若要新增键，说明设计跑偏了。
