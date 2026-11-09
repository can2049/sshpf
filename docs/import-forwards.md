# 从 `~/.ssh/config` 发现转发 / Discover forwards

> **这份文档回答**：哪些指令会被列出、作用域怎么累积、`Match`/`Include` 怎么处理、行去重、哪些行列出来却不可导入、扫描失败怎么留痕。
> **不负责**：面板怎么交互（筛选、全选、自动展开）见 [import-pane.md](import-pane.md)；`ctrl+s` 创建出什么见 [import-save.md](import-save.md)；对端端口扫描见 [port-scan.md](port-scan.md)；转发身份去重见 [dedup.md](dedup.md)。
> **相关代码路径**：`src/ssh/forwards.rs`（入口）、`src/ssh/forwards/scan.rs`（作用域/Include）、`src/ssh/forwards/parse.rs`（指令解析）、`src/model/import.rs`（`SshForward`）。

`sshpf` **从不写** `~/.ssh/config`，但会读它。除连接参数（`HostName`/`Port`/`User`/`ProxyJump`/`IdentityFile`）外，还读三组转发指令：

```text
LocalForward   [bind_address:]port  host:hostport
RemoteForward  [bind_address:]port  host:hostport
DynamicForward [bind_address:]port
```

在表单里从主机下拉接受一个主机，这些指令**自动**列成表、**默认全部勾选**，取消不要的再 `ctrl+s`。一条勾选行 = 一条隧道（`TunnelConfig` 只描述一条转发）。

## 为什么不直接用 `ssh2-config` 的现成字段

