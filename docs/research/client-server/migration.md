# 迁移实施计划 / Migration plan（调研稿）

> **这份文档回答**：改成 client-server 要动哪些文件、按什么顺序落地、每步怎么验证不改行为、出问题怎么回滚；**§11 是"最后一个客户端退出"这个交付项的完整验收定义**。
> **不负责**：协议字段（见 [protocol.md](protocol.md)）、进程生命周期（见 [lifecycle.md](lifecycle.md)）、方案为什么这样（见 [../client-server.md](../client-server.md)）。
> **相关代码路径**：当时作为施工目标的 `src/main.rs`、`src/cli.rs`、`src/app.rs`、`src/app/confirm.rs`；现在已落地为 `src/ipc/`、`src/server/`、`src/client/`。

> 历史施工图（2026-11）。**行数与文件名多为当时的估算**，实际实现已与之分化（例如现在叫
> `src/client/`、`src/server/`，传输层是回环 TCP）。本文保留的价值：分阶段纪律、不变量清单、
> 以及 §11 的验收条款。当前状态见 [../../client-server.md](../../client-server.md)。

## 0. 不变量

| 不变量 | 来源 | 怎么守 |
|---|---|---|
| 生产代码不许 `.unwrap()/.expect()/panic!` | `AGENTS.md` 规则 2 | 新文件一律返回 `Result`/`Option`；`safety_guards` 自动扫描 |
| `unsafe` 只在崩溃写入器 | 规则 3 | IPC 层不需要 `unsafe`（`File::try_lock`/`TcpStream` 都在安全 API 内） |
| 依赖只许向下 | [../../architecture.md](../../architecture.md) | `ipc` 插在 `tunnel` 与 `app` 之间，不向上依赖 |
| 单文件 ≤400 行、单函数 ≤60 行 | `tests/safety_guards.rs` | 每个新文件给预估行数，超 400 必拆 |
| 不新增依赖 | [../../agents.md](../../agents.md) | 只用 std + 已有 `serde`/`toml`/`tokio`/`anyhow` |
| 不改变可观察行为 | 规则 6 | §7 逐项核对；分阶段落地，每阶段 CI 绿 |
| 不让 `process::exit` 跳过 `Drop` | [../../runtime.md](../../runtime.md) | 关服走 `return`，见 [lifecycle.md](lifecycle.md) §5 关服顺序 |

## 1. 模块地图（当时的三层）

| 层 | 职责 | 关键约束 |
|---|---|---|
| `src/ipc/` | **纯协议**：帧、消息、快照、codec、端点 | 不 import `app`/`tunnel`/`ui`/`server`/`client`；只依赖 `model` 的纯数据与叶子层；无线程、不写配置、不 panic |
| `src/server/` | 状态所有者：选举、会话、广播、命令执行、事件泵、生命周期 | 是"新的 app"，可依赖下层 |
| `src/client/` | 视图与交互：收快照、revision 去重、`Instant` 重建、命令映射、重连 | **不**依赖 `tunnel`/`ssh`（它不建连） |

**为什么 `ipc` 必须单独一层**：`server` 与 `client` 双向依赖它；若它依赖 `app`/`tunnel` 就会成环，
违反"只许向下"，且协议无法被单独测试。

**`snapshot` 的取舍**：另写 wire 类型（`WireTunnel`/`WireStatus`）而非复用内部类型——
`TunnelStatus` 含 `Instant` 无法序列化，且 wire 一旦等于内部结构，任何内部重构都会变成协议破坏。

## 2. `App` 拆分（逐字段判据）

判定标准：**它是"转发的事实"还是"我这一屏的样子"**。

| 归属 | 字段 |
|---|---|
| **server**（转发事实） | 配置、`Paths`、权威 `tunnels`、`Supervisor`、事件通道、重连退避状态、退出分类（`pending_exit`/`pending_reason`/`saw_listening`）、`revision`、端口扫描代次 |
| **client**（这一屏） | `selected`、`overlay`、`form`、`picker`、`status`、`should_quit`、滚动位置、`filter`、`started_at`、`select_mode`、`hits`、`import_error`、`pending_port_skipped` |
| **两边都要** | `tunnels`（server 权威 / client 镜像）、`hosts`（server 用于校验 / client 用于下拉）、`host_key_policy`（server 建连 / client 透传） |
| 特殊 | 端口扫描：client 持有**渲染状态**，**server 执行**（它才有 `Supervisor` 与 SSH 会话）；这是单 client 定向应答，**不进广播快照** |

