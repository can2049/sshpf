# 对端监听端口 / The peer's listening ports

> **这份文档回答**：远端端口能不能直接从对端挑、探测怎么执行、哪些端口不列出、单条与批量两种选择方式、`-R` 为什么只读、扫描线程与代次。
> **不负责**：`~/.ssh/config` 的导入（见 [import-forwards.md](import-forwards.md)）、批量保存的字段映射与端口顺延（见 [import-save.md](import-save.md)）、键位全表（见 [keymap.md](keymap.md#对端端口列表--peer-port-list)）。
> **相关代码路径**：`src/ssh/probe.rs`（远端命令与解析）、`src/app/port_scan.rs`（打开/扫描线程/代次）、`src/model/portscan.rs`（`PortPick`/`declared_ports`）、`src/app/port_batch.rs`（批量提交）、`src/ui/ports.rs`。

`~/.ssh/config` 告诉我们要转发什么；但**新建一条转发时，远端端口经常只能靠记忆填**——
用户知道"对端跑着个 5432"，却不知道它现在到底在不在、是不是换成了 5433。表单的
`远端端口` 字段上按 `p` 会去问对端：

```text
command -v ss      → ss -ltnH
else command -v netstat → netstat -ltn
else               → __SSHPF_NO_TOOL__
```

命令是**写死的字符串**（`src/ssh/probe.rs::list_command`），不插入主机名、别名或任何
用户输入，所以恶意 `Host` 别名没有可注入的位置。三条规则值得记住：

| 规则 | 为什么 |
|---|---|
| 只认 `LISTEN` 行 | 每条列出的端口都会被当成"可以转发过去的地方"；一个客户端连接的临时端口看起来同样合理，但转发过去什么都不通 |
| 哨兵区分"没有工具"与"没有监听" | 两者的空输出完全一样，但给用户的结论相反：一个是"装个 ss"，一个是"对端确实没开东西" |
| `ss` 的 `-H` 抑制表头 | 表头 `Local Address:Port` 含冒号，宽松的 token 扫描会把 `Port` 读成地址 |

## 复用隧道那条 SSH 路径

探测走 `ssh::client::connect`（同一个 endpoint 解析、同一条 `ProxyJump` 链、同样的
`IdentityFile`/ssh-agent 认证与主机密钥策略），**不另开一条连接路径**。"列端口能用但隧道连不上"
正是第二条路径造成的经典缺陷。

## 哪些端口不会出现在列表里

`~/.ssh/config` 已经为这个主机声明过的端口**不重复列出**（`model::declared_ports`）——
它们已经由导入面板提供，两个入口指向同一条隧道只会让用户建出两条。隐藏的条数会显示
在面板上（"已隐藏 N 个"），沉默会让用户以为扫描漏了。

## 选择：单条 vs 多条

| 方式 | 手势 | 结果 |
|---|---|---|
| 选一条 | `enter`（或光标行上 `enter`） | 端口填进 `远端端口`，列表收起，本地端口按 `plan_local_port` 重新建议 |
| 选多条 | `space` / `a` / `U` / `I` 勾选后 `ctrl+s` | **每个勾选端口一条 `-L` 转发**，整批全有或全无 |

勾选**默认全空**：发现一个服务不等于要转发它，一次按键不该开出多个监听。

批量路径也过[转发身份去重](dedup.md)：已保存的转发对应的端口会被跳过并在状态栏说明，不会因为本地端口被重新规划就变成"新"转发。

## 为什么 `-R` 是只读列表

`-R` 的远端端口是**服务端要监听的**端口，而扫描列出的是那里**已经被占用**的端口——选一个
必然在 `tcpip-remote-forward` 时撞车。所以 `-R` 下列表只作参考（显示 `[–]`），不能勾选；
`multi=false` 时 `PortPick::can_tick()` 直接返回 false，一次误触也产生不了隧道。

## 扫描是后台线程，且带代次

探测是真的 SSH 连接，不能跑在 UI 线程（200ms 帧循环会卡死）。它和隧道同形：一条线程 +
`current_thread` runtime + `mpsc`，每帧由 `drain_events` 收一次。用户可能在探测途中换主机
或重扫，所以每次扫描带一个 `seq`，**过期的答案直接丢弃**（`PortPick::install` 返回 false），
旧主机的慢响应永远不会覆盖用户正在看的列表。

## 已知边界 / Known limits

| 边界 | 行为 |
|---|---|
| 对端无 `ss` 也无 `netstat` | 明确报告"无法列出端口"，而不是显示成"没有监听" |
| 输出格式不认识 | 明确报告"格式无法解析（只支持 ss / netstat -l）" |
| 重扫 | 保留已勾选的端口（刷新不是重新决策），结果替换后仍然勾着 |
| `-R` | 列表只读，`can_tick()` 恒为 false |
| 扫描失败 | 原因写进面板与状态行，记 `event=portscan.failed` |