连接参数用 [`ssh2-config`](https://crates.io/crates/ssh2-config) 0.9.0 解析，但**转发指令不行**——该版本对三者支持不对等：

| 指令 | `ssh2-config` 0.9.0 | 直接用它读会怎样 |
|---|---|---|
| `RemoteForward` | `HostParams::remote_forward: Vec<RemoteForward>`，逐行追加 | 可用 |
| `LocalForward` | 落进 `unsupported_fields` | **丢顺序、丢条数** |
| `DynamicForward` | 同上 | 同上 |

`unsupported_fields` 是 `HashMap<String, Vec<String>>`——**以指令名为 key**。同一个 `Host` 写两条 `LocalForward`，后一条**覆盖**前一条，且 `HashMap` 不保序：

```text
Host cloud_pc
  LocalForward 18789 127.0.0.1:18789
  LocalForward 8090 127.0.0.1:8080
  DynamicForward 1080

unsupported_fields = {"localforward": ["8090", "127.0.0.1:8080"],   ← 18789 静默消失
                      "dynamicforward": ["1080"]}
```

静态丢数据比崩溃更危险。所以 `src/ssh/forwards.rs` **自己按行扫**这三组指令（面向行、引号感知，不是完整 config 解析器），`ssh2-config` 继续只负责连接参数。

## 作用域规则 / Scope

OpenSSH 的转发指令**从所有匹配的 `Host` 块累积**，不是"命中第一个就停"。

| 规则 | 行为 | 为什么必须这样 |
|---|---|---|
| 多个 `Host` 块都匹配 | **全部累积**，按文件顺序 | 只取第一个会让 `Host *` 的公共转发整个消失 |
| `Host *` | 对任何别名都匹配 | 最常见的"公共转发"写法 |
| 通配符 `*` `?` | 支持，**大小写不敏感** | `*.Example.COM` 要能匹配 `api.example.com` |
| 否定子句 `!prod-db` | 命中即**排除整个块**，否定优先 | `Host prod-* !prod-db` 不该给 `prod-db` 开转发 |
| 第一个 `Host` 行**之前**的指令 | 视为全局，等价 `Host *` | OpenSSH 语义：文件头部指令对所有主机生效 |
| `Match` 块 | **永不导入、也不列出** | 见下 |
| `Include` | 跟进，有**总文件数**上限与环检测 | 用户确实会把转发放进 `conf.d/` |

### `Match` 一律不导入 / Match blocks are never imported

`Match` 的转发**带有连接上下文前置条件**。`sshpf` 不建模这些条件，把受 `Match` 门控的转发当成无条件生效，等于在用户明确设了闸的地方静默开监听端口。所以一行都不导入、也不列出——它不是"这个主机声明了什么"，而是"满足某条件时声明了什么"。

实现上 `Scope` 遇到 `match` 就置 `InBlock { applies: false, excluded: true }`，后续指令一律跳过直到下一个 `Host`。`strip_match_blocks`（`src/ssh/config.rs`）在扫描前还会先剔除 `Match` 行及其后缩进行——同一道预处理也保护了连接参数解析（见 [design.md](design.md#d2-sshconfig-由-ssh2-config-解析但必须自己剔除-match-块)）。

两个已知边界，都是**少导入**而非误导入，因此可接受：

- `Match` 块**未缩进**的后续指令不会被 `strip_match_blocks` 摘掉，但 `scan_config` 自己的 `match` 分支已把 scope 关掉，仍不会导入。
- `Include` 进来的文件**也**过 `strip_match_blocks`，所以 include 文件里的 `Match` 同样被剔除。

### `Include`：总文件数预算，不是嵌套深度

```text
MAX_INCLUDE_DEPTH = 8    // 合计最多读 9 个文件（主文件占 1 次）
MAX_PENDING_INCLUDES = 256  // 待读队列上限
```

`forwards_for_host` 用显式栈遍历，`seen_files` 挡住 `a.conf` ↔ `b.conf` 的死循环，`depth` 挡住"每层都是新文件"的无限链，`MAX_PENDING_INCLUDES` 挡住生成式配置（超过 256 个待读文件时记 `sshconfig.include_capped` 并停止展开，UI 线程不被卡住）。

**上限是"总共读几个文件"，不是"嵌套几层"**：`depth` 每弹一个文件就 `+= 1`，与嵌套关系无关。实测 12 个**并列** `Include` 和 20 层**链式** `Include` 都只读 9 个文件。

顺序：遇到 `Include` 把展开的路径**压栈**，当前文件读完才逐个读 ⇒ include 出来的行总排在该文件所有行**之后**，不是出现在 `Include` 那一行的位置——**不要**把它当 OpenSSH 的文本内联语义。路径解析：`~/` 展开到家目录，绝对路径原样，相对路径**相对于当前文件所在目录**；含 `*`/`?` 的做目录内 glob 并按名字排序。

### 一个无害的作用域泄漏（已记录，不修）

`Include` 若写在某个 `Host` 块**内部**，被包含文件里**没有 `Host` 行**的裸指令会被当成全局（每个文件独立从 `Scope::Global` 开始扫，include 目标不知道自己在谁的块里；OpenSSH 会继承外层作用域）：

```text
Host web
  Include inc.conf          # inc.conf: LocalForward 42000 127.0.0.1:1
查询 web  -> [web 自己的转发, 42000]
查询 db   -> [42000]        ← 泄漏
```

这是**多导入**而非漏导入（用户确实写了这条转发，只是没料到它全局生效）。真实配置里几乎不会把 `Include` 放进 `Host` 块，按"先记录、不引入跨文件作用域传递的复杂度"处理。

## 行去重 / Row dedup

```text
key() =  -D  :  D|listen_addr|listen_port
         -L/-R:  {kind:?}|listen_addr|listen_port|target_host|target_port
```

**不含** `host_alias` 与行号——同一条转发被两个块声明，物理上就是同一个转发。去重在**输出阶段**做，保留**第一次出现**的那条（行号因此指向更具体的块）。

防的失败模式很具体：`Host web` 和 `Host *` 各写一遍，去重后只剩 1 行；否则两条隧道抢同一监听端口，第二条必然 `bind: Address already in use`。

**key 是字面比较，不做地址归一化**：`8080`（省略监听地址）与 `127.0.0.1:8080` 是两个 key。

本节只讲**行去重**（同一份 `~/.ssh/config` 内部）。创建时还有第二层——「这个转发是否已经存在（已保存条目 / 本批次前面的行）」，判据 `TunnelConfig::forward_identity()`，三个创建入口都要过它。见 [dedup.md](dedup.md)。

| 配置 | 行数 | 创建结果 |
|---|---|---|
| `LocalForward 8080 …` × 2（不同块） | 1 | 1 条 |
| `LocalForward 8080 …` + `LocalForward 127.0.0.1:8080 …` | 2 | 2 条，第二条本地端口自动顺延 |

省略监听地址在 OpenSSH 里的含义依赖 `GatewayPorts`，`sshpf` 不替用户猜。

## 列出但不可导入的行 / Rows that cannot be imported

Unix domain socket 形式、以及 `-R` 没有目标，**不能**被默默丢掉——用户会以为工具漏读了配置。它们照常出现在表里，但勾选框显示 `[–]`、整行变暗、并用原因替换最后一个数据列：

```text
  [–] L     localhost:0        ⚠ 端口无法解析
  [–] R     localhost:7006     ⚠ 目标地址无法解析
```

判定在 `SshForward::unsupported_reason()`：

| 条件 | 原因文案 |
|---|---|
| `listen_port == 0` | `端口无法解析` |
| 非 `-D` 且 `target_host` 为空或 `target_port == 0` | `目标地址无法解析` |

端口留 0 是刻意的：解析失败时把端口记成 0，而不是猜一个数字或跳过整行。`listen_display()` 对空地址显示 `localhost`（OpenSSH 默认），所以仍能看出"这条监听解析不出来"而不是 `:0`。

这些行**永远勾不上**：`toggle_cursor` / `check_all_visible` / `invert_visible` 都先查 `row.importable()`，`ImportTable::new` 也不勾它们。`checked_count()` 的分母是 `importable_count()`，状态行因此显示 `2/2` 而不是 `2/3`。

## 完整例子 / Worked example

`~/.ssh/config`：

```text
 1  Host bastion
 2    HostName 203.0.113.9
 3    User ops
 4
 5  Host cloud_pc
 6    HostName 10.20.0.11
 7    User wangcan
 8    ProxyJump bastion
 9    LocalForward 127.0.0.1:18789 127.0.0.1:18789
10    LocalForward 8090 127.0.0.1:8080
11    # LocalForward 127.0.0.1:3081 127.0.0.1:3081
12
13  Host *
14    ServerAliveInterval 30
15    DynamicForward 1080
```

选中 `cloud_pc`，面板自动展开三行：

| 行 | 来源 | 说明 |
|---|---|---|
| `L 127.0.0.1:18789` | 第 9 行，`Host cloud_pc` | 具体块命中 |
| `L localhost:8090` | 第 10 行，`Host cloud_pc` | 省略监听地址 → 显示 `localhost` |
| `D localhost:1080` | 第 15 行，`Host *` | 通配块**累积**进来 |

没出现的：第 11 行注释（整行跳过）、第 1–3 行 `Host bastion`（只声明连接参数）、`HostName`/`User`/`ProxyJump`/`ServerAliveInterval`（不是转发指令）。`ProxyJump` 不影响导入——它决定"怎么连"，导入只关心"转发什么"。

按 `ctrl+s` 创建 3 条（字段怎么填见 [import-save.md](import-save.md#tunnelconfig-字段怎么由指令填)）：

| 名称 | 类型 | 本地 | 远端 | note |
|---|---|---|---|---|
| `cloud_pc-L-18789` | local | `127.0.0.1:18789` | `127.0.0.1:18789` | 来自 ~/.ssh/config 第 9 行 |
| `cloud_pc-L-8090` | local | `127.0.0.1:8090` | `127.0.0.1:8080` | 来自 ~/.ssh/config 第 10 行 |
| `cloud_pc-D-1080` | dynamic | `127.0.0.1:1080` | — | 来自 ~/.ssh/config 第 15 行 |

### 变体：不可导入的行

```text
Host cloud_pc
  LocalForward 127.0.0.1:18789 127.0.0.1:18789
  LocalForward /run/sshpf/app.sock /run/app.sock
  RemoteForward 7006
  RemoteForward 127.0.0.1:7000 127.0.0.1:22
```

| 行 | 显示 | 可导入 | 建成后 |
|---|---|---|---|
| 2 | `L 127.0.0.1:18789 → 127.0.0.1:18789` | ✓ | 本地 `:18789` → 远端 `:18789` |
| 3 | `L localhost:0` + `⚠ 端口无法解析` | ✗ | socket 路径不是 `host:port` |
| 4 | `R localhost:7006` + `⚠ 目标地址无法解析` | ✗ | `-R` 没有目标，无处可转发 |
| 5 | `R 127.0.0.1:7000 → 127.0.0.1:22` | ✓ | 远端监听 `:7000`，本地目标 `:22` |

第 5 行是唯一需要转置方向的：建出来是 `local=127.0.0.1:22`、`remote=127.0.0.1:7000`。

## 已知边界 / Known limits

| 边界 | 行为 |
|---|---|
| `Include` 总文件数上限 | 主文件 + 8 个 include，超出静默不读（`depth` 是总预算而非嵌套深度） |
| `Include` 写在 `Host` 块内 | 被包含文件的裸指令被当全局（多导入，不修） |
| 行去重不做地址归一化 | `8090` 与 `127.0.0.1:8090` 是两行；端口冲突由创建时顺延 + 监听校验兜底 |
| `-L` 身份不含本地监听端口 | 同一目标的 `LocalForward 8080 …` 与 `LocalForward 9090 …` 判同一个转发，只建一条（见 [dedup.md](dedup.md)） |
| `Match` 块 | 一行都不导入、不列出 |
| 可导入范围 | 只有 `LocalForward` / `RemoteForward` / `DynamicForward` |
| 编辑态键位归属 | 字段正在编辑时键归字段（`is_editing_field()`），否则归面板 |
