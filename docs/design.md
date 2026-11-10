# 设计决策索引 / Design decisions

> **这份文档回答**：每个关键设计**为什么**这样，以及"想深入了解该读哪篇"。
> **不负责**：任何展开叙述——每条只有 1–2 句结论 + 链接。**不要**在这里新增流程/规则说明。
> **相关代码路径**：无（决策索引）；实现位置见每条末尾。
>
> 本文文件名 `docs/design.md` **不能改**：`src/text.rs`、`src/ui.rs`、`src/app.rs`、`src/ssh/config.rs`、`src/ssh/known_hosts.rs`、`src/sys/mod.rs` 的模块注释都引用它。证据来源：`docs/research/`（历史调研，见 [research/README.md](research/README.md)）。

## D1 用 `russh` 而不是调系统 `ssh`
调 `ssh` 子进程让两平台行为不一致、失败只能正则解析 stderr；纯 Rust 栈失败是结构化 `russh::Error`，三类实测误判 bug 从根上消失（见 D3）。代价：二进制 1.63→4.34 MB，运行时依赖归零。
→ [runtime.md](runtime.md#就绪判定--readiness) · `src/ssh/client.rs`

## D2 `~/.ssh/config` 由 `ssh2-config` 解析，但必须自己剔除 `Match` 块
它的 `Field` 枚举里**没有 `Match`**，`Match` 行被当未知字段忽略、**其后的选项却继续写进上一个 `Host` 块**——用户查到错的 `user`/`port` 且无任何报错。静态错误比崩溃更危险，所以解析前用 `strip_match_blocks` 剔除。
→ `src/ssh/config.rs` · 导入侧同类规则见 [import.md](import.md#match-一律不导入--match-blocks-are-never-imported)

## D3 就绪判定：从"猜日志"变成"看结果"
旧实现解析 `ssh -v` 的 `Entering interactive session.`，有三个实测假信号。现在就绪是二值真实结果（`bind` 返回 `Ok` / `tcpip_forward()` await 到回复），`TunnelEvent::Ready` 是通往 `Up` 的**唯一**路径。
→ [runtime.md](runtime.md#就绪判定--readiness) · `src/tunnel.rs`、`src/app/runtime.rs`

## D4 每条隧道 = 一个后台线程 + 一个 `current_thread` runtime
隔离性好（一条 panic 不传染），实测 10 条并发建连仅 80 ms；共享 runtime 省内存但把"一条出问题全吐血"引进来。`Handle` 不是 `Clone`，单条隧道内多 task 必须 `Arc<Handle>`。
→ [runtime.md](runtime.md#线程模型--threads) · `src/tunnel.rs`

## D5 tokio task 取代子进程，顺带消掉了孤儿回收
停止 = 置标志 → runtime 丢弃 → socket 关闭（释放实测 172 µs）；`SIGKILL` 不留孤儿；30 次启停无 fd 泄漏。`sys` 层因此只剩文件权限与信号。
→ [runtime.md](runtime.md#停止--stop) · `src/sys/`

## D6 失败分两类，决定"要不要等"
配置类重试无用（标 `✗` 停下），网络类指数退避。`arm_retry` 启动时插占位记录 `attempt = 0`、`tick_retries` 只处理 `attempt >= 1`——否则占位符会让程序在启动瞬间无限重连（集成测试抓到过）。
→ [runtime.md](runtime.md#失败分类与重试--classification--retry) · [logging/events.md](logging/events.md#隧道生命周期--tunnel-lifecycle)

## D7 用户配置与运行状态分离
`config.toml` 是用户意图（可手改、原子写 `0600`/`0700`）；`state.toml` 是纯诊断。放 `~/.ssh/` 是错的（那是 OpenSSH 的领地），放项目目录也是错的（TUI 是全局工具）。
→ `src/config.rs` · 字段表见 [../README.md](../README.md#配置--configuration)

## D8 主机选择用内嵌下拉框，不是独立弹窗
新增转发第一步是选主机，入口必须**看得见**（v1 的 `ctrl+h` 模态框很难被发现）。`SSH 主机` 字段本身就是下拉，**在表单内部**紧贴渲染；**别名优先匹配**——否则输入 `de` 会因某台机器用户名是 `deploy` 而列出 `bastion-prod`。
→ `src/app/picker.rs`、`src/ui/dropdown.rs` · 键位见 [keymap.md](keymap.md#主机下拉--host-dropdown)

## D9 表单两阶段编辑
方向键按一下就改值很挫败，所以分 `Navigating`（只移动高亮）与 `Editing`（按 `enter` 进入）。枚举与开关类字段不进编辑态。
→ [keymap.md](keymap.md#表单导航态--form-navigating) · `src/app/form.rs`

## D10 本地端口推荐：建议，不是决定
用户始终可改；手改过就停止自动覆盖；编辑既有条目不推导。特权端口不能"从远端端口向上扫"（`port_available` 会把所有 <1024 报成不可用，扫描预算会在特权区间全部耗尽）。
→ [import.md](import.md#本地端口顺延--local-port-shifting) · `src/model/`（`plan_local_port`）

## D11 风险操作：三条判断标准
拦的三条：会丢数据、会断连接、事后不易察觉；可逆无副作用的**刻意不拦**。给高频操作加确认会让用户习惯无脑按 `y`，真正的危险确认就失效了。`ConfirmAction` 穷举受保护操作，编译器强制新操作想清分类；表单保存拆两步，待保存配置暂存 `App.pending_form`。
→ [keymap.md](keymap.md#安全规则--safety-rules) · `src/app/confirm.rs`

## D12 按键分发搬出 `main.rs`
二进制 crate 的私有函数无法被 `tests/` 调用，只能靠 pty 驱动真实 TUI——慢、脆、抓不到具体分支。现在 handler 在 `src/keys/`，无副作用，可以当纯函数测。
→ [architecture.md](architecture.md#keys-键分发--srckeys) · [testing.md](testing.md#写新测试--write-a-new-test)

## D13 帮助是数据，不是散文
"所有按键都要有说明"是全称命题，无法靠人工守住，而遗漏是**静默的**——用户不知道有那个键，因此永远不会报 bug。所以 `src/help.rs` 只有 `SECTIONS` 数据，`tests/help_coverage.rs` 双向比对。书面后果（`" / " 分隔、`+` 粘住、方向键用符号）是为了让测试**无歧义地切分**，不是为了好看。
→ [keymap.md](keymap.md) · `src/help.rs`

## D14 覆盖测试两个方向都断言
正向防**遗漏**；反向防**撒谎**。反向最容易踩：删掉 dispatch 忘了删帮助行很常见，而撒谎会摧毁整份帮助的可信度。它曾抓到 `form_editing` 多出 `ctrl+s`/`f10`——编辑态真的没 dispatch。
→ [testing.md](testing.md) · `tests/help_coverage.rs`

## D15 解析用花括号配平，把注释和字符串涂空
花括号配平（不用缩进/行号/正则，都不可靠）。扫描前把注释与字符串字面量**涂成空格**（保留字符字面量，`Char('j')` 的 `'j'` 就是要抽的载荷），涂空格而非删除是为了让偏移量与原文对齐。修饰键作用域是**单个 match arm**。
→ `tests/help_coverage.rs`

## D16 白名单要写成常量并注明理由
刻意例外必须显式、有名字、有注释（`DOC_ONLY` / `CODE_ONLY`）。用"忽略失败"或注释掉断言掩盖差异等于把测试变成仪式；写成常量加理由才把例外变成**设计决策**。
→ [agents.md](agents.md#加按键--add-a-key) · `tests/help_coverage.rs`

## D17 符号归一化后再比对
`KeyCode::Down` vs 界面 `↓`；`Char('j')` + `SHIFT` vs 大写 `'J'`。比对前统一 `↓↑←→`→`down/up/left/right`、`shift+<字母>`→大写。
→ `tests/help_coverage.rs`

## D18 逐列筛选用子串，主表过滤用模糊
主表 `/` 用 `fuzzy_match`；列筛选**刻意**用子串——模糊匹配下 `1080` 会命中 `127.0.0.1:10800`、`1:0:8:0`，而筛选端口是为了**精确定位一行**。
→ [import.md](import.md#为什么用子串而不是模糊匹配--substring-not-fuzzy) · `src/model/`（`col_match`）

## D19 撤销用快照栈，不用增量日志
每次改筛选**之前**压完整 `[String; 4]` 快照（上限 64）。快照是这个状态最小且不会算错的实现；增量式要处理"改哪一列、改成什么"，撤销跨列操作得记多条。
→ `src/model/`（`ImportTable` 撤销栈） · 键位见 [keymap.md](keymap.md#导入列筛选--import-column-filter)

## D20 `f` 优先于筛选输入
`f` 用于循环切换"正在筛选哪一列"，助记位置又正好在被筛选的输入里，所以 `f` **优先**被处理。代价是字面 `f` 无法输入——四列内容没有哪列需要匹配 `f`；而若让 `f` 被输入吃掉，底部「f/←→ 选列」的提示就成了谎话。
→ [import.md](import.md#为什么用子串而不是模糊匹配--substring-not-fuzzy) · `src/keys/import.rs`

## D21 全选只看筛选后的视图
只遍历 `visible()`。这不是实现细节而是安全约束：勾上隐藏行，用户看到 3 行只勾了 1 行，保存却起 3 个监听端口。同一条规则保护主表多选。
→ [import.md](import.md#全选--反选--全不选只看筛选后的视图--bulk-ticks-see-only-the-filtered-view) · `src/model/`

## D22 多选标记按 `TunnelId`，不按行号
过滤和删除都会让行移动；存行号会让"第 3 行"指向另一条隧道，**批量删除会删错东西**。`BTreeSet<TunnelId>` 天然去重、保序。四种批量操作**都过确认闸**，提示用**有效条数**；无需变化时不弹窗——确认"什么都不做"是噪音。
→ [keymap.md](keymap.md#多选模式--selection-mode) · `src/app/select.rs`

## D23 鼠标走语义命中图，不重算坐标
不存坐标：渲染器已算过布局，在事件处理里再推导就是**第二个真相来源**——改个列宽鼠标就点错而键盘完全正常。鼠标调与键盘同一批 `App` 方法，**免费继承**安全规则。

**前提容易被忘掉**：命中图是**栈**不是 z-index。弹窗若不自记矩形，表格矩形仍在栈顶，点击会穿过弹窗——视觉上盖住了，行为上没有。
→ [tui.md](tui.md#命中图--hit-map) · `src/ui/hit.rs`、`src/app/mouse.rs`

## D24 日志格式用 `key=value`，不用 JSON
读者是"拿着终端排障的人"和 `grep`/`awk`，不是日志聚合器。JSON 每行重复键名，肉眼扫起来噪声占一半；固定列顺序让 `cut`/`awk` 有意义，隧道身份 6 字段固定在前让记录脱离上下文也能读懂。
→ [logging.md](logging.md#格式) · `src/logging/format.rs`

## D25 级别按"读的人要做什么"划分
关键判断：**掉线（`was_up=true`）是 WARN，从未连上的失败是 ERROR**——前者系统会自愈，后者要用户动手。`off` 只关常规记录，崩溃记录无条件写。
→ [logging.md](logging.md#级别策略) · [logging/events.md](logging/events.md)

## D26 崩溃捕获两条通路，日志系统自己是最后防线
panic 走 hook（链式调用上一个，所以 ratatui 恢复终端照常生效）；致命信号走 `sigaction`，handler 内只能 async-signal-safe 调用。第三层是"崩溃机制本身会挂"——panic 恰在持锁写入中时 hook 再 `lock()` 就是自己等自己（**不是 poison，是同线程重入挂死**），所以用 thread-local 哨兵识别重入。

最容易被漏的一步是**解除内核对该信号的屏蔽再 `raise`**：不解除则 `raise` 只排队，`_exit(139)` 抢先执行，进程就从"被 SIGSEGV 杀死"变成"以 139 退出"，core dump 也不产生。
→ [security.md](security.md#崩溃捕获--crash-capture) · `src/logging/`

## D27 防御式边界：三层降级
> 边界之外是任意的，边界之内不许 panic；出问题就降级，降级必须留痕。

日志这一层只保"记录得下来"，不保"程序活着"——把崩溃静默掉才是更坏的选择。
→ [security.md](security.md#三层防御--three-layers)

## D28 Windows：纯 Rust 让平台差异几乎消失
子进程不存在，进程树回收问题自然消失，**SSH 相关代码零平台分支**。三个实测结论：`ring` 加密后端必须（`aws-lc-rs` 在 Windows x86-64 要 NASM + CMake）；不再需要假 ssh 夹具；端口占用反查进程仅 Linux 可用。
→ [architecture.md](architecture.md) · `Cargo.toml`、`src/sys/`

## D29 分支保护
4 个 CI 检查全绿（`strict: true`）、禁止强推与删除、`enforce_admins: true`、必需 review 批准为 0（单人仓库无法自我批准，实测会卡在 `BLOCKED`）。
→ [testing.md](testing.md#ci-分工--ci-jobs) · `.github/workflows/ci.yml`

## D30 v0.3.0：拆分、分层、收口 `unsafe`
单体大文件（`model.rs` 2532 / `ui.rs` 3095 / `logging.rs` 1522 / `keys.rs` 1828 / `app.rs` 1602 行）拆成一层一职责；重复实现收敛到 `src/text.rs` 与 `src/eventlog.rs`；`eventlog` 上移修掉"底层依赖上层"；`unsafe` 收口；文档按 agent 信息架构重建。
→ [architecture.md](architecture.md#模块地图--module-map) · [agents.md](agents.md)

## D31 v0.3.1：新建转发弹窗加宽
原本 74 列，「原始指令」列只有 23，把 `RemoteForward 9000 127.0.0.1:9001`（33 列）截成 `12…`——恰好截掉用户辨认一行所依据的那列。改为 110 列（`layout::FORM_POPUP_W`）；两个测试看护（各列放得下 / 120 列终端里文本**不含** `…`）。
→ [tui.md](tui.md#浮层--overlays) · `src/ui/layout.rs`、`src/ui/form.rs`

## D32 去重比「意图」，不比端口
`-L` 身份**不含本地监听端口**——那是 `plan_local_port` 会顺延、且顺延值会被持久化的派生量，原始请求事后不可恢复。任何 `saved.local_port == requested` 的判定都会漏掉实测到的那类重复。代价是 `8080 host:80` 与 `9090 host:80` 判同一条；取舍：同目标两个本地监听是冗余的，而漏判会让用户**根本清不掉**重复——前者可绕过，后者不可。
→ [dedup.md](dedup.md) · `src/model/tunnel.rs`、`src/app/duplicate.rs`

## D33 「默认全选」不等于「赢过手填」
旧实现把「面板可见」等同于「面板就是用户的意思」，于是手填一条按 `ctrl+s` 会被批量路径静默吞掉。修法是给两种意图分优先级：**未动过的面板是默认提议，完整的手填表单是明确指令**。**「动过」还有一个更强的约束：动过的面板必须有内容**，否则 `ctrl+s` 会答「没有勾选任何转发」而手填内容原地卡住。一句话：**没有可执行内容的那份界面不能赢。**
→ [import.md](import.md#提交归属默认提议-vs-明确指令) · `src/app/form_submit.rs`、`src/model/import/table.rs`

## D34 去重必须发生在「提议」这一层，而不只是「提交」这一层
守卫拦得住**新建**的重复，拦不住两种已经发生的：① **面板的默认勾选**——已建过的转发仍是「全部勾上」默认，每份副本还被顺延到新端口，下一轮连守卫都认不出来；② **`config.toml` 里已写进去的重复**——守卫永远看不到。所以修法分两处，都在**提交之前**：面板标「已存在」并禁勾（提议层）+ 载入时按身份折叠（持久层）。只修一层都不够。
→ [dedup.md](dedup.md#已写进配置的重复载入时折叠--fold-duplicates-on-load) · [import.md](import.md#已存在的行--rows-that-already-exist) · `src/model/dedup.rs`、`src/model/import/row.rs`、`src/app/import_pane.rs`

## D35 子焦点不能吞掉保存键：`ctrl+s` 不是字母
`ctrl+s` 是 `Char('s')` 带 CONTROL 修饰，字符本身**不是**控制字符，所以任何 `Char(c) if !c.is_control()` 的兜底分支都会把它当普通文本（主机下拉查询框、导入面板列筛选）。结果是"按 `ctrl+s`，端口没有建立"且**日志完全空白**——键在分发层就被吃掉了，这比创建失败更难查。修法：每个子焦点入口先问 `keys::is_save_key`，命中就交还表单，并记 `form.save_key_swallowed`。原则：**保存键的语义优先于任何子焦点的文本输入**；被吃的键必须留下痕迹，否则「没反应」无法与「没按」区分。
→ [keymap.md](keymap.md) · `src/keys.rs::is_save_key`、`src/keys/dropdown.rs`、`src/keys/import.rs`、`src/eventlog/user.rs`

## D36 连通性靠主动探活，不靠 accept 循环
`-L`/`-D` 的就绪判据是 `bind` 成功，而**已建立的会话死亡对 listener 完全不可见**：`accept()` 照样返回，每条连接随后都失败，状态列永远 `●`。修法是每 `health_check_secs` 发 `keepalive@openssh.com` 并等回复。探活放在**隧道线程**里而不是另起 task——另起 task 会让 serve 循环继续陪伴死会话，那正是要修的 bug。也**不新增状态机**：`StaleConnection` 只是普通的网络类失败分类。
→ [runtime.md](runtime.md#健康检查--health-checks) · `src/ssh/client/health.rs`、`src/ssh/client/driver.rs`、`src/model/failure.rs`

## D37 宽度用 `unicode-width` 量字符串，不手维护区间表、不逐字符相加
原先 `text.rs` 手维护一张 East-Asian 宽度区间表，并假设 `display_width` = 各字符宽度之和。两个假设都是错的：① 表只返回 1 或 2，**永远不返回 0**，于是组合符（combining mark）、零宽连接符（ZWJ）、变体选择符都被当成占一列；② **字符串宽度不是逐字符相加**——ZWJ 序列 `👨‍👩‍👧` 是一个 2 列字形，逐字符相加得 8。表格里带 emoji 的行因此整体错位。
现在 `char_width` / `display_width` 直接走 `unicode-width`（UAX #11）。**零新增依赖**：它早已在依赖树里（经 `ratatui-core`），提升为直接依赖只是让本模块与终端、ratatui 用同一把尺子。代价是三条连带规则，都写进了 `src/text.rs` 的头注释：字符宽度可能是 **0**；`display_width` 必须**量整串**；因此 `truncate_to_width` 与 `wrap` 用**前缀测量**而不是累加，否则会超出预算或截断字形。
→ `src/text.rs`、`Cargo.toml`（`unicode-width`）
