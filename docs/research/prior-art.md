# 竞品与交互设计调研 / Prior art

> **这份文档回答**：同类工具怎么做、v1 界面的列宽与状态符号从哪来、哪些交互被反复验证过。
> **不负责**：当前键位与渲染——见 [../keymap.md](../keymap.md)、[../tui.md](../tui.md)；当前决策——见 [../design.md](../design.md)。
> **相关代码路径**：`src/ui/layout.rs`（列宽）、`src/keys/`、`src/help/`。

> 历史调研（2026-11），针对 v1（系统 `ssh` 子进程）。**不是当前事实来源**；
> 当前界面以 `src/ui/` 为准，本文只解释"当时为什么这样设计"。

## 1. 同类工具盘点

| 工具 | 形态 | 可借鉴点 |
|---|---|---|
| [twbot/mole](https://github.com/twbot/mole) | CLI（Rust，封装 autossh） | 从 `~/.ssh/config` **直接发现**隧道；模糊选择器；启动前端口冲突检测 |
| [davrodpin/mole](https://github.com/davrodpin/mole) | CLI（Go） | **自动挑空闲本地端口**；多隧道复用一条连接；`--detach` + `logs --follow` |
| [SSH Tunnel Manager](https://justcodepl.github.io/ssh-tunnel-manager/) | GUI（Go+WebView） | 数据写回 `~/.ssh/config`（元数据用注释），不锁定私有格式 |
| [0fuz/ssh-tunnel-manager](https://github.com/0fuz/ssh-tunnel-manager) | GUI（macOS） | **枚举失败原因**并保持到重连；端口冲突边输入边校验 |
| [autossh](https://manpages.debian.org/bookworm/autossh/autossh.1.en.html) | CLI（C） | 启动闸门（首 30s 失败即退出，避免无限重试掩盖配置错误） |
| [gost](https://github.com/go-gost/gost) | CLI/配置文件 | 服务端前置条件（如 `GatewayPorts`）必须在错误里点名 |
| [Portato](https://github.com/portuber/portato) | GUI | `space` 启停、`l` 每隧道日志、`/` 模糊过滤、`C` 复制并**自动避开已占用端口** |
| [VS Code Remote](https://code.visualstudio.com/docs/remote/ssh) | 编辑器内 | 端口被占时**自动映射并明确告知**（"3000 被占用，已映射为 4123"） |

**共性结论**

1. 成熟工具都做到「**一行一条目 + 一个明确状态** + 可展开的原始日志」三层粒度；没有工具把完整
   stderr 塞进列表行。
2. 被反复验证的交互：`space` 启停、`r` 重启、`n/e/d` 增改删、`/` 模糊过滤、`l` 该条目日志、
   `?` 帮助、`q` 退出**先确认**。
3. **失败原因要分类 + 短句**（`port in use` / `auth failed` / `host key mismatch` / `dns` /
   `timeout`），原始报错放详情；端口冲突是最高频失败，必须在**录入时**与**启动时**各拦一次。
   —— 这条是 `FailureKind` 分类设计的出处。

## 2. 表格列（按 120 列定稿）

| 列 | 宽度 | 说明 |
|---|---|---|
| 光标/标记 | 1 | `▶` 当前行；`*` 有未保存编辑 |
| 名称 | 12–14 | 唯一、可模糊搜索；缺省 `host:localport` |
| 类型 | 1 | `L` / `R` / `D`，比 `local`/`remote` 省 5 列 |
| 本地地址 | 16 | `127.0.0.1:8080`；越界截断中间 |
| 远端地址 | 16 | `D` 类型显示 `(SOCKS5)` |
| SSH 主机 | 11–14 | 显示 config 别名而非解析后 IP，完整值进详情 |
| 状态 | 11–12 | `●` / `◐` / `◌` / `✗` / `○` |
| 最后错误 | 26–34 | 只放**归类短句**，绝不换行 |
| 时长 | 6（可选） | 仅 connected 显示，`02:14 / 1h07m / 3d02h` |

**空间不足时的取舍顺序**：① 丢"时长" → ② 本地/远端合并为 `8080→3000` → ③ SSH 主机缩为 4 字符 →
④ 错误列缩到 18 列只留分类词。PID/连接数**不进主表**（进程内多路复用时无独立 PID，易误导），放详情。

**长错误折叠**：短句 ≤ 34 列，超出用**尾部** `…`、保留开头（开头通常是 `bind:`/`auth`/`dial`
这类分类词）。完整文本只在两处：底部一行错误条 + 详情面板滚动区。

## 3. 新增转发的录入流程

**结论：单表单 + 顶部"主机选择"，不用多步向导。** 字段总量 5–7 个，分步会强制 3 次回车且难以回看；
GUI 需要向导是因为字段藏在折叠面板，TUI 一屏放得下。

```text
步骤 0（仅当 ssh config 有 Host）：/ 模糊搜索 Host 别名 → 自动填充 host/user/port/identity
步骤 1：单页表单  Name  Type(L/R/D)  Local  Remote  SSH Host  [Identity]  [ ] auto-start
        校验实时，错误显示在字段下方一行；Enter 提交 / Esc 取消
步骤 2：提交即落盘并立即尝试连接；失败不撤销，行保留为 ✗ failed
```

- **Host 选择**：子序列模糊匹配（`dbs` 命中 `db-staging`），按最近使用排序，最多 8 行；也允许直接
  输入 `user@host:port`。
- **默认值**：`Type` 默认 `L`；`Remote` 默认 `127.0.0.1:<port>`；`Local` 取远端端口，被占则
  **+1 递增**探测并在字段旁提示"已自动改用 5434"；`Name` 默认 `<host>-<local_port>`，冲突加 `-2`。
- **必填校验**（提交前 + 提交时都跑）：端口 1–65535、host 非空无空格；本配置内端口重复**阻止提交**；
  系统内已监听**允许但警示**（提交后由真实 bind 决定）；`R` 类型远端 bind ≠ `127.0.0.1` 时提示需
  服务端 `GatewayPorts yes`。

## 4. "立即生效"的交互语义

- **状态列是主表达**：`◐ connecting` / `● connected` / `✗ failed`（附短因）/ `◌ retrying`（显示下次秒数）。
- **底部单行消息条是次表达**：只报"刚刚发生的事"，3–5s 淡出。**不要**用 toast（TUI 里会遮住表格）。
- **失败条目必须保留在表中**（所有调研工具一致）：删除会让用户重填；保留则 `e` 改端口、`r` 重试。
- **重试策略**：配置类错误（auth / host key / port in use / dns）**停止重试**标 `✗`；网络类
  （timeout / reset）进入 `◌ retrying`，退避 2s→4s→8s…上限 30s。
- **未知 host key**：不静默接受；弹确认框显示指纹，`y` 写入 `known_hosts` 并重连（TOFU）。

## 5. 状态持久化

- **位置**：`~/.config/sshpf/config.toml`（XDG），`--config` 可覆盖。**不放** `~/.ssh/`（OpenSSH 的
  领地，误写会破坏用户配置），**不放**项目目录（TUI 是全局工具）。运行时状态另置
  `~/.local/state/sshpf/state.toml`——**配置与状态分离**，避免每次启停都重写用户配置。
- **格式 TOML**：注释友好、`serde` + `toml` 成熟。
- **"上次运行状态"**：只记 `enabled` 布尔（= 用户意图），**不记**"当时恰好 connected"；首次安装
  默认全部 `enabled = false`，避免开机即弹一串失败。

## 6. 安全

- **默认绑定 `127.0.0.1`**；用户填 `0.0.0.0` 时给黄色警示。`-R` 不写 bind 时 sshd 默认 loopback。
- **配置文件 `0600`、目录 `0700`**；创建时先设 umask 再创建，避免"先 chmod 后写"窗口。
- **绝不存密码/私钥口令**；认证交给 ssh-agent + `IdentityFile`。
- host key 校验默认开启，提供显式 TOFU 弹窗，**不提供**"全局跳过校验"开关。
- 不执行 shell 拼接；日志屏蔽口令/key，错误只留分类 + 目标地址。
- 提权端口（<1024）不自动 sudo，明确报错并给 `CAP_NET_BIND_SERVICE` 指引。

## 7. v1 主界面提案（120×35）

`src/ui.rs` 的主屏布局与本节一致（图标、行序、页脚三段）。要点：

```text
row 0  标题栏     sshpf 0.1.0 · ssh-config N hosts · cfg PATH · HH:MM:SS
row 1  计数条     ● 4 connected  ◐ 2 connecting  ◌ 1 retrying  ✗ 3 failed  ○ 5 off
row 2  表头       NAME  T  LOCAL  REMOTE  SSH HOST  STATUS  LAST ERROR  UP
row 3+ 表格行     每行一条转发；选中行高亮 + ▶
底部   错误条     当前选中行的完整错误 + 可执行动作提示（[e] 改端口 / [C] 复制到空闲端口）
底部   日志区     当前选中行最近日志，L 切级别、f 跟随
末行   页脚键提示
```

**状态色**：`●` 绿、`◐`/`◌` 黄、`✗` 红、`○` 暗灰（`NO_COLOR` 时退化为符号 + 文字，不依赖颜色）。
**布局规则**：表头与页脚固定；日志区高度 = 终端高 − 表格行 − 7，最小 4 行、最大 8 行；
窄于 100 列隐藏日志区与 SSH HOST 列；宽于 110 列可在右侧展开详情面板。

## 8. v1 可执行清单（历史）

1. `config.toml`（TOML，`0600`）+ `state.toml`；条目字段 `name/type/local/remote/ssh/enabled`。
2. 表格 8 列：名称、类型、本地、远端、SSH 主机、状态、错误短句、时长。
3. `n` 单页表单，`/` 选别名，本地端口自动避让，提交后异步连接、行保留、底部单行报结果。
4. 错误分两类：配置类不重试标 `✗`，网络类 `◌` 退避 2s→30s。
5. 默认 `127.0.0.1`；不存密码；启动预检端口占用并给"改端口/复制到空闲端口"动作。

> 这些在 v1 已实现，v0.3.0 之后部分被重做（`russh` 替换、启动闸门移除等）。当前行为见
> [../index.md](../index.md)。
