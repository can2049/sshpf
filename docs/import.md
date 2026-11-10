# 导入与新增 / Import & add

> **这份文档回答**：`~/.ssh/config` 里哪些行会被列出（作用域、`Match`、`Include`、行去重）、导入面板怎么交互（自动展开、`touched`、「已存在」行、逐列筛选）、`ctrl+s` 归谁、一条勾选行变成什么 `TunnelConfig`、对端端口怎么扫。
> **不负责**：转发**身份**去重与载入时折叠（见 [dedup.md](dedup.md)）、键位全表（见 [keymap.md](keymap.md#导入面板--import-pane)）、渲染（见 [tui.md](tui.md)）。
> **相关代码路径**：`src/ssh/forwards.rs`（行扫描 → 作用域 → 行去重）、`src/app/import_pane.rs`（面板交互）、`src/app/import.rs`（批量提交）、`src/app/form_submit.rs`（提交路由）、`src/model/import/`（`SshForward`/`ImportTable`/`plan_local_port`）、`src/ssh/probe.rs` + `src/app/port_scan.rs`（对端端口）。

一次「新增」分四个阶段，身份规则只有一份：

| 阶段 | 做什么 | 代码 |
|---|---|---|
| 发现 | 按行扫三组转发指令，累积作用域，行去重 | `ssh::forwards` |
| 提议 | 面板列出候选行默认全勾；已存在的行标灰不可勾 | `app::import_pane`、`model::import` |
| 提交 | `ctrl+s` 路由 → 校验 → 构建 `TunnelConfig` → 逐条建 | `app::form_submit`、`app::import` |
| 去重 | 拦「已经存在」；载入时折叠历史重复 | `model::tunnel::identity_of` + [dedup.md](dedup.md) |

## 从 `~/.ssh/config` 发现 / Discovery

`sshpf` **从不写** `~/.ssh/config`，但会读它。除连接参数（`HostName`/`Port`/`User`/`ProxyJump`/`IdentityFile`）外还读三组转发指令：

```text
LocalForward   [bind_address:]port  host:hostport
RemoteForward  [bind_address:]port  host:hostport
DynamicForward [bind_address:]port
```

表单里接受一个主机，这些指令**自动**列成表、**默认全部勾选**；一条勾选行 = 一条隧道。

连接参数用 [`ssh2-config`](https://crates.io/crates/ssh2-config) 0.9.0 解析，但**转发指令不行**：`LocalForward`/`DynamicForward` 会落进 `unsupported_fields`，那是 `HashMap<String, Vec<String>>`，同名的多条指令**互相覆盖且不保序**（`Host` 里两条 `LocalForward` 只活一条）。所以 `src/ssh/forwards.rs` 自己按行扫（行导向、引号感知，不是完整解析器）。

## 作用域规则 / Scope

OpenSSH 的转发指令**从所有匹配的 `Host` 块累积**，不是"命中第一个就停"。

| 规则 | 行为 |
|---|---|
| 多个 `Host` 块都匹配 | **全部累积**，按文件顺序（只取第一个会让 `Host *` 的公共转发整个消失） |
| `Host *` / 通配符 `*` `?` | 支持，**大小写不敏感**（`*.Example.COM` 要能匹配 `api.example.com`） |
| 否定子句 `!prod-db` | 命中即**排除整个块**，否定优先 |
| 第一个 `Host` 行**之前**的指令 | 视为全局，等价 `Host *` |
| `Match` 块 | **永不导入、也不列出**（见下） |
| `Include` | 跟进，有**总文件数**上限与环检测（见下） |

### `Match` 一律不导入 / Match blocks are never imported

`Match` 的转发**带连接上下文前置条件**。`sshpf` 不建模这些条件，把受 `Match` 门控的转发当成无条件生效，等于在用户明确设了闸的地方静默开监听端口——它不是"这个主机声明了什么"，而是"满足某条件时声明了什么"。

实现上 `Scope` 遇到 `match` 就置 `InBlock { applies: false, excluded: true }` 直到下一个 `Host`；`strip_match_blocks`（`src/ssh/config.rs`）在扫描前还会先剔除 `Match` 行及其后缩进行，同一道预处理也保护连接参数解析（见 [design.md](design.md#d2-sshconfig-由-ssh2-config-解析但必须自己剔除-match-块)）。两个已知边界都是**少导入**而非误导入：未缩进的后续指令躲过 `strip_match_blocks`，但 `scan_config` 的 `match` 分支已关掉 scope；`Include` 进来的文件同样过 `strip_match_blocks`。

### `Include`：总文件数预算 / Include budget

`forwards_for_host` 用显式栈遍历，三个上限：`MAX_INCLUDE_DEPTH = 8`（**合计**最多读 9 个文件，主文件占 1 次）、`MAX_PENDING_INCLUDES = 256`（待读队列上限，超出记 `sshconfig.include_capped` 并停止展开，防生成式配置卡住 UI 线程）。`seen_files` 挡 `a.conf` ↔ `b.conf` 死循环，`depth` 挡"每层都是新文件"的无限链。**上限是"总共读几个文件"，不是"嵌套几层"**——实测 12 个并列 `Include` 与 20 层链式 `Include` 都只读 9 个文件。

顺序：遇到 `Include` 把展开的路径**压栈**，当前文件读完才逐个读 ⇒ include 出来的行总排在该文件所有行**之后**，不是 `Include` 那一行的位置——**不要**当 OpenSSH 的文本内联语义。路径解析：`~/` 展开到家目录，绝对路径原样，相对路径**相对于当前文件所在目录**，含 `*`/`?` 的做目录内 glob 并按名字排序。

**一个无害的作用域泄漏（已记录，不修）**：`Include` 写在某个 `Host` 块**内部**时，被包含文件里**没有 `Host` 行**的裸指令会被当成全局（每个文件独立从 `Scope::Global` 开始扫；OpenSSH 会继承外层作用域）。这是**多导入**而非漏导入，真实配置里几乎不会这样写。

## 行去重 / Row dedup

```text
key() =  -D  :  D|listen_addr|listen_port
         -L/-R:  {kind:?}|listen_addr|listen_port|target_host|target_port
```

**不含** `host_alias` 与行号——同一条转发被两个块声明，物理上就是同一个转发。去重在**输出阶段**做，保留**第一次出现**的那条。防的失败模式很具体：`Host web` 和 `Host *` 各写一遍，不去重则两条隧道抢同一监听端口，第二条必然 `bind: Address already in use`。

**key 是字面比较，不做地址归一化**：`8080`（省略监听地址）与 `127.0.0.1:8080` 是两个 key ⇒ 两行、两条隧道，第二条的本地端口自动顺延。省略监听地址在 OpenSSH 里的含义依赖 `GatewayPorts`，`sshpf` 不替用户猜。

本节只讲**行去重**（同一份 `~/.ssh/config` 内部）；创建时的第二层见 [dedup.md](dedup.md)。

## 列出但不可导入的行 / Rows that cannot be imported

Unix domain socket 形式、以及 `-R` 没有目标，**不能**被默默丢掉——用户会以为工具漏读了配置。它们照常出现在表里，但勾选框显示 `[–]`、整行变暗、并用原因替换最后一个数据列：

```text
  [–] L     localhost:0        ⚠ 端口无法解析
  [–] R     localhost:7006     ⚠ 目标地址无法解析
```

判定在 `SshForward::unsupported_reason()`：`listen_port == 0` → `端口无法解析`；非 `-D` 且 `target_host` 为空或 `target_port == 0` → `目标地址无法解析`。端口留 0 是刻意的（不猜数字、不跳过整行）；`listen_display()` 对空地址显示 `localhost`，所以仍能看出"这条监听解析不出来"而不是 `:0`。

这些行**永远勾不上**：`toggle_cursor` / `check_all_visible` / `invert_visible` 都先查 `row.importable()`，`ImportTable::new` 也不勾它们。`checked_count()` 的分母是 `importable_count()`，状态行因此显示 `2/2` 而不是 `2/3`。

## 面板 / The pane

| 判据 | 行为 |
|---|---|
| `importable_count() > 0` | 自动展开（`form.import_open = true`，可创建的行已全部勾好） |
| `importable_count() == 0` | 不自动展开：全是 `[–]` 或全是「已存在」时它没有默认可给，弹出来只会盖住表单 |
| `import.is_empty()` | 主机一条转发都没有 ⇒ `space`/`i` 也不展开，只在状态行说明「该主机在 ~/.ssh/config 中没有端口转发配置」 |
| 主机字段为空 | **根本不扫描**：`Host *` 匹配空串，扫一遍会把全局转发挂到用户没选过的别名下，`ctrl+s` 会真的照这些行建隧道 |

`space`/`i` 永远能手动开关面板，所以"自动展开"只决定**默认状态**；面板非空时手动打开，用途从"给你默认"变成"给你解释"（那几行 `[–]`/`[✓]` 的原因都在）。

**同一主机重复刷新是 no-op**：否则接受同一台主机两次会把用户已取消的勾选打回全勾、把手动收起的面板弹回来。默认值只应生效一次；换到另一台主机是另一份默认，于是重新展开。

### 已存在的行 / Rows that already exist

`config.toml` 里**已经有**同一个转发（判据见 [dedup.md](dedup.md#forward_identity意图身份)）时，这一行仍然列出，但：

| 表现 | 原因 |
|---|---|
| 勾选框是 `[✓]`，不是 `[x]` | "你已经有了"与"还没建"是两句话，一个方形 checkbox 说不清 |
| **不可勾选**：`space` / `a` / `I` 全部跳过它 | 任何键盘路径能勾上一条已知重复，就等于没修这个 bug |
| 整行变暗，最后一列写「已存在：<隧道名>」 | 用户的问题是"为什么这行不能勾"，答案需要是句子 |
| 计数里不含它：`1/1 条勾选 · 1 条已存在` | 分母是"保存能创建几条"，算进去会承诺一条并不出现的隧道 |
| 不参与自动展开 | 没有可创建的默认值 |

**还列出而不是隐藏**：隐藏会让面板与 `~/.ssh/config` 静默不一致——而那正是这一层要消除的困惑。**为什么不能"建好配置再比"**：建配置要跑 `plan_local_port`，它会探测端口并可能顺延，于是答案会随当前谁在监听而变。所以走 `SshForward::forward_identity()`，用**指令里写的**端口，不规划。

### touched：默认提议 vs 用户意图

`touched` 只回答一个问题：**这份勾选是"到手的默认值"还是"用户的选择"**。提交路由靠它决定 `ctrl+s` 归面板还是归手填表单。

| 会置位（用户动过） | 不置位（只是看/默认） |
|---|---|
| `toggle_cursor`（勾选/取消） | `move_cursor` / `home` / `end` |
| `check_all_visible` / `uncheck_all_visible` / `invert_visible` | `clamp_cursor` / `ensure_visible` |
| 改筛选（`filter_char` / `set_filter` / `clear_*` / `undo_filter`） | 自动展开（`install_import_rows`） |
| 手动 `space`/`i` 展开、聚焦某列筛选 | 点行选中（`import_select_at`，只移动光标） |

**导航不是选择**（翻着看一遍不代表要这几行，否则"随便看看"就抢走了手填表单的提交权）；**换主机清零**（`FormState::set_import_table`），**收起不清零**（动过就是动过）。`touched`、`existing`、`checked_count` 三者正交：分别问「这份勾选是不是用户选的」「这一行还能不能创建」「这份选择里有没有东西」。

### 逐列筛选 + 撤销 / Column filters

四列（**类型 / 监听 / 目标 / 原始指令**）各有独立筛选，多个筛选 **AND**。单元格内容与筛选看到的值由同一个函数产生（`ImportColumn::cell`），所以表格与匹配器不可能对"这一列是什么"有分歧。

| 特性 | 实现 | 为什么 |
|---|---|---|
| 大小写不敏感**子串** | `col_match` = `to_lowercase().contains(..)` | 见下 |
| 空筛选 | 视为不生效 | 否则空串等于全匹配但会参与 AND |
| 撤销栈 | `Vec<[String; 4]>`，上限 64，LIFO，**改之前**压完整快照 | 一次按键一个撤销点：输入 `8080` 压 4 份，按 4 次 `z` 逐字符回退；`ctrl+r` 清四列只压 1 份；栈满丢最旧 |
| 改筛选后 | `cursor = 0; scroll = 0; clamp_cursor()` | 视图变了不重锚，光标会指向不存在的行 |

`undo_filter()` 返回 `bool`，调用方据此提示「已撤销筛选」或「没有可撤销的筛选」——空栈静默会让人以为按键没生效。

#### 为什么用子串而不是模糊匹配 / Substring, not fuzzy

主表 `/` 用**模糊子序列**，列筛选**刻意不用**：模糊匹配下 `1080` 会命中 `127.0.0.1:10800`、`1:0:8:0`，而筛选端口是为了**精确定位一行**。`类型` 列只有单字符 `L`/`R`/`D`，子串等于精确匹配。`f` **优先于**筛选输入（连按是换列），代价是字面 `f` 无法输入——四列内容没有哪列需要匹配 `f`。理由见 [design.md](design.md#d20-f-优先于筛选输入)。

#### 全选 / 反选 / 全不选只看筛选后的视图 / Bulk ticks see only the filtered view

三个操作都只作用于 `ImportTable::visible()`：3 行（`L`/`R`/`L`）先全部取消勾选、再筛选 `类型=R`、按全选，结果是 `checked = [false, true, false]` ——只有可见的 R 行被勾上。

若"全选"勾上隐藏行，用户看到 3 行里只勾了 1 行，保存却创建 3 条隧道——对批量创建是明确的脚枪。鼠标点这些按钮走同一批函数；同一条规则保护主表多选（[design.md](design.md#d22-多选标记按-tunnelid不按行号)）。

## 保存归属 / Who wins the save

`ctrl+s`（或 `f10`）在**面板赢得提交**时走批量路径，**整批先校验、全过才写**。

### 提交归属：默认提议 vs 明确指令

面板自动展开、自动全勾是刻意的产品默认，所以「面板在屏幕上」**不能**独自等于「面板就是用户的意思」——它同样是"选完主机、然后开始手填一条具体转发"的状态。判据（`App::import_pane_wins_submit`）：`非编辑态 && 面板可见 && 表非空 && (一行都没勾 ? 表单不完整 : 用户动过面板 || 表单字段不完整)`。

| 状态 | `ctrl+s` 做什么 | 表单字段 |
|---|---|---|
| 面板未被动过，手填字段**完整** | 单条：只按手填字段创建一条 | 校验并采用 |
| 面板未被动过，手填字段**不完整** | 批量：每条勾选行一条隧道 | 忽略（只是草稿） |
| 面板**动过**，且还有勾选行 | 批量：每条勾选行一条隧道 | 忽略（只是草稿） |
| 面板**动过但一行都没勾**，手填字段**完整** | 单条：只按手填字段创建一条 | 校验并采用 |
| 面板**动过但一行都没勾**，手填字段**不完整** | 报「没有勾选任何转发」，不创建 | 不适用 |
| 面板已收起 | 单条：按手填字段创建一条 | 校验并采用 |

前两行修的是"面板可见即归属"会**静默吞掉手填输入**的实测缺陷；后两行修的是"勾选被清空不等于面板在发号施令"。手填胜出时状态栏说明「已创建手填的 1 条转发；导入面板的 N 条未创建」——沉默会被读成"批量也跑了"。第 4 行还覆盖不需要用户动手的空面板：转发**全部已存在**时每行都不可勾，`checked_count()` 恒为 0，用户展开只为看灰行（`any_checked()` 按 `creatable()` 计数）。

原则一句话：**校验必须对着用户真正在用的那份界面**；反过来，**没有可执行内容的那份界面不能赢**。

## 批量提交：整批先校验 / All-or-nothing batch

```text
for each ticked forward:            ← 已保存的行根本不会被勾上
    TunnelConfig ← config_from_forward
    跳过：已保存的隧道 / 本批次前面的行里有同一转发（见 dedup.md）
    校验字段（validate_tunnel）→ 校验监听冲突（已保存 + 本批次前面的行）
    通过 → 收进 out
errors 非空 → 错误写进表单，一条都不创建
否则 → append、persist 一次、逐条启动
```

"全部校验完才写"防的是**部分写入**：创建 3 条、第 4 条报错，用户会得到一个自己没要求过的状态。

### TunnelConfig 字段怎么由指令填

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

`-R` 不在本机监听，端口原样保留。特权端口不能在自己所在区间向上扫：`port_available` 会把所有 <1024 的端口报成不可用（绑定需要 root），从那里起步会烧完整个扫描预算仍找不到端口。

`taken` 列表在批次内**逐条增长**，所以同批次两行要同一端口时第二行被顺延；已保存但**未运行**的隧道也计入——它现在没在监听，但一旦启动就会冲突。**顺延值会被持久化**，这正是[转发去重](dedup.md)为什么不比本地端口的原因：`config.toml` 里存的是顺延后的数字，原始请求事后不可恢复。

### 名称与保存后的状态 / Name & after-state

建议名 `<host>-<L|R|D>-<监听端口>`（如 `cloud_pc-L-8090`），两级去重：`unique_name` 对已保存隧道去重（→ `-2`），`unique_within` 对本批次内去重。**名字描述配置里写了什么，不是运行时落在哪**：`8090` 被占而顺延成 `8091` 时，名字仍是 `cloud_pc-L-8090`。

每条隧道 `enabled = true`，`persist` **一次**（不是 N 次），然后逐条启动；`note` 记 `来自 ~/.ssh/config 第 N 行` 便于回溯；表单关闭，光标落到**第一条**新建隧道上。部分行因重复被跳过时状态栏追加「跳过 N 条重复转发」；**全部**被跳过时创建 0 条、状态栏「N 条转发已存在，未重复创建」并且表单正常关闭——"你要求的事已经成立"不是错误。

## 对端监听端口 / The peer's listening ports

新建一条转发时远端端口经常只能靠记忆填。表单的 `远端端口` 字段上按 `p` 会去问对端：

```text
command -v ss      → ss -ltnH
else command -v netstat → netstat -ltn
else               → __SSHPF_NO_TOOL__
```

命令是**写死的字符串**（`src/ssh/probe.rs::list_command`），不插入主机名、别名或任何用户输入，所以恶意 `Host` 别名没有可注入的位置。三条规则值得记住：

| 规则 | 为什么 |
|---|---|
| 只认 `LISTEN` 行 | 每条列出的端口都会被当成"可以转发过去的地方"；客户端连接的临时端口看起来同样合理，但转发过去什么都不通 |
| 哨兵区分"没有工具"与"没有监听" | 空输出完全一样，但结论相反：一个是"装个 ss"，一个是"对端确实没开东西" |
| `ss` 的 `-H` 抑制表头 | 表头 `Local Address:Port` 含冒号，宽松的 token 扫描会把 `Port` 读成地址 |

探测走 `ssh::client::connect`（同一个 endpoint 解析、同一条 `ProxyJump` 链、同样的认证与主机密钥策略），**不另开一条连接路径**。"列端口能用但隧道连不上"正是第二条路径造成的经典缺陷。

`~/.ssh/config` 已为该主机声明过的端口**不重复列出**（`model::declared_ports`）——它们已由导入面板提供，两个入口指向同一条隧道只会让用户建出两条。隐藏的条数显示在面板上（"已隐藏 N 个"），沉默会让用户以为扫描漏了。

| 方式 | 手势 | 结果 |
|---|---|---|
| 选一条 | `enter` | 端口填进 `远端端口`，列表收起，本地端口按 `plan_local_port` 重新建议 |
| 选多条 | `space` / `a` / `U` / `I` 勾选后 `ctrl+s` | **每个勾选端口一条 `-L` 转发**，整批全有或全无；也过[转发身份去重](dedup.md)，已保存的转发对应端口被跳过并说明 |

勾选**默认全空**：发现一个服务不等于要转发它，一次按键不该开出多个监听。

**为什么 `-R` 是只读列表**：`-R` 的远端端口是**服务端要监听的**端口，而扫描列出的是那里**已被占用**的端口——选一个必然在 `tcpip-remote-forward` 时撞车。所以显示 `[–]` 且不能勾选（`multi=false` 时 `PortPick::can_tick()` 恒为 false）。

**扫描是后台线程，且带代次**：探测是真的 SSH 连接，不能跑 UI 线程（200ms 帧循环会卡死）。它与隧道同形：一条线程 + `current_thread` runtime + `mpsc`，每帧由 `drain_events` 收一次。用户可能在探测途中换主机或重扫，所以每次扫描带 `seq`，**过期的答案直接丢弃**（`PortPick::install` 返回 false）。

## 完整例子 / Worked example

```text
 5  Host cloud_pc
 6    HostName 10.20.0.11
 7    User wangcan
 8    ProxyJump bastion
 9    LocalForward 127.0.0.1:18789 127.0.0.1:18789
10    LocalForward 8090 127.0.0.1:8080
11    # LocalForward 127.0.0.1:3081 127.0.0.1:3081
13  Host *
14    ServerAliveInterval 30
15    DynamicForward 1080
```

选中 `cloud_pc` 后面板自动展开三行，`ctrl+s` 创建 3 条：第 9 行 → `cloud_pc-L-18789`（`127.0.0.1:18789`→`127.0.0.1:18789`）；第 10 行 → `cloud_pc-L-8090`（`127.0.0.1:8090`→`127.0.0.1:8080`，省略监听地址显示 `localhost`）；第 15 行 → `cloud_pc-D-1080`（通配块**累积**进来）。没出现的是第 11 行注释与 `HostName`/`User`/`ProxyJump`/`ServerAliveInterval`——`ProxyJump` 决定"怎么连"，导入只关心"转发什么"。

不可导入的变体：

| 指令 | 显示 | 可导入 | 建成后 |
|---|---|---|---|
| `LocalForward 127.0.0.1:18789 127.0.0.1:18789` | `L 127.0.0.1:18789 → 127.0.0.1:18789` | ✓ | 本地 `:18789` → 远端 `:18789` |
| `LocalForward /run/sshpf/app.sock /run/app.sock` | `L localhost:0` + `⚠ 端口无法解析` | ✗ | socket 路径不是 `host:port` |
| `RemoteForward 7006` | `R localhost:7006` + `⚠ 目标地址无法解析` | ✗ | `-R` 没有目标，无处可转发 |
| `RemoteForward 127.0.0.1:7000 127.0.0.1:22` | `R 127.0.0.1:7000 → 127.0.0.1:22` | ✓ | 唯一需要转置方向：`local=127.0.0.1:22`、`remote=127.0.0.1:7000` |

## 已知边界 / Known limits

| 边界 | 行为 |
|---|---|
| `Include` 总文件数上限 | 主文件 + 8 个 include，超出静默不读（`depth` 是总预算而非嵌套深度） |
| `Include` 写在 `Host` 块内 | 被包含文件的裸指令被当全局（多导入，不修） |
| 行去重不做地址归一化 | `8090` 与 `127.0.0.1:8090` 是两行；端口冲突由创建时顺延 + 监听校验兜底 |
| `-L` 身份不含本地监听端口 | `LocalForward 8080 …` 与 `LocalForward 9090 …`（同目标）判同一个转发，只建一条（见 [dedup.md](dedup.md)） |
| `Match` 块 / 可导入范围 | `Match` 一行都不导入、不列出；只认 `LocalForward` / `RemoteForward` / `DynamicForward` |
| 编辑态键位归属 | 字段正在编辑时键归字段（`is_editing_field()`），否则归面板 |
| 面板高度 | 终端太矮时面板整个让位，字段优先；`import_open` 仍为 true，渲染层在表单里提示"导入面板未显示" |
| 两个子面板不共存 | 打开对端端口列表会收起导入面板，反之亦然 |
| 字面 `f` | 无法输入到筛选（`f` 永远先被当成"换列"） |
| `touched` 的粒度 | 是整个表面，不是逐行：动过任意一行就整体算"用户用过这个面板" |
| 已存在的行 | 只是"不再作为默认"，不是"从文件里删掉"：`~/.ssh/config` 不会被改动，那行照旧列出 |
| 编辑已有条目 | **没有导入面板**（`refresh_import_table` 对 `FormMode::Edit` 直接返回），永远单条 + 二次确认 |
| 部分失败的批次 / 重复行 | 一条都不写（全有或全无），错误全部写进表单；重复行跳过而非报错，每条记 `event=action.duplicate_skipped` |
| 名称里的端口 | 是**请求**的端口，不是顺延后的 |
| 对端无工具 / 格式不认识 | 明确报告"无法列出端口"或"格式无法解析（只支持 ss / netstat -l）"，而不是显示成"没有监听" |
| 重扫 | 保留已勾选的端口（刷新不是重新决策）；扫描失败原因写进面板与状态行，记 `event=portscan.failed` |
