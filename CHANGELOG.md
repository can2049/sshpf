# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 修复

- **手填的「新建转发」不再被自动展开的导入面板吞掉**（未被动过的面板只是默认提议，完整的手填
  表单才是明确指令）；**新增转发也不再重复堆积**，判定比意图不比端口，见 [docs/dedup.md](docs/dedup.md)。

### 变更

- **给 AI agent 的协作约束：一条改动一个 worktree。** 文档新增「一特性一 worktree」
  一节（[`AGENTS.md`](AGENTS.md) + [`docs/agents.md`](docs/agents.md#一特性一-worktree--one-worktree-per-change)）：
  开发新特性、修复问题一律在 `.agents/worktrees/<分支名>` 的独立 worktree + 独立分支里做，
  不在共享 checkout 里改；**PR 合并后立即删除该 worktree 与本地/远端分支**，不留僵尸分支。
  纯文档约定变更，无代码、无行为变化。
- 新增事件 `action.duplicate_skipped`；`docs/import-forwards.md` 按职责拆为 import-pane /
  import-save / port-scan 三篇，`design.md` 加 D32/D33。

## [0.5.0] - 2026-11-09

本次是 minor 版本，两件事：把「新建 / 编辑转发」里最靠记忆的一步——填远端端口——
变成可以直接从对端正在监听的端口里挑；并修正 v0.4.0 引入的一个主机密钥误判
（同一台主机的「不同算法」被错报成「密钥已改变」）。无新增依赖、无配置格式变化。

### 新增

- **远端端口可以直接从对端监听端口里选**。把光标放在表单的 `远端端口` 上按 `p`，
  sshpf 会通过**隧道同一条 SSH 路径**（同样的 `~/.ssh/config` endpoint、`ProxyJump` 链、
  `IdentityFile`/ssh-agent 认证与主机密钥策略）在对端执行 `ss -ltnH`（没有则
  `netstat -ltn`），把正在监听的端口列成面板：
  - `enter` 把光标行的端口填进字段（本地端口随之重新推荐），列表收起；
  - `space` 勾选多个后用 `ctrl+s` 保存，**每个勾选端口创建一条 `-L` 转发**，
    整批先校验、全过才写（不会出现"建了三条例、第四条报错"的半成品状态）；
  - `a` / `U` / `I` 全选 / 清空 / 反选，`r` 重扫，`esc` / `i` 收起。

  设计上的几条取舍：勾选**默认全空**（发现一个服务不等于要转发它，一次按键不该开出多个
  监听）；`~/.ssh/config` 已为该主机声明过的端口**不重复列出**（它们已由导入面板提供，
  面板上写明隐藏了几个）；`-R` 下列表是**只读**的（那里的端口已被占用，选一个必然
  在建立远程转发时撞车）；对端既没有 `ss` 也没有 `netstat` 时明确报告，而不是显示成
  "没有监听"；命令是写死的固定文本，不插入主机名或任何用户输入。扫描在独立线程 +
  `current_thread` runtime 上跑，带代次号，换主机或重扫时旧结果被丢弃。

### 变更

- 帮助目录新增 `对端端口` 分区（`SectionId::PortPicker`），表单分区补 `p` 一行；
  `docs/keymap.md`、`docs/import-forwards.md`、`docs/tui.md`、`docs/architecture.md`、
  `docs/index.md`、`docs/logging/events.md` 同步更新。
- 新增日志事件 `portscan.started` / `portscan.finished` / `portscan.failed`（它另外开一条
  SSH 连接，需要给对端的认证日志一个本地解释），以及 `portscan.stale` / `portscan.dropped` /
  `portscan.batch_rejected`。

### 修复

- **「服务器提供多种密钥算法」被误报为「主机密钥已改变」。** `known_hosts` 的条目本应按
  「主机 + 算法」匹配（OpenSSH 语义）：一台主机可以同时有 `ssh-ed25519` 与
  `ecdsa-sha2-nistp256` 两行，协商到后者不等于密钥被换。原实现把**任何**不匹配的条目都
  记为 `Changed`，于是"服务器提供多算法、而 `known_hosts` 只记了其中一种"这种极常见的情况
  会跳出「可能存在中间人攻击」并拒绝连接——即使 `@revoked`、算法都没问题。

  实测：某主机在 `known_hosts` 只有 ECDSA 一行，而会话协商到 ED25519。OpenSSH 在
  `StrictHostKeyChecking=yes` 下能正常登录，sshpf 却拒绝，两者对同一份文件判定相反：

  ```text
  INF 10.244.119.128 的主机密钥已改变（known_hosts 第 36 行），可能存在中间人攻击
  ```

  现在按算法区分：**同一算法下不同密钥**才是 `Changed`（仍然拒绝/告警）；**不同算法**属
  未知，走该策略对未知主机的处理（`accept-new` 学到并追加一行，`strict` 拒绝）。这一条也让
  `learn` 能为一台主机补记第二种算法，此前会因误判成 `Changed` 而拒绝追加。

  影响面：v0.4.1 之前，凡是 `known_hosts` 只记录了部分算法的多算法主机，都可能被误拒；
  这也是"`ssh` 能连、sshpf 连不上"除了跳板机之外的另一类成因。


## [0.4.0] - 2026-11-09

本次是 minor 版本：新增主机密钥变更的忽略能力。默认行为不变（密钥变了仍然拒绝），
新增一个全局开关，并让 sshpf 尊重 `~/.ssh/config` 里用户已经写下的 `StrictHostKeyChecking` /
`UserKnownHostsFile`。无配置格式变化、无键位变化。

### 新增

- **`--ignore-host-key-changes` 全局开关**。开启后接受"已改变"的主机密钥，只记一条
  `WARN knownhosts.ignored_changed`（带服务器实际呈现的 SHA256 指纹），**不写入
  `known_hosts`**；`@revoked` 吊销的密钥仍然拒绝。等价于 `ssh -o StrictHostKeyChecking=no`，
  是排障时的兜底手段，不是默认行为。
- **尊重 `~/.ssh/config` 的 `StrictHostKeyChecking` / `UserKnownHostsFile`，逐跳生效**。
  新增 `src/ssh/hostkey_conf.rs`：`StrictHostKeyChecking no|off` 或
  `UserKnownHostsFile /dev/null` 映射为**该 Host** 的 `HostKeyPolicy::Ignore`；`yes` / `ask` /
  未写保持 sshpf 原有的 `accept-new`（不会因为读配置反而变严格，否则所有没写这条指令的
  用户都会被打断）。作用域（`Host *` 继承、首个匹配块优先）由 `ssh2-config` 按 OpenSSH
  语义处理。策略是**逐跳**的：`ProxyJump` 链上每一跳各用自己的策略，目标主机严格、跳板机
  忽略是常见组合。全局 `--ignore-host-key-changes` 优先，不会被更严格的 `Host` 块削弱。

### 动机

`ssh cloud_pc` 能正常登录，sshpf 建同样的隧道却报 `host key verification failed`。实测根因
**不是 `cloud_pc`，而是它的跳板机那一跳**：用户的 `~/.ssh/config` 里
`Host cloud_pc_jump` 写了 `StrictHostKeyChecking no` + `UserKnownHostsFile /dev/null`，
OpenSSH 因此不校验这一跳；sshpf 用自己的 `known_hosts` 校验器，照样拒绝，日志实证：

```text
ERROR event=knownhosts.rejected host=desk-ssh.srv.deeproute.cn port=80 policy=accept-new
msg="[desk-ssh.srv.deeproute.cn]:80 的主机密钥已改变（known_hosts 第 34 行），可能存在中间人攻击"
```

结论：sshpf 拒绝得没错，但**不该无视用户已经对某台主机表达过的意图**。本次改动把"用户的
意图"和"安全的默认值"分开：默认仍是拒绝（并指出 `known_hosts` 行号），忽略必须显式开启，
且每次降级都留痕。

## [0.3.1] - 2026-11-09

本次是 patch 版本：把「新建 / 编辑转发」这一屏被截断的信息补回来，并修正 Windows 上
DNS 失败被错报为「未知错误」的分类。无新增依赖、无配置格式变化、无键位变化。

### 新增

- **新建/编辑转发弹窗加宽（74 → 110 列）**。原先内宽 70 列时，值列只有 49 列、导入面板
  的「原始指令」列只有 23 列，`RemoteForward 9000 127.0.0.1:9001` 这样的完整指令被画成
  `RemoteForward 9000 12…`，截掉的正是用户辨认一行所依据的那一列；长主机名同样被截断。
  弹窗宽度提为 `src/ui/layout.rs` 里的 `FORM_POPUP_W = 110`，内宽 106 时「原始指令」列
  33 列、正好容纳完整指令。窄终端仍由 `centered` 夹到帧内，所以只增不减；导入面板的
  键位提示也随之从 5 行降到 4 行。

### 修复

- **Windows 上域名解析失败显示「未知错误」。** 系统提示是本地化文本（中文 Windows 为
  「不知道这样的主机。」），按英文措辞匹配的 DNS 判定落空。现在优先按 Winsock 错误码
  （11001–11004）判定，表格显示「域名解析失败」，与系统语言无关。

## [0.3.0] - 2026-11-09

本次是**纯重构版本**：不改变任何外部可观察行为（日志的 `event=` 值、帮助文案、键位、
渲染文本逐字保持），目标是把单体大文件拆成高内聚、低耦合的小模块，并把安全约束
从评审口径变成可执行断言。唯一的行为变化是「不再可能因内部错误 panic」这一类加固。

### 新增

- **界面内帮助全面覆盖，中英双语**。新增 `src/help.rs` 作为帮助目录：14 个上下文
  （主界面、多选模式、表单导航 / 编辑、主机下拉、导入面板、导入列筛选、确认框、详情、
  过滤、帮助自身、鼠标、状态图例、安全与诊断）各一节，每条都是「中文在前、英文在后」。
  覆盖该上下文的**全部**按键、鼠标手势、状态符号与安全规则，不再有"这个键只能从 README
  里看到"的情况。
- **帮助浮层改为数据驱动、分区、按宽度折行**。浮层由 `help::SECTIONS` 渲染，分区标题
  `中文 · English`；条目按当前终端宽度折行（放不下时英文换到续行并缩进到键列之后），
  滚动上限按**折行后的实际行数**计算，到底不卡住也不留白。`src/ui.rs` 里旧的
  `HELP_ITEMS` 常量与手写 `draw_help` 已删除。
- **底部提示补全并双语**。主界面提示扩为 3 行并列出全部键；多选模式、表单、主机下拉、
  导入面板、确认框、详情、过滤、帮助浮层各自都有双语提示行，且与该上下文真实键位一致。
- **`--help` 双语**。用法、选项、日志环境变量、界面快捷键、导入面板与鼠标操作全部中英并列。
- **新增 `tests/help_coverage.rs`**：用花括号配平解析 `src/keys.rs` 每个 handler 真实
  dispatch 的 `KeyCode` 字面量，与 `src/help.rs` 的帮助目录做**双向**断言——代码 dispatch
  的键必须在帮助里（不得遗漏），帮助写的键必须真的 dispatch（不得撒谎）。另断言
  `HitTarget` 的每个变体都在鼠标一节被说明、帮助目录里每个 token 都出现在
  `docs/keymap.md`。加键忘写帮助、或写了代码里不存在的键，都会让测试失败。
- 新增 [docs/keymap.md](docs/keymap.md)：按上下文分表的完整键位参考（按键 | 中文 |
  English），[docs/design.md](docs/design.md) 增加「界面帮助体系」一节。
- **导入面板自动展开并默认全选**。新建转发时从 `~/.ssh/config` 的主机下拉里接受一个主机，
  该主机声明的 `LocalForward` / `RemoteForward` / `DynamicForward` 会**直接列进**
  「从 ~/.ssh/config 导入转发」面板且默认全部勾选，不再需要先按 `space` / `i` 才看得见。
  需要再按一次键才出现的全选只是藏起来的提议，不是默认值。只有 ≥1 条**可导入**的转发才
  自动展开：全部不可导入（Unix socket 形式、`-R` 无目标）或该主机没有转发时不展开，
  `space` / `i` 仍可展开读原因。同一主机重复刷新不会重置勾选，也不会把手动收起的面板弹回来；
  换到另一台有可导入转发的主机时重新展开。`space` / `i` 现在用于收起与重新展开。

### 修复

- **编辑态按 `ctrl+s` / `f10` 不保存，还会把 `s` 打进字段**。保存只在表单导航态生效，
  编辑态下 `ctrl+s` 落到通用 `Char(c)` 分支被当成普通字符插进缓冲区（静默的数据损坏），
  `f10` 则完全无反应；而界面提示一直写着"`ctrl+s` 保存"。现在两个键在两阶段都保存。
- **界面此前缺失说明的键**，本次全部补上：`ctrl+c`（等同 `q` 的先确认退出）、`E` / `V`
  别名、`f2` / `ctrl+h` / `ctrl+o`（打开主机下拉）、`f10`（保存）、`enter` 关闭详情、
  `pgup` / `pgdn`（详情、帮助、导入面板、主机下拉）、`home` / `end`、`shift+tab`
  （编辑态提交并回退）、编辑态的 `backspace` / `delete` / `home` / `end`、导入面板的
  `ctrl+r` 清全部筛选、`ctrl+z` / `z` 撤销、`F` 清当前列、`i` 收起面板、`U` / `I`
  全不选 / 反选、以及表单的 `h` / `l` 类型切换。
- **帮助浮层滚动到底会留白或卡住**。帮助改为分区、每条中英同排后，屏幕上的一行可能折成
  两三行，而滚动上限仍按条目数计算。现在上限按**当前宽度下折行后的实际行数**算，且渲染
  与限位共用同一个纯函数，两者不可能不一致。
- **多跳 `ProxyJump` 只用了第一跳**。`ProxyJump a,b` 在解析时被截成 `a`，于是像
  `cloud_pc`（`ProxyJump office_pc,ali_cloud_pc_jump`）这样的主机只会经
  `office_pc` 直连目标；`office_pc` 到目标所在网段没有路由，隧道永远停在
  `Failed to open channel (ConnectFailed)` 并不断重试。现在保留完整的跳板机列表，
  并按 OpenSSH 语义展开：第一跳自己的 `ProxyJump` 前置展开，其余跳原样跟随；
  配置成环时停止展开，不会无限循环。
- **靠 ssh-agent 认证的跳板机会 panic**。`load_auth` 在隧道的 runtime 里又建了
  一个 runtime 并 `block_on`，命中 `Cannot start a runtime from within a runtime`；
  多跳链路里凡是没写 `IdentityFile` 的中间跳板机都会在认证前崩溃。改为在现有
  runtime 里直接 `await` agent 连接。
- **接受 `none` 认证的跳板机连不上**。OpenSSH 会先试 `none`；例如 desk-ssh 网关
  接受它、收到公钥反而拒绝。现在同样先试 `none`，被拒后再用密钥 / ssh-agent。
- 认证失败的原因带上 `host:port`，多跳时能看出是哪一跳拒绝了密钥。
- **面板默认展开后会吞掉正在输入的字母**。面板常驻屏幕上，`j`/`k`/`a`/`i` 原本无条件归面板，
  于是在名称里输入 `api-db` 会在打字过程中勾选全表并关掉面板。现在编辑态下键位归字段，
  退出编辑后行导航才恢复。
- **收起面板后无法手填单条转发**。`ctrl+s` 的批量判据从「导入表非空」改为「面板可见」：
  用户故意收起面板、想按手填字段建一条单条隧道时，原先会永远得到「没有勾选任何转发」。
  原则仍是校验必须对着用户正在看的那份界面。
- **主机字段为空时按全局转发扫描**。`Host *` 会匹配空串，把全局转发列在用户没选过的别名下，
  `ctrl+s` 会真的照这些行建隧道。现在空字段不扫描、不展开。
**不再有裸 panic：所有可达崩溃都消除了，异常一律进诊断日志。**

以前输入不合法时会直接把进程打崩，用户看到的是一段 Rust 栈而不是一句人话，退出码是
101（panic），日志里只有 `crash.panic` 没有原因。现在这些场景都能正常退出，并把原因写成
结构化记录：

- **`~/.ssh/config` 第一行是中文注释就崩。** 任何非 ASCII 开头都会（`#中文注释`、`#é`、
  `#😀`），报 `end byte index 5 is not a char boundary`。原因是识别 `Match` 关键字时按
  字节位置切了字符串，第 5 个字节正好落在中文字符中间。
- **没有终端就崩。** CI、`ssh host sshpf` 不带 `-t`、stdin 被重定向时，程序以 101 退出并
  打出一段 `failed to initialize terminal` 的栈。现在以 1 退出、写 `app.fatal stage=tui`，
  并在 stderr 说一句「错误: …」。
- **命令参数里有非 UTF-8 字节就崩。** `-c` 后面跟一个非 UTF-8 路径直接 panic。现在路径按
  字节原样接受；作参数名时是正常的用法错误（退出码 2）。
- **输出写不出去就崩。** `sshpf --help > /dev/full`、`--list-hosts | head -1`（管道提前
  关闭）、磁盘满，都会 panic 在 `print!` 里。现在记 `cli.stdout_failed`，退出码保持该模式
  的真实结果。
- **`config.toml` 里手写 `id = 18446744073709551615` 后新建转发就崩。** 算下一个 id 时
  `+ 1` 溢出。现在回落到空闲 id，并记 `app.id_space_exhausted`。
- **超长输入就崩。** 64K 字符的过滤词/字段值、极宽终端下的导入面板、极窄（≤4 列）终端下
  打开确认框，分别溢出 `u16` 和除零。现在全部正常渲染，被降级的那一块记
  `ui.render_degraded`（每处只记一次，不会刷屏）。
- **崩溃处理机制自己会挂。** panic 时如果日志锁正好被持有（例如某个值格式化时 panic），
  panic hook 会等自己那把锁，进程**永久僵住**、日志 0 字节、终端卡在备用屏。现在 hook 拿不到
  锁就跳过该条记录，改用预 `dup` 的 fd 裸写一行 `crash.log_reentrant`，然后照常退出。
- **隧道线程 panic 会卡住整行。** 以前该行永远停在「启动中」，没人知道为什么。现在兜住并
  转成失败态，记 `tunnel.thread_panic`，UI 和其余隧道继续跑。

新增 `tests/panic_hardening.rs`（13 条命令行黑盒回归，断言子进程**不是被信号杀死**且日志里
有对应事件）；[docs/logging.md](docs/logging.md) 增加「panic 加固」一节、13 个新事件和按症状
索引的排查配方。

### 重构

- **大文件拆分为按职责命名的子模块**，公共 API 路径保持不变（父模块保留 `mod` + `pub use`）：
  - `src/model.rs`（2532 行）→ `model/{tunnel,import,batch,failure,status,form,picker,msg,match}.rs`
  - `src/ui.rs`（3095 行）→ `ui/{palette,text,frame,header,hints,table,selection,help,form,dropdown,confirm,detail,filter}.rs`；
    `ui/import.rs` 再拆 `import/{body,header,footer,row,cursor}.rs`
  - `src/logging.rs`（1522 行）→ `logging/{level,sink,format,panic,crash}.rs`
  - `src/keys.rs`（1828 行）→ `keys/{main,select,form,import,dropdown,misc}.rs`
  - `app/log.rs`（727 行，已删除）→ 顶层 `src/eventlog.rs` + `eventlog/{process,tunnel,ssh,user}.rs`
  - `src/ssh/client.rs`（814 行）→ `ssh/client/{classify,handler,auth,connect,driver}.rs`
  - `src/main.rs` → `main.rs`（入口 + 事件循环）+ `cli.rs`（参数解析与 `USAGE`）
- **修掉依赖倒置**：`ssh/client.rs` 不再依赖 `crate::app::log`（事件词汇上移为顶层
  `eventlog`）；`model` 与 `ui` 不再依赖 `help` 的渲染无关内容。
- **`src/text.rs` 成为唯一字符串层**：显示宽度、截断、填充、双语拼接、双语渐进降级、
  贪心折行、glob 匹配。此前 `ui` / `help` / `ssh` 各有一份拷贝并已漂移。
- **单文件生产行数 ≤ 400、单函数 ≤ 60 行**，由 `tests/safety_guards.rs` 断言（`SIZE_EXEMPT`
  为空，即没有例外）。

### 安全加固

- **`unsafe` 收口到 `src/logging/crash.rs` 一个文件**，文件头写明 SAFETY CONTRACT
  （每块理由、只允许 async-signal-safe 调用、不得分配/加锁）；其余全仓安全 Rust，同样是断言。
- **去掉 `main.rs` 里唯一的 `unsafe`**：信号注册改用 `signal_hook` 的安全 API，
  行为不变（仍只做原子写、仍在循环结束后记录信号名、仍返回 `ExitCode::SUCCESS`）。
- **消除 `src/help.rs` 的 `expect`**：14 个 section 的唯一性改为常量兜底 + 测试断言，不再 panic。
- **`tests/safety_guards.rs`（新增）**：逐文件扫描生产代码，断言无 `.unwrap()` / `.expect()` /
  `panic!` / `unreachable!` / `todo!`、`unsafe` 只出现在 `*crash*.rs` 且每块有 `// SAFETY:`、
  每个源文件有 `//!` 模块文档、`src/lib.rs` 的每个 `pub mod` 都有真实文件。
- `logging/crash.rs` 新增单元测试作为 `unsafe` 的看护证据：`build_signal_record` 的格式/截断/
  边界（`i32::MIN`）、`push_int` 边界、`signal_name` 全覆盖。

### 文档

文档按「**AI agent 是主要读者**」重建信息架构：入口是 [docs/index.md](docs/index.md)
（表格式「我想…→ 读哪篇 / 看哪个文件」），每篇开头 3–8 行写明「回答什么问题 / 不负责什么 /
相关代码路径」，单篇 ≤ 400 行，同一事实只详述一处。

- 新增 [docs/index.md](docs/index.md)（agent 入口）、[docs/architecture.md](docs/architecture.md)
  （分层、逐文件模块地图、依赖方向规则、新功能放哪）、[docs/runtime.md](docs/runtime.md)
  （隧道生命周期/就绪/重试/事件泵）、[docs/tui.md](docs/tui.md)（渲染与交互）、
  [docs/security.md](docs/security.md)（panic/unsafe 政策、崩溃捕获、权限、脱敏）、
  [docs/testing.md](docs/testing.md)（测试分层与怎么跑）、[docs/agents.md](docs/agents.md)
  （给 agent 的改码约定与 checklist）、[docs/logging/events.md](docs/logging/events.md)（事件目录整表）。
- [docs/design.md](docs/design.md) **文件名保留**（`src/` 多处引用），内容重写为「设计决策索引」：
  30 条决策各 1 段 + 链接到新文档，不再与 README / import-forwards 重复叙述。
- [docs/import-forwards.md](docs/import-forwards.md)、[docs/logging.md](docs/logging.md)
  精简：保留规则、坑与排查配方，例子与长论证收进小节；事件整表搬到 `logging/events.md`。
- [docs/keymap.md](docs/keymap.md) 重排（加开头路标与格式约定），**未删除任何按键 token**
  ——它被 `tests/help_coverage.rs` 用 `include_str!` 校验，删 token 会让测试失败。
- [docs/research/README.md](docs/research/README.md)（新增）声明该目录是**历史调研，不是当前事实来源**。
- [README.md](README.md) 精简到用户视角（是什么 / 装 / 用 / 风险 / 配置 / 排查 / 开发），
  内部结构一律链接 [docs/index.md](docs/index.md)；删掉与 `design.md`、
  `import-forwards.md` 互相复制的大段内容。
- 修正 v0.2.1 文档里记录但**代码从未 emit** 的 `tunnel.state` 事件：状态变化由
  `tunnel.ready` / `tunnel.exit` / `tunnel.retry_scheduled` / `tunnel.failed` 表达。

## [0.2.1] - 2026-11-08

### 新增

- **结构化诊断日志**。每次运行向 `~/.local/state/sshpf/logs/sshpf.log` 追加
  一行一条的记录：固定列顺序（RFC 3339 时间戳、级别、`seq`、`event`）+ `key=value`
  字段，值含空白/引号/控制字符时自动加引号转义，因此一条记录永不跨行。
  覆盖启动退出、配置读写、主机解析、隧道启停/就绪/重试/终态失败、主机密钥决策、
  目标端口不可达、用户确认闸的放行与取消、批量操作的实际生效条数、UI 卡帧。
  单文件 2 MiB 轮转、保留 3 个归档（上限约 8 MiB），目录 `0700` 文件 `0600`。
  `SSHPF_LOG_DIR` 换目录、`SSHPF_LOG_LEVEL=off|error|warn|info|debug` 换级别。
- **崩溃写入日志**。panic 记录线程名、源码位置、消息与前 25 帧栈（隧道线程崩了
  与 UI 崩了能区分开）；SIGSEGV/SIGBUS/SIGABRT/SIGILL/SIGFPE 用 `sigaction` 在
  处理器里直接 `write(2)` 一条记录，同时退出备用屏、往 stderr 提示路径，最后恢复
  默认处理并重新 `raise`，退出状态与 core dump 行为保持不变。
- `--help` 增加日志一节；新增 [docs/logging.md](docs/logging.md)（事件目录、
  字段说明、排查配方）。

### 修复

- **`--help` 没有列出本次新增的键与能力**：`v` 多选模式、导入面板的入口
  （`space`/`i`）以及鼠标操作都只能从 README 里看到。现在 `--help` 直接说明。

## [0.2.0] - 2026-11-08

### 新增

- **从 `~/.ssh/config` 导入转发**。新建转发时选中主机，表单会列出该主机已声明的
  `LocalForward` / `RemoteForward` / `DynamicForward`，**默认全部勾选**，取消掉不要的
  再保存。N 条勾选行生成 N 条隧道（`TunnelConfig` 只描述一条转发），整批先校验、
  全过才写。勾选只是内存里的意图，确认之后才真正建连。
- **导入面板的逐列筛选**：类型 / 监听 / 目标 / 原始指令四列各一个筛选，AND 组合、
  大小写不敏感子串匹配；单列清除、全部清除、LIFO 撤销。"全选 / 反选 / 全不选"
  只作用于筛选后的可见行。
- **主表多选模式**（`v`）：标记若干条后批量启用 / 停用 / 重启 / 删除。标记按
  `TunnelId` 记录而非行号，四种批量操作都走既有的确认闸。
- **鼠标操作**：点击行选中、点击导入表的勾选框、点击列头开始筛该列、点击筛选
  chip 的 `×` 删除、点击 `清除全部筛选` / `撤销筛选` / 全选类按钮、滚轮滚动列表、
  多选模式下点击动作条按钮、`shift` 点击扩选范围。

### 修复

- **导入的特权端口会落到另一个特权端口**。`LocalForward 80 …` 原本向上扫描落在
  280，`bind` 直接 permission denied。本地端口规则收敛到 `model::plan_local_port`，
  表单与导入共用一份实现（两者已经漂移过一次）。
- **全部取消勾选后保存报"名称不能为空"**，描述的是用户根本没在看的字段。
- **`help` / `confirm` / `detail` 弹窗点击穿透**：三个弹窗未记录自身命中矩形，
  点击会命中其下层的表格行（确认框开着时点一下就把别的行标记了）。
- **`f` 不循环筛选列**，永远回到"类型"，让 UI 页脚的 `f/←→ 选列` 成为假话。
- **多选模式仍显示单行快捷键**：模式是重绑键而非追加，第一行提示还写着
  `space 启停` / `r 重启`。

## [0.1.1] - 2026-11-08

### 修复

- **Linux 预编译二进制改为静态链接**。0.1.0 的 `sshpf-linux-x86_64` 在
  `ubuntu-latest`（glibc 2.39）上构建且动态链接，在 glibc 较旧的系统上
  直接报错无法启动：
  ```
  version `GLIBC_2.32' not found
  version `GLIBC_2.34' not found
  weak version `GLIBC_2.39' not found
  ```
  现改用 `x86_64-unknown-linux-musl` 静态链接，任意发行版可直接运行。
  发布流程新增校验步骤，产物若非静态链接则构建失败，防止回归。
  （用 0.1.0 的源码 `cargo install sshpf` 不受影响。）

### 说明

- 0.1.0 的源码本身没有缺陷，此版本仅修复发布资产。

## [0.1.0] - 2026-11-08

首个公开发布。

### 新增

- 纯 Rust SSH 实现（`russh`），不再调用系统 `ssh` 子进程，Linux/Windows
  走完全相同的代码路径。
- 三种转发：`-L` 本地、`-R` 远端、`-D` 动态 SOCKS5（内置 SOCKS5 服务端）。
- TUI 表格界面：启停、编辑、删除、过滤、详情与完整日志；配置持久化，
  启动时恢复上次启用的条目。
- 主机来自 `~/.ssh/config`，支持 `Include`、`Host` 通配、`!` 否定、
  `Host *` 继承、`ProxyJump`。
- 自行实现的 `known_hosts` 校验：`*`/`?` 通配、`!` 否定、`[host]:port`、
  `|1|` 哈希条目、`@revoked` 优先拒绝；默认策略 `accept-new`。
- 结构化失败原因分类（端口占用、权限不足、认证失败、DNS 失败、
  主机密钥校验失败等），网络类失败自动退避重连。
- 所有破坏性操作弹确认框。

### 已知限制

- 端口占用反查进程仅 Linux 可用（依赖 `/proc`）。
- 仅支持公钥 / ssh-agent 认证，TUI 内不接管密码与 2FA 输入。
- 不统计流量；同一主机多条转发各自建连，未做连接复用。
- macOS 走 Unix 分支，未实测。

[Unreleased]: https://github.com/can2049/sshpf/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/can2049/sshpf/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/can2049/sshpf/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/can2049/sshpf/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/can2049/sshpf/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/can2049/sshpf/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/can2049/sshpf/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/can2049/sshpf/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/can2049/sshpf/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/can2049/sshpf/releases/tag/v0.1.0
