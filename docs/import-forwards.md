# 从 `~/.ssh/config` 导入转发 / Import forwards

> **这份文档回答**：哪些转发指令会被列出、作用域怎么累积、什么情况不导入、怎么去重与顺延端口、保存时一条勾选行变成什么。
> **不负责**：表单与面板的键位（见 [keymap.md](keymap.md#导入面板--import-pane)）、渲染细节（见 [tui.md](tui.md)）、设计权衡的完整论证（见 [design.md](design.md#d21-全选只看筛选后的视图) 等条目）。
> **相关代码路径**：`src/ssh/forwards.rs`（扫描/作用域/去重）、`src/model/`（`SshForward`/`ImportTable`/`plan_local_port`）、`src/app/import.rs`（发现与批量提交）。

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

静态丢数据比崩溃更危险。所以 `src/ssh/forwards.rs` **自己按行扫**这三组指令，`ssh2-config` 继续只负责连接参数。扫描是**面向行、引号感知**的，不是完整 config 解析器——要处理的指令都是单行，且作用域规则比 `Host`/`Match`/`Include` 简单。

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

**上限是"总共读几个文件"，不是"嵌套几层"**：`depth` 在 `while` 循环里每弹一个文件就 `+= 1`，与嵌套关系无关。实测 12 个**并列** `Include` 和 20 层**链式** `Include` 都只读 9 个文件。

顺序：遇到 `Include` 把展开的路径**压栈**，当前文件读完才逐个读 ⇒ include 出来的行总排在该文件所有行**之后**，不是出现在 `Include` 那一行的位置。这对"生成一组独立隧道"无影响（只影响表格行序），但**不要**把它当 OpenSSH 的文本内联语义。

路径解析：`~/` 展开到家目录，绝对路径原样，相对路径**相对于当前文件所在目录**；含 `*`/`?` 的做目录内 glob 并按名字排序。

### 一个无害的作用域泄漏（已记录，不修）

`Include` 若写在某个 `Host` 块**内部**，被包含文件里**没有 `Host` 行**的裸指令会被当成全局，从而对所有主机生效：

```text
Host web
  Include inc.conf          # inc.conf: LocalForward 42000 127.0.0.1:1
查询 web  -> [web 自己的转发, 42000]
查询 db   -> [42000]        ← 泄漏
```

原因是每个文件独立从 `Scope::Global` 开始扫，include 目标不知道自己在谁的块里；OpenSSH 会继承外层作用域。这是**多导入**而非漏导入（用户确实写了这条转发，只是没料到它全局生效）。真实配置里几乎不会把 `Include` 放进 `Host` 块，按"先记录、不引入跨文件作用域传递的复杂度"处理。

## 去重 / Dedup

```text
key() =  -D  :  D|listen_addr|listen_port
         -L/-R:  {kind:?}|listen_addr|listen_port|target_host|target_port
```

**不含** `host_alias` 与行号——同一条转发被两个块声明，物理上就是同一个转发。去重在**输出阶段**做，保留**第一次出现**的那条（行号因此指向更具体的块）。

防的失败模式很具体：`Host web` 和 `Host *` 各写一遍，去重后只剩 1 行；否则两条隧道抢同一监听端口，第二条必然 `bind: Address already in use`。

**key 是字面比较，不做地址归一化**：`8080`（省略监听地址）与 `127.0.0.1:8080` 是两个 key。

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

这些行**永远勾不上**：`toggle_cursor` / `check_all_visible` / `invert_visible` 都先查 `row.importable()`，`ImportTable::new` 也不勾它们。`checked_count()` 的分母是 `importable_count()`，所以状态行显示 `2/2 条勾选` 而不是 `2/3`。

### 自动展开的判定：可导入行数 > 0

```text
importable_count() > 0  →  form.import_open = true   （行已由 ImportTable::new 全部勾好）
importable_count() == 0 →  form.import_open = false
```

"有这个主机的行"不等于"有可做的事"。一台主机只写了 Unix socket 转发时表里全是 `[–]`，自动弹出只会盖住表单、把"没有可导入项"伪装成"有一堆东西等你处理"。此时面板保持关闭，但 `space`/`i` 仍能打开——用途从"给你默认"变为"给你解释"（那几行 `⚠` 原因都在）。

**主机一条转发都没有**是另一回事：空面板解释不了任何事，所以 `space`/`i` 不展开，只在状态行说明「该主机在 ~/.ssh/config 中没有端口转发配置」。判据是 `import.is_empty()`，与自动展开的 `importable_count() > 0` 是两个不同问题——前者问"展开有意义吗"，后者问"有默认可给吗"。

**主机字段为空时根本不扫描**：`Host *` 匹配空串，扫一遍会把所有全局转发放到用户没选过的别名下面，`ctrl+s` 会真的照这些行建隧道。空串是"还没选主机"，不是"匹配所有主机"。

**同一主机重复刷新是 no-op**：否则接受同一台主机两次会把用户已取消的勾选打回全勾、把手动收起的面板弹回来。默认值只应生效一次，换到另一台主机则是另一份默认，于是重新展开。

## 逐列筛选 + 撤销 / Column filters

四列（**类型 / 监听 / 目标 / 原始指令**）各有独立筛选，多个筛选 **AND**。单元格内容与筛选看到的值由同一个函数产生（`ImportColumn::cell`），所以表格与匹配器不可能对"这一列是什么"有分歧。

| 特性 | 实现 | 为什么 |
|---|---|---|
| 大小写不敏感**子串** | `col_match` = `to_lowercase().contains(..)` | 见下 |
| 空筛选 | 视为不生效 | 否则空串等于全匹配但会参与 AND |
| 撤销栈 | `Vec<[String; 4]>`，上限 64，LIFO | 一次按键一个撤销点 |
| 改筛选后 | `cursor = 0; scroll = 0; clamp_cursor()` | 视图变了不重锚，光标会指向不存在的行 |

### 为什么用子串而不是模糊匹配 / Substring, not fuzzy

主表 `/` 用**模糊子序列**（字符按序出现即命中）。列筛选**刻意不用**：

| 匹配器 | `1080` 会命中 |
|---|---|
| 模糊子序列 | `127.0.0.1:1080`、`127.0.0.1:10800`、`1:0:8:0`… |
| 子串（采用） | 只命中字面连续的 `1080` |

筛选端口是为了**精确定位一行**，返回一堆无关行等于没筛。列筛选是"缩小范围"工具（`8080`、`localhost`、`-R`），子串才符合用途。`类型` 列只有单字符 `L`/`R`/`D`，子串等于精确匹配。

`f` **优先于**筛选输入被处理（连按 `f` 是换列，不是把 `f` 写进去），代价是字面 `f` 无法输入——四列内容是端口、主机地址和指令名，没有哪列需要匹配 `f`。理由见 [design.md](design.md#d20-f-优先于筛选输入)。

### 撤销是快照式、一次一个键

`filter_char` / `filter_backspace` / `set_filter` / `clear_filter` / `clear_all_filters` **每次都在改之前压一份完整快照**：

- 输入 `8080` 压 4 份，按 4 次 `z` 逐字符回退；
- `ctrl+r` 一次清空四列只压**一**份，一次 `z` 全部恢复；
- 删掉某列的 chip 也只压一份；栈满 64 丢最旧。

`undo_filter()` 返回 `bool`，调用方据此提示「已撤销筛选」或「没有可撤销的筛选」——空栈静默无反应会让人以为按键没生效。

### 全选 / 反选 / 全不选只看筛选后的视图 / Bulk ticks see only the filtered view

三个操作都只作用于 `ImportTable::visible()`：

```text
3 行: L(18003) / R(18004) / L(18005)，先全部取消勾选，再筛选 类型=R，按 全选
实际结果: checked = [false, true, false]    ← 只有可见的 R 行被勾上
```

若"全选"勾上隐藏行，用户看到 3 行里只勾了 1 行，保存却创建 3 条隧道——对批量创建（起 N 个监听端口）是明确的脚枪。鼠标点这些按钮走同一批函数，规则一致。同一条规则保护主表多选（[design.md](design.md#d22-多选标记按-tunnelid不按行号)）。

## 保存：一行一条隧道 / Save: one row, one tunnel

`ctrl+s`（或 `f10`）在**面板可见**时走批量路径，**整批先校验、全过才写**：

```text
for each ticked forward:
    TunnelConfig ← config_from_forward
    校验字段（validate_tunnel）
    校验监听地址冲突：已保存的隧道 + 本批次里前面的行
    通过 → 收进 out
errors 非空 → 错误写进表单，一条都不创建
否则 → 全部 append、persist 一次、逐条启动
```

"全部校验完才写"防的是**部分写入**：创建 3 条、第 4 条报错，用户会得到一个自己没要求过的状态。

### 提交归属：看面板在不在屏幕上

判定条件是 `!is_modifying() && import_open && !import.is_empty()`——**三个都要**。

为什么不是"导入表非空"就够：面板曾经要手动 `space` 才出现，那时"有行"确实等于"用户正在用它"。面板改成默认展开后这个等式断了——用户可能故意收起面板、想手填一条单条转发；若仍按"有行"走批量，`ctrl+s` 只会得到「没有勾选任何转发」，那条手填的隧道永远建不出来，收起面板于是等于"这个表单废了"。

| 面板状态 | `ctrl+s` 做什么 | 表单字段 |
|---|---|---|
| 在屏幕上（默认） | 批量：每条勾选行一条隧道 | 忽略（只是草稿） |
| 已收起 | 单条：按手填字段创建一条 | 校验并采用 |

原则一句话：**校验必须对着用户正在看的那份界面**。

### `TunnelConfig` 字段怎么由指令填

| 指令 | `local_addr` / `local_port` | `remote_host` / `remote_port` |
|---|---|---|
| `-L` | 指令的**监听**端 | 指令的**目标**端 |
| `-R` | 指令的**目标**端 | 指令的**监听**端 |
| `-D` | 指令的**监听**端 | 空 / 0（SOCKS5 没有远端） |

`-R` 两端**对调**：`RemoteForward 7000 localhost:22` 里 `7000` 是服务端监听、`localhost:22` 是本地目标；而 `TunnelConfig` 的 `local_*` 永远是"本机这一侧"。填反了会静默转发错误的端口。监听地址为空时填 `defaults.bind`（默认 `127.0.0.1`）。

### 本地端口顺延 / Local port shifting

`-L`/`-D` 需要在本机 bind，所以先过 `plan_local_port`（与表单的建议逻辑**共用同一份规则**）：

| 情况 | 结果 |
|---|---|
| 端口 ≥1024 且可用 | 原样使用 |
| 端口 <1024（需 root） | 惯用别名 `remote + 2200`（`22 → 2222`），**不从特权区间向上扫** |
| 以上都不行 | 从 `max(port, 1024)` 向上扫，最多 200 个候选 |
| 200 个都不行 | **保留原端口**，让校验报出真正的问题，而不是静默改成 0 |

`-R` 不在本机监听，端口原样保留。特权端口不能在自己所在区间向上扫的原因：`port_available` 会把所有 <1024 的端口报成不可用（绑定需要 root），从那里起步会烧完整个扫描预算仍然找不到端口。

`taken` 列表在批次内**逐条增长**，所以同批次两行要同一端口时第二行被顺延；已保存但**未运行**的隧道也计入——它现在没在监听，但一旦启动就会冲突。

### 名称

建议名 `<host>-<L|R|D>-<监听端口>`（如 `cloud_pc-L-8090`），两级去重：`unique_name` 对已保存隧道去重（→ `-2`），`unique_within` 对本批次内去重。

**名字描述配置里写了什么，不是运行时落在哪**：`8090` 被占而顺延成 `8091` 时，名字仍是 `cloud_pc-L-8090`。

### 保存后的状态

每条隧道 `enabled = true`，`persist` **一次**（不是 N 次），然后逐条启动；`note` 记 `来自 ~/.ssh/config 第 N 行` 便于回溯；表单关闭，光标落到**第一条**新建隧道上。

## "默认全选"只是意图，不是监听

- 勾选状态只是**意图标记**，存在 `ImportTable` 里：不写盘、不 bind、不建线程；
- 隧道只在 `ctrl+s` **确认保存之后**才创建；
- 关掉表单（`esc`，有未保存内容会二次确认）不创建任何东西；
- **编辑已有条目时没有导入面板**（`refresh_import_table` 对 `FormMode::Edit` 直接返回）——把一次编辑悄悄变成批量新建是数据丢失级的问题。

**为什么"自动展开"是这条默认成立的必要条件**：一个要再按一次 `space` 才出现的全选，不是默认值，而是一份**藏起来的提议**——对知道入口的用户等价于默认，对不知道的用户等于不存在，而"默认全选"的价值恰恰落在后一类用户身上。默认值是"不需要做任何动作就得到的初始状态"。

这条规则还有一个必须在按键层处理好的副作用：面板常驻屏幕上，就不能像"按需打开的子模式"那样无条件吞掉字母键。某个文本字段正在编辑时 `j`/`k`/`a`/`i` 是普通字符（`api-db`、`jenkins`），必须归字段。守卫看的是 `is_editing_field()`，不是"面板开没开"。

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

按 `ctrl+s` 创建 3 条：

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
| 去重不做地址归一化 | `8090` 与 `127.0.0.1:8090` 是两行；端口冲突由创建时顺延 + 监听校验兜底 |
| `Match` 块 | 一行都不导入、不列出 |
| 可导入范围 | 只有 `LocalForward` / `RemoteForward` / `DynamicForward` |
| 编辑态键位归属 | 字段正在编辑时键归字段（`is_editing_field()`），否则归面板 |