**关键取舍**：`App.tunnels` 这个名字保留，内容来自快照——这样 `src/ui/**` 与 `src/keys/**` 的读取点
**完全不用改**，把改动面压到最小。改名是纯审美收益，却要动整个渲染层。

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

| 新依赖 | 允许？ | 说明 |
|---|---|---|
| `ipc` → `model`（仅纯数据） | 是 | wire 化副本；**不 import `app`/`tunnel`/`ui`** |
| `server` → `ipc`, `tunnel`, `config`, `ssh`, 叶子 | 是 | server 是"新的 app"，可依赖下层 |
| `client` → `ipc`, `config`, 叶子 | 是 | client **不**依赖 `tunnel`/`ssh`（它不建连） |
| `app`/`ui`/`keys` → `client` 的数据结构 | 需谨慎 | 建议 `client` 只提供快照类型，`App` 直接持有它，避免 `ui` 依赖 `client` |
| `ipc` → `app`/`tunnel`/`ui` | **禁止** | 反向依赖，且会造成 server↔client 的共享层反向调用 |

## 4. 状态同步接线

```text
改造前：隧道线程 ─TunnelEvent→ event_rx ─drain_events→ App.tunnels ─→ ui::draw
改造后：
[server] 隧道线程 ─→ event_rx ─→ pump ─→ ServerState.tunnels ─→ revision += 1
                                    └→ broadcast(Snapshot) ─→ 每会话有界队列 ─→ 帧
[client] 帧 ─→ sync ─→ 丢弃旧 revision ─→ App.tunnels（镜像）─→ ui::draw
```

**`pump` 必须复用既有 `drain_events` 的时序纪律**（排空 → 回收退出竞态 → 分类延迟退出 →
触发到期重试 → 状态消息过期），只是把"直接改 UI 状态"改成"改权威状态并递增 revision"。
[../../runtime.md](../../runtime.md) 的 5 步顺序与 2 条时序规则（`reason` 随 `Exited` 传、
`was_up` 决定 WARN/ERROR）必须逐条保留——这是迁移中最需小心的一处。

## 5. 分阶段落地

每阶段独立可合并、CI 必绿、行为可验证。

| 阶段 | 内容 | 验证 | 回滚 |
|---|---|---|---|
| 1 `ipc` 层 | 新增纯协议模块，**没有业务代码使用它** | 协议 round-trip、帧长度边界、超长帧拒绝、版本不匹配文案；`--lib` + `safety_guards` 绿 | 删掉该目录与登记行 |
| 2 `App` 拆分 | 引入权威状态类型，字段搬过去；**不引入 socket** | **测试数量守恒**：`cargo test --lib -- --list` 前后只增不减 | 还原字段（一次提交） |
| 3 server + 单 client | 真 IPC；client 变成"连接 → 收快照 → 渲染" | 真起 server + 单 client；断言 client 显示 == server 状态；`--check`/`--list-hosts` 不变 | 保留**临时** `--legacy-single-process`，阶段 5 删 |
| 4 多 client + 选举 + 生命周期 | 选举、广播、宽限、**§11 退出提示（独立交付项）** | 8 进程选举唯一性、多 client 一致性、`Bye` 竞态、慢消费者断开 | 只撤回 `count_prompt` 新分支（server 复核保留） |
| 5 清理与文档 | 删临时开关；更新架构/索引/README/CHANGELOG | 全量 CI（fmt / clippy -D warnings / 各 test job） | revert |

**这不是一个 PR 能安全完成的事**：阶段 2 触及大文件与全部测试夹具，阶段 4 竞态密集。

## 6. 测试策略

| 类型 | 放哪 | 覆盖 |
|---|---|---|
| 协议单测 | `src/ipc/` 内 `#[cfg(test)]` | round-trip、边界长度、未知字段、版本不匹配 |
| 快照合并单测 | client 同步层 | revision 单调、旧快照丢弃、乱序到达 |
| 选举集成 | `tests/client_server.rs`（新） | 8 进程并发抢锁唯一性；`SIGKILL` 后残留恢复 |
| 多 client 一致性 / 生命周期 | 同上 | 同一 revision；最后 client 退出 → server 退出；宽限内新 client → 取消 |
| 退出提示 | `src/app/confirm.rs` 单测 + 集成 | 文案含"最后一个客户端"与条数；`count>=2` 时不提示 |
| 既有回归 | 原样保留 | `--lib`、`help_coverage`、`safety_guards`、`logging`、`panic_hardening`、`e2e` |

