# sshpf

[![CI](https://github.com/can2049/sshpf/actions/workflows/ci.yml/badge.svg)](https://github.com/can2049/sshpf/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/sshpf.svg)](https://crates.io/crates/sshpf)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

SSH 端口转发管理器，命令行 TUI 表格界面。Linux 与 Windows 均支持。

在不离开终端的情况下查看、新增、启停、删除端口转发，每条转发实时显示状态与失败原因；配置持久化，重启后自动恢复上次启用的条目。

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
│  sshpf 0.2.1  ·  ssh-config: 12 hosts  ·  cfg: ~/.config/sshpf/config.toml          13:47:03    │
│  ● 2 up   ◐ 0 starting   ◌ 0 retrying   ✗ 1 failed   ○ 1 off                                     │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│   名称          T 本地             远端             SSH 主机    状态        最后错误             │
│ ▶ web-ssh       L 127.0.0.1:19711  127.0.0.1:22    web-server  ● 已连通    —                    │
│   staging-db    L 127.0.0.1:15432  127.0.0.1:5432  staging-db  ✗ 失败      主机密钥校验失败      │
│   old-tunnel    L 127.0.0.1:18080  127.0.0.1:8080  dev-box     ○ 未启用    —                    │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│  就绪 · n 新建转发 · ? 帮助 · q 退出                                                              │
│  j/k ↑↓ 移动  space 启停  r 重启  n 新建  e 编辑  d 删除  enter 详情  l 日志                      │
│  / 过滤  a 全启  x 全停  g/G 首尾  v 多选  ? 帮助  q 退出                                          │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

`v` 进入多选模式：底部多出一条动作条，标记若干条后一次启停/重启/删除。

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
│  名称           T 本地              远端              SSH 主机       状态        最后错误         │
│  ✓ web-ssh      L 127.0.0.1:19711   127.0.0.1:22      web-server     ● 已连通    —              │
│  ✓ staging-db   L 127.0.0.1:15432   127.0.0.1:5432    staging-db     ✗ 失败      主机密钥校验失败 │
│ ▶✓ old-tunnel   L 127.0.0.1:18080   127.0.0.1:8080    dev-box        ○ 未启用    —              │
│  · socks        D 127.0.0.1:1080    (SOCKS5)          dev-box        ◐ 连接中    —              │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│ 已选 2 条  ·  s 启用  ·  x 停用  ·  R 重启  ·  D 删除  ·  a 全选  ·  v/esc 退出                    │
│ j/k ↑↓ 移动  space 勾选  J/K 扩选  g/G 首尾                                                       │
│ a 全选  s/x/R/D 批量操作  esc 退出  ? 帮助                                                        │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

## 设计要点

**SSH 由 Rust 直接实现，不再调用系统 `ssh` 子进程。** 用的是 [`russh`](https://crates.io/crates/russh)（纯 Rust SSH 协议栈）。这样 Linux 和 Windows 走**完全相同的代码路径**，行为一致；而且失败原因来自结构化错误，不再靠正则解析 `ssh -v` 的 stderr 文本。

**主机来自 `~/.ssh/config`，转发条目由本工具管理。** 职责分离：ssh config 负责「怎么连」，sshpf 负责「转发什么」。不会往 `~/.ssh/config` 写入任何内容。`Include`、`Host` 通配、否定、`Host *` 继承、`ProxyJump` 都由 [`ssh2-config`](https://crates.io/crates/ssh2-config) 解析（它名字里的 "ssh2" 有误导性，实际是纯 Rust、零 C 依赖）。

**每条转发 = 一个后台线程 + 独立的 tokio runtime。** UI 线程保持同步（`crossterm::event::poll(200ms)`），事件通过 `std::sync::mpsc` 回灌，所以界面永不被网络阻塞。三种转发都由 SSH 协议原语实现：

- `-L`：本地 `TcpListener` + `direct-tcpip` channel
- `-R`：`tcpip-forward` 请求 + `forwarded-tcpip` 回调
- `-D`：内置 SOCKS5 服务端（自己写的，约 150 行，含 IPv4/域名/IPv6）

**就绪判定不看日志，看真实结果。** `-L`/`-D` 的"就绪"是 `TcpListener::bind` 真的成功了；`-R` 的"就绪"是 `tcpip_forward()` 等到了服务端的协议回复。旧版本靠 stderr 里的 `Entering interactive session.` / `remote forward success for` 判断，有两个实测过的误判场景（ProxyJump 子进程的行会混入、`-R` 的成功标记晚于 `Entering`）——换实现后这类 bug 从根上消失。

**主机密钥校验自己实现。** `russh` 自带的 known_hosts 校验实测有缺陷（不支持通配符、Tab 分隔漏判，且 `@revoked` 与正常条目并存时**会放行被吊销的密钥**），所以匹配逻辑自己写：支持 `*`/`?` 通配、`!` 否定、`[host]:port`、`|1|` 哈希条目、`@revoked` 优先拒绝。默认策略 `accept-new`（首次连接自动记录，密钥变更则拒绝）。

**失败原因分类来自结构化错误。** 不再解析 stderr：`ConnectionRefused`/`Timeout`/`AddrInUse`/`PermissionDenied`/`RequestDenied` 等直接来自 `russh::Error` 变体。主机密钥被拒时会明确显示是"已改变"还是"已吊销"，并指向 known_hosts 的行号。

**转发被拒绝时主动停止会话。** 会话建立但转发请求被拒（`-R` 端口被占等）时，隧道是没用的；主动停掉，界面和现实才一致。

**`~/.ssh/config` 里的转发可以直接导入，不必重打一遍。** 新建转发时选好主机，表单会列出该主机已声明的 `LocalForward` / `RemoteForward` / `DynamicForward`，**默认全部勾选**，取消掉不要的再保存。保存时一条勾选行生成一条隧道（`TunnelConfig` 只描述一条转发），所以 N 行就是 N 条，命名为 `<host>-<L|R|D>-<端口>` 并与已有名字去重。

作用域按 OpenSSH 语义：**多个 `Host` 块都命中时全部累积**（`Host web` 和 `Host *` 都贡献转发），否定子句 `!prod` 排除整块，通配 `*`/`?` 大小写不敏感，第一个 `Host` 行之前的指令视为全局。`Match` 块里的转发**一行都不导入**——它们带连接上下文前置条件，当成无条件生效等于在用户设了闸的地方静默开端口。`Include` 会被跟进（有总文件数上限和环检测）。同一条转发被多个块重复声明时去重，否则两条隧道会抢同一个端口。

Unix socket 形式和 `-R` 无目标的转发**列出但不勾选**，变暗并写明原因（`⚠ 端口无法解析` / `⚠ 目标地址无法解析`）——默默隐藏会让人以为工具漏读了配置。默认全选只是**意图**，不是监听：隧道在你按 `ctrl+s` 确认之后才创建。详见 [docs/import-forwards.md](docs/import-forwards.md)。

**导入表可以按列筛选，且筛选可撤销。** 四列（类型 / 监听 / 目标 / 原始指令）各有独立筛选，多个筛选 AND 到一起。匹配用大小写不敏感**子串**，刻意不用主表的模糊子序列匹配——模糊匹配端口列会把 `1080` 匹配到一堆无关行，而筛选端口列的目的是精确定位一行。`全选`/`反选`/`全不选` **只作用于筛选后可见的行**：勾上隐藏行会让“看起来勾了 3 条”变成“实际创建了 8 条隧道”。筛选改动可逐个 `z` 撤销（快照栈，上限 64）。

**多选按 `TunnelId`，不按行号。** 过滤和删除都会让行移动；标记若存行号，一次删除后“第 3 行”就指向另一条隧道，批量删除会删错东西。四种批量操作（启用/停用/重启/删除）**都过确认闸**，提示用有效条数——标记 5 条但只有 1 条在跑时，停用提示说 1 条。

**鼠标走语义命中图，不重算坐标。** 渲染器每帧把“这块矩形是什么意图”记进 `src/ui/hit.rs` 的 `HitMap`（点列头解析为 `ImportColumnHeader(col)`，不是 `x=23`）。三条规则：只有渲染器写、后 push 的赢、零面积矩形忽略。后 push 的赢意味着**弹窗后画就自动遮蔽下层表格**，不需要任何 z-index 记账——绘制顺序本来就表达了层级。之所以不存坐标：渲染器已经算过布局，在事件处理里再推导一遍就是第二个真相来源，改个列宽鼠标就点错而键盘完全正常。鼠标调用的是和键盘同一批 `App` 方法，所以**免费继承**键盘的安全规则（点批量删除按钮一样弹确认框）。点击落空什么都不做，不“透传”给下面的控件。

## 安装

从 crates.io 安装（需要 Rust 1.89+）：

```bash
cargo install sshpf
```

预编译二进制可从 [GitHub Releases](https://github.com/can2049/sshpf/releases) 下载（Linux x86_64 / Windows x86_64）。
Linux 版是静态链接（musl），不依赖系统 glibc 版本，任意发行版可直接运行。

从源码构建：

```bash
git clone https://github.com/can2049/sshpf.git
cd sshpf
cargo build --release
install -m755 target/release/sshpf ~/.local/bin/sshpf
```

也可以直接装 git 上的最新版：

```bash
cargo install --git https://github.com/can2049/sshpf
```

**运行时零外部依赖**：不再需要系统 `ssh`。认证走 `~/.ssh/config` 里的 `IdentityFile` 或 ssh-agent。

构建期需要 C 工具链（`ring` 加密后端，Windows 上即预装的 MSVC），因为密码学汇编需要编译；产物本身是自包含的。

| 平台 | 状态 | 说明 |
|---|---|---|
| Linux | ✅ 完整支持 | 端到端测试跑真实 sshd |
| Windows | ✅ 支持 | 同一套纯 Rust 代码路径，无需安装 OpenSSH |
| macOS | ⚠️ 未测试 | 代码走 Unix 分支，预期可用 |

CI 在 `ubuntu-latest` 与 `windows-latest` 上分别编译并运行单元测试。

## 使用

```bash
sshpf                       # 启动 TUI，自动恢复上次启用的转发
sshpf --no-restore          # 启动但不自动恢复
sshpf -c /path/config.toml  # 使用指定配置
sshpf --list-hosts          # 打印 ~/.ssh/config 里的主机后退出
sshpf --check               # 校验配置文件后退出
```

### 快捷键 / Key bindings

界面里**任何时刻**按 `?` 都能看到当前上下文的按键说明，中英双语，且与本文一致。
完整键位参考（按上下文分表）见 [docs/keymap.md](docs/keymap.md)。

Press `?` at **any** moment to see the keys for the current context, in Chinese and
English, guaranteed to match this page. The full reference is
[docs/keymap.md](docs/keymap.md).

#### 主界面 / Main screen

| 键 / Key | 中文 | English |
|---|---|---|
| `j` / `k`、`↓` / `↑` | 移动光标 | Move the cursor |
| `g` / `G`、`home` / `end` | 跳到首行 / 末行 | Jump to the first / last row |
| `space` | 启用 / 停用（运行中先确认） | Enable / disable (asks if running) |
| `r` | 重启（运行中先确认，未运行直接起） | Restart (asks if running, starts if not) |
| `n` | 新建转发 | Create a tunnel |
| `e` / `E` | 编辑（两个键同效） | Edit (`e` and `E` are aliases) |
| `d` | 删除（需确认） | Delete (asks first) |
| `enter` / `l` | 详情 + 完整日志 | Detail + full log |
| `/` | 过滤 | Filter |
| `a` / `x` | 全部启用 / 全部停用（需确认） | Enable all / disable all (ask first) |
| `v` / `V` | 进入 / 退出多选模式（两个键同效） | Enter / leave selection mode (aliases) |
| `esc` | 清除过滤 | Clear the filter |
| `?` | 打开 / 关闭帮助 | Open / close the help |
| `q` / `ctrl+c` | 退出（有运行中转发时确认；`ctrl+c` 等同 `q`，不会直接退出） | Quit (asks when tunnels run; `ctrl+c` equals `q` and never quits outright) |

#### 多选模式 / Selection mode

按 `v` 进入。部分键**被重新绑定**，不是叠加：

Entered with `v`. Some keys are **re-bound**, not added:

| 键 / Key | 中文 | English |
|---|---|---|
| `space` | 勾选 / 取消光标行（不是启停） | Mark / unmark the row (not start / stop) |
| `j` / `k`、`↓` / `↑` | 移动光标，标记保留 | Move the cursor; marks stay |
| `g` / `G`、`home` / `end` | 跳到首行 / 末行 | Jump to the first / last row |
| `J` / `K`、`shift+↓` / `shift+↑` | 从锚点扩选到光标 | Extend the marks from the anchor |
| `a` | 全选**可见**行；再按一次全部取消 | Toggle all **visible** rows |
| `s` / `x` / `R` / `D` | 批量 启用 / 停用 / 重启 / 删除（均需确认） | Batch start / stop / restart / delete (all ask first) |
| `v` / `V` / `esc` | 退出（清空标记） | Leave (marks are cleared) |

`R`/`D` 用大写是为了不与单行的 `r`/`d` 混淆。多选模式下 `space` 不再启停、`r`/`d`/`n`/`e` 不生效，
所以底部两行提示会整体换掉——留着旧提示等于广告一批会做别的事或什么都不做的键。

`R`/`D` are uppercase so they cannot be confused with single-row `r`/`d`. Here `space`
never starts or stops a tunnel and `r`/`d`/`n`/`e` do nothing, so the footer is replaced
entirely — leaving the old hints would advertise keys that now do something else or nothing.

#### 表单 / Form

表单采用**两阶段编辑**（详见下文「新增转发：两阶段编辑 + 自动推荐本地端口」）。
The form has **two editing stages**.

导航态（默认，方向键只移动高亮，不改值）/ Navigating (the default; arrows only move the highlight):

| 键 / Key | 中文 | English |
|---|---|---|
| `tab` / `shift+tab`、`↓` / `↑` | 移动字段高亮 | Move the field highlight |
| `enter` | 进入编辑（`类型` 循环、`自动重连` 取反、`SSH 主机` 展开下拉） | Start editing (cycle type, toggle auto-restart, open host list) |
| `←` / `h`、`→` / `l` | `类型` 切上一个 / 下一个（L/R/D）；`自动重连` 取反 | On type: previous / next kind; on auto-restart: toggle |
| `space` | `类型` / `自动重连` 切换；`SSH 主机` 展开 / 收起导入面板 | Cycle type, toggle auto-restart, toggle the import pane |
| `i` | `SSH 主机` 字段：展开 / 收起导入面板 | On SSH host: toggle the import pane |
| `f2` / `ctrl+h` / `ctrl+o` | `SSH 主机` 字段：打开主机下拉（三个键同效） | On SSH host: open the dropdown (three aliases) |
| `ctrl+s` / `f10` | 保存（新建直接连；编辑先确认，运行中会重建） | Save (create connects at once; edit asks first) |
| `esc` | 取消（有未保存内容会确认） | Cancel (asks when there are unsaved edits) |

编辑态（某个文本字段正在接收输入）/ Editing (a text field is taking input):

| 键 / Key | 中文 | English |
|---|---|---|
| 任意字符 | 插入到光标处 | Type to insert at the cursor |
| `backspace` / `delete` | 删除光标前 / 后一个字符 | Delete before / after the cursor |
| `←` / `→` | 光标左移 / 右移 | Move the cursor left / right |
| `home` / `end` | 光标到行首 / 行尾 | Cursor to the start / end |
| `enter` / `esc` | 结束编辑，留在本字段 | Stop editing, stay on this field |
| `tab` / `shift+tab` | 结束编辑并切到下一 / 上一字段 | Stop editing and change field |
| `ctrl+s` / `f10` | 保存（结束字段编辑后提交，不会把 `s` 打进字段） | Save (ends the field edit, then submits; never types `s`) |

> **修复**：编辑态下 `ctrl+s` / `f10` 现在能保存了。此前 `ctrl+s` 会把字母 `s` 插进正在
> 编辑的字段（静默的数据损坏），`f10` 完全无反应——而界面提示一直写着「`ctrl+s` 保存」。
> 现在两个阶段行为一致：先结束字段编辑，再提交表单。
>
> **Fix**: `ctrl+s` / `f10` now save while a field is being edited. Previously `ctrl+s`
> inserted the letter `s` into the field (silent data corruption) and `f10` did nothing at
> all, while the footer advertised `ctrl+s`. Both stages now behave the same: end the field
> edit, then submit.

SSH 主机字段按 `enter` 展开下拉，`space` / `i` 展开「从 `~/.ssh/config` 导入转发」面板。
`enter` on the SSH host field opens the dropdown; `space` / `i` open the import pane.

#### 导入面板 / Import pane

面板展开后，键位**归面板**：`↑`/`↓`、`j`/`k`、`g`/`G`、`home`/`end`、`pgup`/`pgdn` 移动的是导入行，
不再是表单字段；`tab` 仍然切表单字段，`ctrl+s` 仍然保存。

Once the pane is open its own keys win: `↑`/`↓`, `j`/`k`, `g`/`G`, `home`/`end`,
`pgup`/`pgdn` move import rows, not form fields. `tab` still changes fields and `ctrl+s`
still saves.

| 键 / Key | 中文 | English |
|---|---|---|
| `j` / `k`、`↓` / `↑` | 移动行 | Move the row |
| `g` / `G`、`home` / `end` | 首 / 尾行 | First / last row |
| `pgup` / `pgdn` | 向上 / 向下翻页 | Page up / down |
| `space` | 勾选 / 取消当前行 | Tick / untick the current row |
| `a` / `U` / `I` | 全选 / 全不选 / 反选（**只作用于筛选后可见的行**） | Tick all / none / invert (**visible rows only**) |
| `f` / `←` `→` | 切换正在筛选的列（`f` 循环：类型→监听→目标→原始指令→类型） | Switch filter column (`f` cycles) |
| 直接输入 | 追加到当前列筛选（`backspace` 删字符） | Append to the current filter (`backspace` deletes) |
| `enter` | 结束筛选输入（面板不收起） | End filter input (the pane stays open) |
| `F` | 清除当前列的筛选（没有筛选时提示「没有筛选可清除」） | Clear this column's filter (says so when there is none) |
| `ctrl+r` | 清除全部筛选 | Clear every filter |
| `ctrl+z` | 撤销上一次筛选改动 | Undo the last filter change |
| `z` | 撤销上一次筛选改动（LIFO） | Undo the last filter change (LIFO) |
| `esc` | 第一次退出筛选输入；不在输入时收起面板 | Leave the filter first; otherwise close the pane |
| `i` | 收起面板 | Close the pane |
| `ctrl+s` | 保存：勾选 N 行生成 N 条隧道 | Save: one tunnel per ticked row |

`space` 只能**展开**面板：面板打开后 `space` 已经被「勾选当前行」占用，再按一次会勾行而不是收起。
收起用 `i` 或 `esc`，与展开它的键对称。代价是字面字符 `f` 无法输入到筛选里（`f` 优先循环列），
但每列的内容都是端口、主机名或指令名，`←`/`→` 加退格足够覆盖。

`space` can only **open** the pane: once open it means "tick this row", so a second press
ticks instead of collapsing. Close it with `i` or `esc`, symmetric with the key that opened
it. The cost is that a literal `f` cannot be typed into a filter (`f` always switches
columns), but every column holds a port, a host or a directive name, so `←`/`→` plus
backspace cover the rest.

#### 详情 / 过滤 / 帮助浮层 / Detail, filter and help overlay

| 上下文 / Context | 键 / Key | 中文 | English |
|---|---|---|---|
| 详情 / 日志 | `j` / `k`、`↓` / `↑`、`pgup` / `pgdn` | 滚动 | Scroll |
| 详情 / 日志 | `q` / `esc` / `enter` / `l` | 关闭返回 | Close and go back |
| 过滤输入 | 任意字符 / `backspace` | 即时过滤 / 删字符 | Filter live / delete a character |
| 过滤输入 | `enter` / `esc` | 保留过滤关闭 / 清除过滤关闭 | Keep the filter / clear it and close |
| 帮助浮层 | `j` / `k`、`↓` / `↑`、`pgup` / `pgdn` | 滚动 | Scroll |
| 帮助浮层 | `?` / `esc` / `q` / `enter` | 关闭帮助 | Close the help |
| 确认框 | `y` / `Y` / `enter` | 执行 | Run the action |
| 确认框 | `n` / `N` / `esc` | 取消 | Cancel |
| 主机下拉 | `↑` / `↓`、`j` / `k`、`pgup` / `pgdn`、`home` / `end` | 移动 / 翻页 / 首尾 | Move / page / first-last |
| 主机下拉 | `enter` / `esc` / `backspace` / 任意字符 | 选定 / 收起 / 删查询字符 / 过滤主机 | Pick / collapse / delete / filter hosts |

### 界面帮助 / In-app help

按 `?` 打开**分区帮助**：主界面、多选模式、表单（导航 / 编辑）、主机下拉、导入面板、
导入列筛选、确认框、详情、过滤、帮助自身、鼠标、状态图例、安全与诊断——每个上下文一节。
每一条都是**中文在前、英文在后**，且与真实键位逐一对应。

Press `?` to open the **sectioned help**: main screen, selection mode, form (navigating and
editing), host dropdown, import pane, import column filter, confirmation, detail, filter,
the help itself, mouse, status legend, and safety. Every row is Chinese first, English
second, and matches the real key map.

- 帮助内容不是手写散文，而是 `src/help.rs` 里的**数据**（`SECTIONS`）。浮层渲染它，
  界面底部提示复用同一套措辞，所以"页脚说的"和"`?` 说的"不可能互相矛盾。
  The help is **data** (`SECTIONS` in `src/help.rs`), not prose. The overlay renders it and
  the footer hint lines reuse the same wording, so "what the footer says" and "what `?`
  says" cannot disagree.
- 浮层按当前终端宽度折行，滚动上限按**折行后的实际行数**计算，到底不会卡住也不会留白。
  It wraps to the terminal width and scrolls by the wrapped line count, so it neither sticks
  nor leaves blank space at the end.
- 浮层内 `j` / `k` / `pgup` / `pgdn` 滚动，`?` / `esc` / `q` / `enter` 关闭。
  Scroll with `j` / `k` / `pgup` / `pgdn`, close with `?` / `esc` / `q` / `enter`.
- 同一份数据还由 `tests/help_coverage.rs` **机器校验**：解析 `src/keys.rs` 里每个 handler
  真正 dispatch 的键，与帮助目录双向比对。加键忘写帮助会测试失败，写了代码里不存在的键
  也会失败——见 [docs/keymap.md](docs/keymap.md) 末尾。
  The same data is checked by `tests/help_coverage.rs`: it parses the keys each handler in
  `src/keys.rs` really dispatches and compares them **both ways** with the catalog. A key
  added without help fails the test, and so does a documented key that nothing dispatches.

### 鼠标 / Mouse

终端支持鼠标时（捕获是 best-effort 开启的，失败也不影响键盘）：

When the terminal supports it (capture is best-effort; failing only disables the mouse):

| 位置 / Where | 中文 | English |
|---|---|---|
| 表格行 | 移动光标；多选模式下同时切换该行标记 | Move the cursor; in selection mode also toggle that row's mark |
| 表格行 + `shift` | 从锚点扩选范围 | Extend the marks from the anchor |
| 导入表复选框 | 勾选 / 取消该行 | Tick / untick that row |
| 导入表行其它位置 | 把导入表光标移到该行 | Move the import cursor to that row |
| 导入表列头 | 开始编辑该列筛选 | Start filtering that column |
| 筛选 chip 的 `×` | 删除该列筛选 | Drop that column's filter |
| `全选` / `反选` / `全不选` | 对应操作（只作用于筛选后可见行） | The matching bulk tick (visible rows only) |
| `清除全部筛选` / `撤销筛选` | 对应操作；无内容时变暗且不可点 | Clear / undo filters; dimmed and not clickable when nothing to act on |
| 多选动作条按钮 | 对应批量操作（走确认闸） | The batch operation (still behind the confirmation gate) |
| 列表上的滚轮 | 滚动（主表移动光标，导入表移动其光标） | Scroll (the main table moves the cursor, the import list its own) |

点击落空**不做任何事**——按钮旁边的误点不该启动隧道，也不"透传"给下面的控件。
A click on empty space does **nothing** — a misclick next to a button must not start a
tunnel, and it never falls through to the widget below.

弹窗（确认框 / 帮助 / 详情）会**吞掉**点击，不会穿透到下面的表格：确认框是在破坏性动作
之前拦人的，一次本想点对话框的点击绝不能落到某行上把它标记或启动。
Popups (confirmation, help, detail) **swallow** clicks instead of letting them reach the
table underneath: the confirmation gate must never mark or start a row behind it.

### 风险操作确认

所有**破坏性或有副作用的操作**都会先弹确认框，必须按 `y` 或 `enter` 才执行，`n` 或 `esc` 取消：

| 操作 | 触发 | 确认提示 |
|---|---|---|
| 删除转发 | `d` | 确认删除「名称」？此操作不可撤销。 |
| 停用转发 | `space`（运行中时） | 确认停用「名称」？将断开其现有连接。 |
| 重启转发 | `r`（运行中时） | 确认重启「名称」？连接会短暂中断。 |
| 停用全部 | `x` | 确认停用全部 N 条运行中的转发？将断开所有连接。 |
| 启用全部 | `a` | 确认启用全部转发？将新开 N 个监听端口。 |
| 批量启用 | 多选模式 `s` | 确认启用选中的 N 条转发？将新开 N 个监听端口。 |
| 批量停用 | 多选模式 `x` | 确认停用选中的 N 条转发？将断开它们的连接。 |
| 批量重启 | 多选模式 `R` | 确认重启选中的 N 条转发？连接会短暂中断。 |
| 批量删除 | 多选模式 `D` | 确认删除选中的 N 条转发？此操作不可撤销。 |
| 保存修改 | 编辑已有条目后 `ctrl+s` | 确认保存对「名称」的修改？运行中的连接会重建。 |
| 放弃编辑 | 表单里 `esc`（有未保存内容） | 确认放弃未保存的修改？已输入的内容会丢失。 |
| 退出程序 | `q` / `ctrl+c` | 确认退出 sshpf？（有运行中转发时提示会停止几条） |

```
╭ 确认 ────────────────────────────────────╮
│ ⚠ 确认停用「db-prod」？将断开其现有连接。│
│                                          │
│  y / enter 确认     n / esc 取消         │
╰──────────────────────────────────────────╯
```

**刻意不弹确认的操作**（可逆、无副作用，弹窗只会碍事）：

- 移动光标 `j/k`/`↑↓`/`g/G`、过滤 `/`、打开帮助 `?`、查看详情 `enter`
- **启动**某条未启用的转发（新增/启动是可逆的，停用才需要确认）
- **新建**转发并保存（新增是增量操作，删掉即可撤销；编辑才算覆盖）
- 表单里在两阶段导航态下移动高亮（不会改动任何值）

确认框的设计取舍：批量操作与单项操作共用同一套对话框，但文案会区分「一条」和「N 条」，
避免用户以为 `x` 只停当前行。

### 新增转发：两阶段编辑 + 自动推荐本地端口

表单采用**两阶段编辑**，避免方向键误改内容：

- **导航态**（默认）：`↑↓`/`tab` 只在字段间移动高亮，**不会改动任何值**
- **编辑态**：在高亮字段上按 `enter` 才进入，此时才接受键盘输入
- 编辑态按 `enter`/`esc` 结束编辑回到导航态；`tab` 结束并跳到下一字段
- `ctrl+s` 在任何时候保存

字段顺序按实际填写逻辑排列，**远端在前、本地在后**：

```
╭ 新建转发 ──────────────────────────────────────────────────────────╮
│ SSH 主机  testhost                                                ▾ │
│ 名称      demo                                                      │
│ 类型      < 本地转发 -L >                                           │
│ 远端地址  127.0.0.1                                                 │
│ 远端端口  8080                                                      │
│ 本地地址  127.0.0.1                                                 │
│ 本地端口  8080                                                      │
│ 自动重连  [x] 网络类失败时自动退避重试                              │
│ · 已自动设为 8080（与远端端口相同）（可编辑）                       │
│ · 127.0.0.1:8080 可用                                               │
│ ↑↓ 选择字段  ·  enter 编辑  ·  ctrl+s 保存  ·  esc 取消             │
╰─────────────────────────────────────────────────────────────────────╯
```

**本地端口自动推荐**：填完远端端口后自动给出建议值，用户仍可编辑覆盖。

| 场景 | 推荐行为 |
|---|---|
| 远端端口空闲 | 直接用相同数值（`8080 → 8080`） |
| 远端端口被占用 | 向上顺延到下一个空闲端口（`8080 → 8081`） |
| 远端端口是特权端口（<1024） | 用 `+2200` 的惯用别名（`22 → 2222`），因为绑定低位端口需要 root |
| 与其它转发条目冲突 | 跳过该端口（即使那条转发当前没在跑） |
| 用户手改过本地端口 | **不再自动覆盖**；只在提示里说明 |

推荐结果只是建议，字段始终可编辑；修改远端端口会重新推荐，一旦你手动改过本地端口就停止自动填充。

### 从 `~/.ssh/config` 导入转发

在 SSH 主机字段上按 `space` 或 `i`，展开该主机已声明的转发。面板是表单的子区域，不另开弹窗，
所以字段值和待导入的行可以对着看：

```
╭ 新建转发 ──────────────────────────────────────────────────────────╮
│ SSH 主机  cloud_pc                                                ▾ │
│ 名称      cloud_pc                                                  │
│ 类型      < 本地转发 -L >                                           │
│ 远端地址  127.0.0.1                                                 │
│ 远端端口                                                            │
│ 本地地址  127.0.0.1                                                 │
│ 本地端口                                                            │
│ 自动重连  [x] 网络类失败时自动退避重试                              │
│ ╭ 从 ~/.ssh/config 导入转发 · cloud_pc ───────────────────────────╮ │
│ │     类型  监听                 目标              原始指令        │ │
│ │▸[x] L     127.0.0.1:18789      127.0.0.1:18789   LocalForward 1…│ │
│ │ [x] L     localhost:8090       127.0.0.1:8080    LocalForward 8…│ │
│ │ [x] D     localhost:1080       (SOCKS5)          DynamicForward …│ │
│ │ [–] R     localhost:7006       :0                ⚠ 目标地址无法解析│ │
│ │全选  反选  全不选     清除全部筛选  撤销筛选                     │ │
│ │3/3 条勾选  space 勾选 · f/←→ 选列 · F 清该列 · ctrl+r 清全部 · z …│ │
│ ╰──────────────────────────────────────────────────────────────────╯ │
╰─────────────────────────────────────────────────────────────────────╯
```

**默认全部勾选**，取消掉不要的再保存。保存时**一条勾选行创建一条隧道**，名称 `<host>-<L|R|D>-<端口>`
（如 `cloud_pc-L-8090`），与已有名字重复时加 `-2` 后缀。

勾选状态只是**意图**：不写盘、不 bind、不建线程，隧道在你按 `ctrl+s` 确认之后才创建。
编辑已有条目时没有这个面板——把一次编辑悄悄变成批量新建是数据丢失级的问题。

几条容易踩到的规则（完整说明见 [docs/import-forwards.md](docs/import-forwards.md)）：

| 情况 | 行为 |
|---|---|
| `Host cloud_pc` 和 `Host *` 都声明了转发 | **全部累积**，两边的行都列出（OpenSSH 语义） |
| `Host prod-* !prod-db`，别名是 `prod-db` | 否定命中 → 整块排除 |
| 转发写在 `Match` 块里 | **不导入、也不列出**（它们带连接上下文前置条件） |
| 注释掉的 `# LocalForward …` | 不列出 |
| `Include` 的文件 | 会跟进；总文件数有上限，且挡环状 include |
| `Host *` 重复了具体块的转发 | 去重成一行，否则两条隧道抢同一端口 |
| Unix socket 形式 / `-R` 没有目标 | 列出但不勾选，变暗并写明原因 |
| 供 `-R` 使用 | 两端**对调**：指令的监听端 → 远端，指令的目标端 → 本地 |
| 本地端口已被本机或其它条目占用 | 自动向上顺延（`-R` 不占本地端口，不顺延） |

去重按**字面** key 比较，不做地址归一化：`LocalForward 8090 …` 和
`LocalForward 127.0.0.1:8090 …` 是两行，虽然语义上是同一个监听端点。第二条会被自动顺延到
下一个空闲端口并命名成 `…-8090-2`，所以结果仍然是安全的——创建时的监听地址冲突校验会兜底。

面板支持**按列筛选**：`f` 在四列间循环（或 `←`/`→`），直接输入即筛选当前列，`F` 清当前列，
`ctrl`+`r` 清全部，`z` 逐次撤销。多个筛选 AND 到一起，`全选`/`反选`/`全不选` **只作用于筛选后
可见的行**——勾上被隐藏的行会让"看起来选了 3 条"变成"实际创建 8 条"。

## 配置

`~/.config/sshpf/config.toml`（`$XDG_CONFIG_HOME` 生效；`SSHPF_CONFIG` 可覆盖），权限 `0600`。
运行时状态单独放 `~/.local/state/sshpf/state.toml`，不污染用户编辑的配置。

```toml
version = 1

[defaults]
bind = "127.0.0.1"        # 新转发的默认本地监听地址
restore_on_start = true   # 启动时恢复 enabled = true 的条目
auto_restart = true       # 网络类失败自动退避重连（2s→4s→…→30s）
retry_initial_secs = 2
retry_max_secs = 30

[[tunnels]]
id = 1
name = "cloud-pc-ssh"
kind = "local"            # local(-L) | remote(-R) | dynamic(-D, SOCKS5)
ssh_host = "cloud_pc"     # ~/.ssh/config 里的别名，或 user@host:port
local_addr = "127.0.0.1"
local_port = 19711
remote_host = "127.0.0.1"
remote_port = 22
enabled = true            # 上次是否在运行 → 启动时据此恢复
auto_restart = true
ssh_options = []          # 额外 ssh -o 选项，如 ["Compression=yes"]
note = ""
```

**不存任何密码或密钥口令。** 认证只走 `~/.ssh/config` 的 `IdentityFile` 或 ssh-agent（与旧版本的 `BatchMode=yes` 行为一致）。需要密码/2FA 的主机会立即失败并在表格里显示「认证失败」，提示你先配好免密登录——TUI 不会卡在密码提示上。密码与键盘交互登录暂不支持。

## 失败原因分类

表格「最后错误」列只放归类短句，完整 ssh 原始输出在 `enter` 详情页里（可滚动、不截断）：

| 短句 | 触发 |
|---|---|
| 本地端口被占用 | `bind …: Address already in use`；也包含启动前的端口预检 |
| 端口权限不足(<1024) | `bind …: Permission denied` |
| 认证失败(公钥/agent) | `Permission denied (publickey…)` |
| 域名解析失败 | `Could not resolve hostname` |
| 连接被拒绝 / 连接超时 | `Connection refused` / `Connection timed out` |
| 主机密钥校验失败 | `Host key verification failed`、`REMOTE HOST IDENTIFICATION HAS CHANGED` |
| 远端转发失败 | `remote port forwarding failed` |
| 目标不可达 | 隧道通但目标端口拒绝（属正常日志，不算隧道失败） |

无 ssh-agent 时的 `ssh_get_authentication_socket: Connection refused` 是**无害**提示，不会被误判为失败。

## 诊断日志

每次运行都往一个追加写的文件里记录结构化日志——启动与退出、隧道启停与重连、
异常与失败原因、**panic 与致命信号（SIGSEGV/SIGABRT/…）**。TUI 里那份日志只活在
进程内，进程一退（尤其是崩溃）就没了；这个文件是留给事后分析的。

```bash
tail -f ~/.local/state/sshpf/logs/sshpf.log     # 实时看
grep ' ERROR ' ~/.local/state/sshpf/logs/*      # 只看出事的
grep 'tunnel=3 ' ~/.local/state/sshpf/logs/sshpf.log   # 一条隧道的完整时间线
```

一行一条记录，列顺序固定：

```
2026-11-08T18:55:42.957+08:00 INFO  seq=12 event=tunnel.ready tunnel=3 name=cloud-pc forward=local listen=127.0.0.1:19711 target=127.0.0.1:22 host=cloud_pc elapsed_ms=812
```

崩溃也留得下来：

```
… ERROR seq=42 event=crash.panic thread=sshpf-tunnel-3 location=src/app.rs:112:9 msg="index out of bounds" backtrace=yes
… ERROR seq=43 event=crash.panic.trace msg="  | panic backtrace\n  |    0: sshpf::tunnel::run\n  | …"
ts=1794000000 ERROR seq=0 event=crash.signal signal=11 name=SIGSEGV pid=4242
```

- 目录 `0700`、文件 `0600`；单文件 2 MiB 轮转，保留 3 个归档（上限约 8 MiB）。
- `SSHPF_LOG_DIR=<目录>` 换位置，`SSHPF_LOG_LEVEL=off|error|warn|info|debug` 换级别（默认 `info`）。
  `off` 只关常规记录，**崩溃记录照写**。
- 不记私钥、passphrase、`known_hosts` 内容。
- **提 issue 请附整个 `logs/` 目录**（含 `.1`~`.3`），重连风暴、配置被改的证据都在更早的位置。

完整事件目录、字段说明与排查配方见 [docs/logging.md](docs/logging.md)。

## 故障排查

```bash
sshpf --list-hosts          # 确认主机能被读到
sshpf --check               # 确认配置没写坏
```

- **主机密钥校验失败**：本机 `known_hosts` 与服务端密钥不一致（服务器重装过）。在终端里手动 `ssh <主机>` 确认指纹后处理。
- **认证失败**：先手动 `ssh <主机>` 确认能免密登录，或 `ssh-add` 加载密钥。
- **端口被占用**：详情页会显示占用进程的 `pid/进程名`。
- **ProxyCommand 主机**：这类主机的配置无法烘焙成一次性配置，会自动回退使用你自己的 `~/.ssh/config`（日志里有 warn）。

## 开发

```bash
cargo test                    # 全部：单元测试 + tests/*.rs 集成测试（e2e 默认 #[ignore]，见下一行）
cargo test --lib              # 只跑单元测试（CI 的 OS 矩阵跑这个）
cargo test --test help_coverage   # 帮助覆盖的机器校验（代码 ↔ 帮助目录双向断言）
cargo test --test logging         # 诊断日志集成测试
cargo test --test e2e -- --ignored --test-threads=1   # 真实 sshd 端到端（默认被 #[ignore])
cargo clippy --all-targets    # 零警告
cargo fmt --check             # 格式检查
```

`cargo test` 会同时跑 lib 单元测试与 `tests/*.rs` 下的集成测试；端到端那组默认
`#[ignore]`，必须显式 `--ignored` 才会执行（它需要真实的 `sshd`、`ssh-keygen`）。
`cargo test` runs the lib unit tests *and* the integration tests in `tests/*.rs`; the
end-to-end group is `#[ignore]`d by default and needs an explicit `--ignored` (it requires a
real `sshd` and `ssh-keygen`).

测试分布：

单元测试（`cargo test --lib`）：

| 模块 | 测试数 | 覆盖重点 |
|---|---|---|
| `app` | 102 | 就绪信号、重试退避、确认门控、过滤与选中、导入批量校验、多选标记与批量派发 |
| `ssh` | 84 | config 解析（含 Match 污染防护）、**转发指令扫描与作用域**、known_hosts 匹配、SOCKS5、错误分类 |
| `keys` | 65 | 两阶段编辑语义、主机下拉、确认键位、导入面板按键归属、两阶段保存、鼠标派发、未知键忽略 |
| `ui` | 69 | 1×1 到 40×12 极端尺寸、超长文本、NO_COLOR、命中图、导入面板列宽降级、双语帮助浮层折行 |
| `model` | 55 | 三种转发 spec、失败码分类、输入缓冲边界、逐列筛选与撤销栈、多选语义 |
| `config` | 27 | 缺省值、原子写、0600/0700 权限、字段校验、容错 |
| `logging` | 16 | 记录格式/转义、级别过滤、轮转不切记录、panic 与致命信号真写盘、0600/0700 |
| `tunnel` | 5 | 启停、事件顺序、重复启动、停止幂等 |
| `help` | 5 | 帮助目录不变式：14 个分区齐全、每条中英齐全、`fit` 三级退化 |
| `sys` / `text` | 8 | 文件权限、退出信号、显示宽度与截断 |
| **合计** | **436** | `cargo test --lib` |

集成测试（`tests/*.rs`）：

| 文件 | 测试数 | 覆盖重点 |
|---|---|---|
| `tests/help_coverage.rs` | 8 | **机器校验帮助覆盖**：解析 `src/keys.rs` 每个 handler 的 `KeyCode`，与帮助目录双向比对（不得遗漏 / 不得撒谎）、`HitTarget` 每个变体都被说明、每个 token 都出现在 [docs/keymap.md](docs/keymap.md) |
| `tests/logging.rs` | 15 | 真实文件系统上的日志落盘、轮转与权限 |
| `tests/e2e.rs` | 10（9 个默认 `#[ignore]`） | 真实 `sshd` 上的 `-L` / `-R` / `-D` 端到端 |

模块分层（每层只依赖上层）：

```
model   纯数据类型，无 I/O、无 ratatui
ssh     ~/.ssh/config 解析（连接参数 + 转发指令）、known_hosts 校验、SOCKS5、russh 客户端
tunnel  隧道线程监管：启动 / 停止 / 事件
config  TOML 持久化（原子写、0600）
logging 诊断日志：结构化记录、轮转、panic/致命信号崩溃捕获
app     状态机：持有转发、消费事件、导入批次、多选、鼠标派发
keys    按键分发（从 main 抽出以便单元测试）
ui      ratatui 渲染（纯函数，无副作用）+ 每帧重建的鼠标命中图
main    CLI、终端生命周期、信号处理、鼠标捕获
```

调研依据（`docs/research/` 下的早期调研笔记，不随 crates.io 包发布，仅在仓库中可读）：
[pure-rust-ssh-plan.md](https://github.com/can2049/sshpf/blob/main/docs/research/pure-rust-ssh-plan.md)（纯 Rust 化的决策与实测）、
[pure-rust-ssh-spike.md](https://github.com/can2049/sshpf/blob/main/docs/research/pure-rust-ssh-spike.md)（三种转发的端到端验证）、
[tech-options.md](https://github.com/can2049/sshpf/blob/main/docs/research/tech-options.md)（早期技术选型）、
[prior-art.md](https://github.com/can2049/sshpf/blob/main/docs/research/prior-art.md)（竞品与交互设计）。

## 已知限制（v1）

- **端口占用反查进程**仅 Linux 可用（依赖 `/proc`）。其它平台该信息显示为空，不影响转发本身。
- 隧道既是 tokio task，杀进程即关闭 socket，会话随之结束——不需要单独的孤儿回收机制；
  macOS 走 Unix 分支。
- 只支持公钥 / ssh-agent 认证，TUI 内不接管密码输入。
- 不统计流量（需要按 channel 累计字节，留待后续）。
- 同一主机多条转发各自建连，未做连接复用（ControlMaster 的 `-O forward` 绑定失败仍返回 exit 0，无法可靠判定成败，故不采用）。
- **导入只读 `LocalForward` / `RemoteForward` / `DynamicForward`**。`Match` 块里的转发一律不导入
  （带连接上下文前置条件），Unix socket 形式与 `-R` 无目标只列出不可导入。
- `Include` 有**总文件数**预算（含主文件共 9 个），超出部分静默不读；`Include` 写在 `Host` 块内部时，
  被包含文件里的裸指令会被当成全局指令（OpenSSH 会继承外层作用域）。
- 导入去重按**字面** key，不做地址归一化：`8090` 与 `127.0.0.1:8090` 是两行。端口冲突在创建时兜底
  （自动顺延 + 监听地址校验），所以结果仍安全。
- 鼠标是 best-effort：终端拒绝捕获时只影响鼠标，键盘 UI 完整可用。字面字符 `f` 无法输入到导入表筛选里
  （`f` 优先用于切换筛选列）。
- **崩溃日志在 Windows 上只覆盖 panic**。没有 `sigaction`/Unix 信号语义，SEH 层（访问违例）
  不在此列；Windows 的 panic 记录、栈、日志文件与 Linux 一致。
- 崩溃记录里的致命信号行用 epoch 秒（`ts=…`）而非格式化时间戳：信号处理器只能用
  async-signal-safe 调用，首次 `localtime_r` 会分配内存。其余字段格式与普通记录一致。

## 许可证

[MIT](LICENSE)
