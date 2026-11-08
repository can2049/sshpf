# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

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

[Unreleased]: https://github.com/can2049/sshpf/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/can2049/sshpf/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/can2049/sshpf/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/can2049/sshpf/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/can2049/sshpf/releases/tag/v0.1.0