**复用既有手法**：`tests/logging.rs` 用 `env!("CARGO_BIN_EXE_sshpf")` 起真实二进制、用 `SSHPF_*`
环境变量隔离。新集成测试同样用 `SSHPF_CONFIG` 指 temp 目录，另把端点目录也隔离到 temp——
**否则测试会互相抢锁**（全局运行时目录只有一个）。

**竞态可复现**：不靠 `sleep` 猜时序。让 server 在测试构建下暴露暂停点
（`SSHPF_TEST_PAUSE_AT=before_bye_apply`），测试等它到达再注入第二个 client——
与既有"用环境变量驱动故意崩溃的子进程"是同一手法。

## 7. 不改变可观察行为

| 项 | 是否变 | 说明 |
|---|---|---|
| `event=` 名 | **不变** | 既有事件一个不改；新增只在 `server.*` / `client.*` 命名空间 |
| 键位 | **不变** | 全部保留 |
| 帮助文案 | 少数新增 | 新增 `--server`/`--keep-server` 说明；Safety 节加一句"最后一个 client" |
| 渲染文本 | **不变** | 表格列、状态符号、页脚提示逐字保留 |
| 退出确认 | **变（需求要求）** | 仅"最后一个 client 且有运行中转发"时文案增强；其它情况逐字不变 |
| `--check`/`--list-hosts` | **不变** | 不连 server |
| 配置文件格式 | **不变** | `config.toml` / `state.toml` 版本与字段不变 |

## 8. 风险与回滚

| # | 风险 | 影响 | 缓解 | 回滚 |
|---|---|---|---|---|
| 1 | `App` 拆分裂伤既有逻辑 | 高 | 只搬字段不搬逻辑；测试数量守恒；`ui`/`keys` 不动 | revert 该阶段 |
| 2 | 多一层 IPC 让排障变难 | 中 | `server.*`/`client.*` 事件；`--server` 前台可见日志 | 临时开关 |
| 3 | 最后 client 退出 = 全停，用户困惑 | **高** | 确认框明说"最后一个客户端"；`--keep-server`；宽限窗口；文档 | 纯 UI/文案，无需回滚 |
| 4 | Windows 传输层 | 中 | 当时列为"不做"；实际用回环 TCP 解决 | — |
| 5 | 慢 client 拖死 server | 中 | 每会话有界队列 + 超阈值断开（spike 实测） | 断开即可 |
| 6 | 测试互相抢锁导致 flaky CI | 中 | 配置与端点目录都隔离到 temp | — |
| 7 | 端口扫描挪到 server 后表单体验变化 | 低 | 定向应答不走广播 | 回退到"不支持扫描"降级 |
| 8 | `Instant` 在 wire 上失真 | 低 | `since_ms + server_now_ms` 预算 age（spike 实测） | — |

## 9. 工作量（当时的估算）

| 阶段 | 规模 | 依据 |
|---|---|---|
| 1 `ipc` 层 | **M** | 6 文件约 760 行 + 单测；无业务逻辑，但 codec 边界易错 |
| 2 `App` 拆分 | **L** | 触及 1394 行文件与全部测试夹具；纯结构改动但回归面最大 |
| 3 server + 单 client | **L** | 新 server 约 1330 行 + client 同步；真 IPC 调试成本高 |
| 4 多 client/选举/生命周期 | **L** | 竞态密集；集成测试最难写（最不确定的一块） |
| 5 清理文档 | **S** | 删代码 + 改文档 |

## 10. 不做

| 不做 | 原因 |
|---|---|
| 后台 daemon / systemd user service | 与"最后 client 退出即停"的需求冲突，另立需求 |
| 跨主机 / 非回环监听 | 会引入认证与加密需求，安全边界完全不同 |
| 传输层加密 | 文件权限（0700/0600）+ 令牌已是本机同 uid 的边界；加密收益低 |
| 多 server 间同步 | 身份按 `config_path` 隔离，多个 server 是不同配置 |
| 替换 SSH 层 | 与本改动正交，零收益 |

## 11. 最后一个客户端退出 / Last client exit

需求原文：「最后一个 client 退出时，给出风险提示，所有端口转发都要停止啦！」语义要：关掉一个终端
= 停掉全部转发（含别的历史 client 启动的），而用户以为自己只关了一个窗口。

### 11.1 判定条件与**计数来源**（三条同时成立才提示）

| 条件 | 取值来源 | 不成立时 |
|---|---|---|
| `client_count == 1`（本次退出后归零） | **client 本地快照的 `client_count`** | 仍有别的 client → **不提示**（D11 的"确认疲劳"教训） |
| 存在运行中转发（`Up`/`Starting`/`Retrying` 计数 > 0） | 快照 `tunnels` 的状态字段 | 全停 → 不额外提示 |
| 用户按的是 `q` / `ctrl+c` | 键分发 | 崩溃 / `SIGKILL` 根本无法提示，见 11.4 |

> ⚠️ **`{n}` 必须来自快照的计数，不能读本地 `Supervisor::running_count()`**——
> 那正是改造前 `count_prompt` 的写法。client 已不再持有 `Supervisor`，读它会静默显示 0
> （`src/app/confirm.rs` 里那条注释讲的就是这件事）。漏改会在编译期暴露（好）或静默显示 0（坏）。

### 11.2 确认文案落点

| 文件 | 现在 | 改成 |
|---|---|---|
| `src/app/confirm.rs` 的 `Quit` 分支 | `还有 {running} 条转发在运行，退出将全部停止。` | 新增"最后一个客户端"分支（仅 `client_count==1` 时命中） |
| 确认框帮助说明 | 只有 y/n 两行 | 加 `HelpItem::note` 说明退出最后一个客户端会停止全部转发 |
| 安全规则节 | "需要确认：…、退出。" | 补一句"最后一个客户端退出会停止所有转发" |
| `docs/keymap.md` §安全规则 | 同上列表 | 同步同一句 |

文案草案（**中文在前、英文在后**，走 `help::bi`；逐字可测）：

- zh: `这是最后一个客户端：退出后所有端口转发都会停止（{n} 条运行中）。确定退出？`
- en: `Last client: quitting stops every port forward ({n} running). Quit anyway?`

### 11.3 server 侧复核（权威性，绝不许 client 定夺）

1. client 确认后发 `Bye{last_exit:true}`——语义只是"用户已被告知后果并确认"，**是提议不是命令**。
2. server 收到 `Bye` 后**先减计数再判定**：`> 0` → 不退出，只回应答；`== 0` → 才进关服。
3. **禁止**用 `last_exit` 直接触发关服。理由：过期视图拿到破坏权，等于把"信息一致"这个前提取消。
4. server 若判定"还有别人"，回 `Reply{ok:true, rev, remaining}`，client 只写事件日志（不弹框）
   ——用户已明确表达退出意图，不该被扣住。**已裁决：不新增 `ByeAck`**，`Reply` 加可选 `remaining`。

### 11.4 异常退出：无法提示，走宽限窗口

崩溃 / 被 `SIGKILL` / 终端被杀时没有任何机会弹框。**必须承认这个不对称：最后一个终端崩溃 =
全部转发静默停止。** 两项都做，不是二选一：

| 措施 | 说明 |
|---|---|
| 宽限窗口提到 **5000ms 可配**（`Defaults` 新字段 `shutdown_grace_ms`） | 崩溃后仍有 5s 重开终端、连回 server；期间新 client 连入即取消计时 |
| `--keep-server` 逃生阀 | 显式文档化为「与需求相反的可选项」 |

两条路径的宽限语义必须分开：

| 路径 | 触发 | 宽限 |
|---|---|---|
| 主动确认退出 | `Bye{last_exit:true}` + server 复核 `count==0` | **短**（约 500ms，仅覆盖手滑） |
| 异常断开 | 端点 EOF，无 `Bye` | **完整** 5000ms（`shutdown_grace_ms` 可配），期间 `count>0` 即取消 |

### 11.5 该交付项的测试与验证（阶段 4 的独立验收点）

| 测试 | 断言 |
|---|---|
| 文案精确性 | `client_count==1` 且 2 条运行中 → prompt 同时含"最后一个客户端"与 `2`；`count==2` → 文案与改造前逐字相同 |
| 无运行中转发 | `count==1` 且全停 → 不含"最后一个客户端" |
| 竞态：自认最后 | A 收到 `count==1` 快照 → B 在 `Bye` 到达前连上 → **server 不退出**（用 §6 的暂停点构造交错） |
| 2 退 1 | 2 client，退 1 → server 仍在；最后 1 个 `Bye` → server 退出并删端点 |
| 宽限取消 | 最后 client `SIGKILL` → 宽限内新 client 连入 → server 不退出 |
| 帮助同步 | `tests/help_coverage.rs` 仍绿（只加 note 行，不影响键位双向比对） |
| 事件可观测 | 关服前后有 `server.*` 记录，字段带 `remaining=`、`grace_ms=` |

**不变量**：本项**不新增任何键位**，`q`/`ctrl+c` 的 dispatch 不变。这也是"能否安全落地"的判据：
若要新增键，说明设计跑偏了。