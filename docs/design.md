# 设计决策索引 / Design decisions

> **这份文档回答**：每个关键设计**为什么**这样，以及"想深入了解该读哪篇"。
> **不负责**：任何展开叙述——每条只有一段，细节在链接里。**不要**在这里新增流程/规则说明。
> **相关代码路径**：无（决策索引）；实现位置见每条末尾的链接与被引文件。
>
> 本文文件名 `docs/design.md` **不能改**：`src/text.rs`、`src/ui.rs`、`src/app.rs`、`src/ssh/config.rs`、`src/ssh/known_hosts.rs`、`src/sys/mod.rs` 的模块注释都引用它。

证据来源：`docs/research/`（历史调研，见 [research/README.md](research/README.md)）。

## D1 用 `russh` 而不是调系统 `ssh`

v1 用 `std::process::Command` 调系统 `ssh`，理由是 ProxyJump/IdentityFile/agent/known_hosts 语义太复杂。代价是 **Linux 与 Windows 行为不一致**，且失败原因只能正则解析 stderr。换成纯 Rust 协议栈后两个平台走同一套代码，失败来自结构化 `russh::Error`，**三类实测过的误判 bug 从根上消失**（见 D3）。二进制从 1.63 MB 涨到 4.34 MB，但运行时依赖归零。

→ [runtime.md](runtime.md#就绪判定--readiness) · `src/ssh/client.rs`

## D2 `~/.ssh/config` 由 `ssh2-config` 解析，但必须自己剔除 `Match` 块

`ssh2-config` 0.9.0 纯 Rust、零 C 依赖，覆盖 `Include`/通配/否定/`Host *` 继承/`ProxyJump`。但它 `Field` 枚举里**没有 `Match`**，`Match` 行被当未知字段忽略、**其后的选项却继续写进上一个 `Host` 块**——用户查到错误的 `user`/`port` 且没有任何报错。静态错误比崩溃更危险，所以解析前用 `strip_match_blocks` 按行剔除 `Match` 块，并有单测固定。

→ `src/ssh/config.rs` · 导入侧的同类规则见 [import-forwards.md](import-forwards.md#match-一律不导入--match-blocks-are-never-imported)

## D3 就绪判定：从"猜日志"变成"看结果"

旧实现解析 `ssh -v` 的 `Entering interactive session.`，有三个实测假信号：`-L` 双栈半失败、ProxyJump 子进程的行混入、`-R` 的成功标记晚于 `Entering`。现在就绪是二值的真实结果：`-L`/`-D` 是 `TcpListener::bind` 返回 `Ok`，`-R` 是 `tcpip_forward()` await 到服务端回复。`TunnelEvent::Ready` 是通往 `Up` 的**唯一**路径，任何日志文本都不得改变状态。

→ [runtime.md](runtime.md#就绪判定--readiness) · `src/tunnel.rs`、`src/app/runtime.rs`

## D4 每条隧道 = 一个后台线程 + 一个 `current_thread` runtime

UI 线程保持同步（`poll(200ms)`），绝不阻塞在网络握手上。一线程一 runtime 的理由：实测 10 条并发建连仅 80 ms，本项目规模下完全够用，且隔离性更好——一条隧道 panic 不影响其它。共享 runtime 会省一点内存，但把"一条出问题全吐血"的概率引进来。`russh::client::Handle` 不是 `Clone`，单条隧道内多 task 共享必须 `Arc<Handle>`。

→ [runtime.md](runtime.md#线程模型--threads) · `src/tunnel.rs`

## D5 tokio task 取代子进程，顺带消掉了孤儿回收

旧实现要处理进程组、`PR_SET_PDEATHSIG`、Windows Job Object，因为子进程会在父进程被杀后变孤儿并占住端口。现在停止 = 置标志 → `select!` 返回 → runtime 丢弃 → socket 关闭（端口释放实测 172 µs）；`SIGKILL` 也不留孤儿；30 次启停无 fd 泄漏。`sys` 层因此从两个 200 行平台文件缩到只剩文件权限与信号。

→ [runtime.md](runtime.md#停止--stop) · `src/sys/`

## D6 失败分两类，决定"要不要等"

**配置类**（端口占用、权限、认证、主机密钥、远端拒绝）重试无用，标 `✗` 停下；**网络类**（连接拒绝、超时、DNS、进程异常）指数退避 2s→…→30s。`arm_retry` 启动时插占位记录 `attempt = 0`，`tick_retries` 只处理 `attempt >= 1`——否则占位符会让程序在启动瞬间无限重连（集成测试抓到过）。

→ [runtime.md](runtime.md#失败分类与重试--classification--retry) · [logging/events.md](logging/events.md#隧道生命周期--tunnel-lifecycle)

## D7 用户配置与运行状态分离

`~/.config/sshpf/config.toml` 是用户意图（转发定义 + `enabled`），可手改，原子写（temp + rename）、`0600`、目录 `0700`；`~/.local/state/sshpf/state.toml` 是上次运行时间与末次状态，纯诊断。放 `~/.ssh/` 是错的（那是 OpenSSH 的领地），放项目目录也是错的（TUI 是全局工具）。

→ `src/config.rs` · 字段表见 [../README.md](../README.md#配置--configuration)

## D8 主机选择用内嵌下拉框，不是独立弹窗

新增转发最常做的第一件事是选主机，所以这个入口必须**看得见**。v1 初版是 `ctrl+h` 打开的居中模态框，实际使用中很难被发现。改成：`SSH 主机` 字段本身就是下拉（右侧 `▾`，空值时显示 `(N台可选)`），展开后**在表单内部**紧贴该字段渲染，不另开窗口。输入即过滤，**别名优先匹配**，别名无命中才回退 `user@host:port`——否则输入 `de` 会因某台机器的用户名是 `deploy` 而列出 `bastion-prod`，看起来像 bug。

→ `src/app/picker.rs`、`src/ui/dropdown.rs` · 键位见 [keymap.md](keymap.md#主机下拉--host-dropdown)

## D9 表单两阶段编辑

方向键是表单里最常按的键。按一下就改值，用户想移动光标却改了类型/勾选状态，很挫败。所以分 `Navigating`（默认，只移动高亮）与 `Editing`（在文本字段上按 `enter` 才进入）。视觉用三档区分（实心选中条 / 加粗高亮 / 普通），光标只在编辑态出现。枚举与开关类字段（类型、自动重连）不进编辑态，`enter`/`←`/`→`/`space` 直接操作。

→ [keymap.md](keymap.md#表单导航态--form-navigating) · `src/app/form.rs`

## D10 本地端口推荐：建议，不是决定

填远端端口后自动推导本地端口，用户始终可改。规则：≥1024 且空闲 → 同值；**<1024 → `remote + 2200`**（`22 → 2222`）；被系统占用 → 向上顺延；被其它条目占用 → 跳过（含**未运行**的——它一旦启动就会冲突）；用户手改过 → 停止自动覆盖；编辑既有条目 → 不推导。特权端口不能"从远端端口向上扫"：`port_available` 会把所有 <1024 端口报成不可用，扫描预算会在特权区间全部耗尽。

→ [import-forwards.md](import-forwards.md#本地端口顺延--local-port-shifting) · `src/model/`（`plan_local_port`）

## D11 风险操作：三条判断标准

命中任意一条就拦：① **会丢数据**（删除、放弃未保存输入）② **会断连接**（停用、重启、批量停用、保存编辑）③ **事后不易察觉**（批量启用一次开出很多监听端口）。反过来，可逆无副作用的**刻意不拦**：移动光标、过滤、看详情、启动未启用的转发、新建条目。给高频操作加确认会让用户习惯无脑按 `y`，真正的危险确认就失效了——这是确认框设计里最容易犯的错。

实现上 `ConfirmAction` 穷举所有受保护操作，`App::ask_confirm()` 统一生成提示，编译器强制新操作想清分类。表单保存拆两步（`validated_form_config` 只校验 → `commit_form_create/edit` 在**确认后**才停进程写配置），待保存配置暂存 `App.pending_form`，所以取消不会莫名断掉连接。

→ [keymap.md](keymap.md#安全规则--safety-rules) · `src/app/confirm.rs`

## D12 按键分发搬出 `main.rs`

`main.rs` 原本有约 530 行按键处理。二进制 crate 的私有函数无法被 `tests/` 调用，只能靠 pty 驱动真实 TUI——慢、脆、抓不到具体分支。现在 handler 在 `src/keys/`（lib 内），签名统一 `fn(&mut App, KeyEvent)`、本身无副作用，可以像纯函数一样测：`App::new` 不 spawn 进程 → 喂 `KeyEvent` → 断言 `App`。`main.rs` 只剩终端生命周期与事件循环。

→ [architecture.md](architecture.md#keys-键分发--srckeys) · [testing.md](testing.md#写新测试--write-a-new-test)

## D13 帮助是数据，不是散文

需求是"**所有**按键都要有说明，**不得遗漏**"。全称命题无法靠人工检查守住，而遗漏是**静默的**——用户不知道有个键存在，因此永远不会报 bug。所以 `src/help.rs` 没有任何渲染代码，只有 `SECTIONS` 数据；`tests/help_coverage.rs` 用 `include_str!` 双向比对代码 dispatch 与帮助目录。四个理由按重要性：可测试 > 可计数（滚动上限输入）> 双语同源（两份文档必然漂移）> 一处措辞（页脚与 `?` 不可能互相矛盾）。

书面后果：并列键用 `" / "` 分隔、修饰键 `+` 粘住、方向键用符号——不是为了好看，而是为了让测试**无歧义地切分**。

→ [keymap.md](keymap.md) · `src/help.rs`

## D14 覆盖测试两个方向都断言

正向（代码 dispatch 的键必须在帮助里）防**遗漏**；反向（帮助写的键必须真的 dispatch）防**撒谎**。反向看着多余，其实是最容易踩的坑：删掉 dispatch 忘了删帮助行很常见，而后果比缺失更严重——缺失只是信息不全，撒谎会摧毁整份帮助的可信度。反向断言曾经抓到 `form_editing` 多出 `ctrl+s`/`f10`：不是帮助写错，而是编辑态真的没 dispatch——用户按 `ctrl+s` 什么都不发生，而页脚一直写着"`ctrl+s` 保存"。测试发现的是"文档和代码里有一处真的错了"。

→ [testing.md](testing.md) · `tests/help_coverage.rs`

## D15 解析用花括号配平，把注释和字符串涂空

handler 体：先按 `fn <name>(` 找签名，再从第一个 `{` 做花括号配平走到深度归零。不用缩进（`rustfmt` 会改）、不用行号（改一行全错位）、不用正则（Rust 嵌套块让正则不可靠）。扫描前把**注释与字符串字面量涂成空格**（保留字符字面量，因为 `Char('j')` 的 `'j'` 就是要抽的载荷）：不清注释会把散文里的 `KeyCode` 当成真的 dispatch；涂成空格而不是删除，是为了让偏移量与原文对齐、报错能指回原文件。修饰键的作用域是**单个 match arm**（向两侧扫到最近的 `=>`/`;`/`{`/`}`），不是整行也不是整个函数体。

→ `tests/help_coverage.rs`

## D16 白名单要写成常量并注明理由

零例外是理想，但真实代码里有几处**刻意**的跨 section 引用，必须显式、有名字、有注释（`DOC_ONLY` / `CODE_ONLY`）。用"忽略失败"或注释掉断言来掩盖差异，等于把测试变成仪式；写成常量加理由，则是把例外变成**设计决策**。

→ [agents.md](agents.md#加按键--add-a-key) · `tests/help_coverage.rs`

## D17 符号归一化后再比对

代码里是 `KeyCode::Down`，界面写 `↓`；终端把 Shift+J 报成大写 `'J'`，源码里可能写成 `Char('j')` + `SHIFT` 守卫。所以比对前统一：`↓↑←→` → `down/up/left/right`，`shift+<字母>` → 大写字母。

→ `tests/help_coverage.rs`

## D18 逐列筛选用子串，主表过滤用模糊

主表 `/` 用 `fuzzy_match`（字符按序出现即命中）。列筛选**刻意**用大小写不敏感子串：端口列用模糊匹配，`1080` 会命中 `127.0.0.1:1080`、`127.0.0.1:10800`、`1:0:8:0`——筛选端口是为了**精确定位一行**，返回一堆无关行等于没筛。列筛选是"缩小范围"工具（`8080`、`localhost`、`-R`），子串才符合用途。

→ [import-forwards.md](import-forwards.md#为什么用子串而不是模糊匹配--substring-not-fuzzy) · `src/model/`（`col_match`）

## D19 撤销用快照栈，不用增量日志

每次改筛选**之前**压一份完整 `[String; 4]` 快照（上限 64，丢最旧）。输入 `8080` 压 4 份，`z` 逐字符回退；`ctrl+r` 清四列只压 1 份，一次 `z` 全恢复。快照是"4 个字符串的向量"这个状态的最小且不会算错的实现；增量式要处理"改哪一列、从什么改成什么"，撤销跨列操作时得记多条。64 份的内存代价可忽略。

→ `src/model/`（`ImportTable` 撤销栈） · 键位见 [keymap.md](keymap.md#导入列筛选--import-column-filter)

## D20 `f` 优先于筛选输入

`f` 用于循环切换"正在筛选哪一列"，而它的助记位置又正好在被筛选的输入里。实现上 `f` **优先**被处理：连按是"换列"，不是"打开输入然后把 f 写进去"。代价是字面 `f` 无法输入到筛选里——四列内容是端口、主机地址和指令名，没有哪列需要匹配字母 `f`；反过来若让 `f` 被输入吃掉，底部「f/←→ 选列」的提示就成了谎话，第二次按下时行为突变，是"看起来能用、用起来出错"的设计。

→ [import-forwards.md](import-forwards.md) · `src/keys/import.rs`

## D21 全选只看筛选后的视图

`check_all_visible` / `invert_visible` / `uncheck_all_visible` 只遍历 `visible()`。这不是实现细节，是安全约束：带筛选时若勾上隐藏行，用户看到 3 行只勾了 1 行，保存却创建 3 条隧道——起 3 个监听端口。同一条规则保护主表多选。

→ [import-forwards.md](import-forwards.md#全选--反选--全不选只看筛选后的视图--bulk-ticks-see-only-the-filtered-view) · `src/model/`

## D22 多选标记按 `TunnelId`，不按行号

过滤和删除都会让行移动。标记若存行号，一次删除后"第 3 行"指向另一条隧道，**批量删除会删错东西**。`MultiSelect` 用 `BTreeSet<TunnelId>`：天然去重、天然保序（提示与日志同序），删除后 `retain_existing` 清失效 id。四种批量操作**都过确认闸**（含"启用"——它一次开 N 个端口），提示用**有效条数**（标记 3 条但只有 1 条在跑，停用提示说 1 条）。标记为空或无需变化时不弹窗，直接提示——让人确认"什么都不做"是噪音。

→ [keymap.md](keymap.md#多选模式--selection-mode) · `src/app/select.rs`

## D23 鼠标走语义命中图，不重算坐标

渲染器每帧把"这块矩形是什么意图"记进 `HitMap`（`ImportColumnHeader(col)`，不是 `x=23`）。三条规则：只有渲染器写、后 push 的赢、零面积矩形忽略。"后 push 的赢"意味着弹窗后画就自动遮蔽下层，不需要 z-index 记账——绘制顺序本来就表达了层级。不存坐标因为渲染器已经算过布局，在事件处理里再推导就是**第二个真相来源**：改个列宽鼠标就点错而键盘完全正常。鼠标调用与键盘同一批 `App` 方法，所以**免费继承**安全规则。

**前提容易被忘掉**：命中图是**栈**不是 z-index。弹窗画在表格之后，若它不记录自己的矩形，表格矩形仍在栈顶，点击会穿过弹窗——视觉上盖住了，行为上没有。所以三个弹窗都先记 `HitTarget::Popup` 再画内容。

→ [tui.md](tui.md#命中图--hit-map) · `src/ui/hit.rs`、`src/app/mouse.rs`

## D24 日志格式用 `key=value`，不用 JSON

读者是"拿着终端排障的人"和 `grep`/`awk`，不是日志聚合器。JSON 每行重复键名，肉眼扫起来噪声占一半；`key=value` 用 `grep 'tunnel=3 '` 就能切出时间线。固定列顺序（时间、级别、seq、event）让 `cut`/`awk` 有意义；隧道身份 6 字段固定在前让任何一条隧道级记录脱离上下文也能读懂。值必须转义——裸换行会把一条记录劈成两条，后面所有按行处理全部错位。

→ [logging.md](logging.md#格式) · `src/logging/format.rs`

## D25 级别按"读的人要做什么"划分

不是按严重程度。关键判断：**掉线（`was_up=true`）是 WARN，从未连上的失败是 ERROR**。前者网络抖一下、系统会自愈；后者要用户动手。`tunnel.failed` 带 `retryable` 字段，读者不用猜"等一下会不会自己好"。`SSHPF_LOG_LEVEL=off` 只关常规记录，崩溃记录无条件写——想把日志关掉的人往往正是最需要那行崩溃信息的人。

→ [logging.md](logging.md#级别策略) · [logging/events.md](logging/events.md)

## D26 崩溃捕获两条通路，日志系统自己是最后防线

panic 走 hook（链式调用上一个，所以 ratatui 恢复终端的逻辑照常生效）；致命信号走 `sigaction`，handler 里只能 async-signal-safe 调用。递归崩溃由 `SA_RESETHAND` 兜住。第三层是"崩溃处理机制本身会挂"——panic 恰好发生在持锁写入过程中时，hook 再 `lock()` 就是自己等自己（**不是 poison，是同线程重入挂死**），所以用 thread-local 哨兵事先识别重入，改走裸 `write(2)`。

最容易被漏的一步是**解除内核对该信号的屏蔽再 `raise`**：不解除的话 `raise` 只排队，`_exit(139)` 抢先执行，进程就从"被 SIGSEGV 杀死"变成"以 139 退出"，core dump 也不产生。

→ [security.md](security.md#崩溃捕获--crash-capture) · `src/logging/`

## D27 防御式边界：三层降级

> 边界之外是任意的，边界之内不许 panic；出问题就降级，降级必须留痕。

第一层输入解析不 panic（按字节比较、`args_os`、`write_all`、`try_init`、u16 扩宽）；第二层只剩不变量可依赖处降级（`get()` + 安全默认值 + 每条降级一条记录，且每帧可触发的路径**限流**）；第三层日志系统自保。这一层只保"记录得下来"，不保"程序活着"——把崩溃静默掉才是更坏的选择。

→ [security.md](security.md#三层防御--three-layers)

## D28 Windows：纯 Rust 让平台差异几乎消失

v1 需要 Job Object 替代进程组、`KILL_ON_JOB_CLOSE` 替代 `PR_SET_PDEATHSIG`。改用 `russh` 后子进程不存在，进程树回收问题自然消失，`sys` 层只剩文件权限与退出信号两类（合计不到 100 行），**SSH 相关代码零平台分支**。

三个实测结论：① `ring` 加密后端必须——默认 `aws-lc-rs` 在 Windows x86-64 需要 NASM + CMake（同一句 `cargo check` 对比：`aws-lc` 请求 59×gcc/2×NASM/3×CMake，`ring` 只请求 9×gcc）；② 不再需要假 ssh 夹具（v1 曾用 `rustc` 编译假 ssh 可执行文件，因为 `/bin/sh` 脚本在 Windows 上无法执行）；③ 端口占用反查进程仅 Linux 可用。

→ [architecture.md](architecture.md) · `Cargo.toml`、`src/sys/`

## D29 分支保护

`main` 开启保护：4 个 CI 检查全绿（`strict: true`）、禁止强推与删除、`enforce_admins: true`、必需 review 批准为 0（单人仓库无法自我批准，实测 `mergeStateStatus` 会因此卡在 `BLOCKED`）。实测验证：CI 失败时 `gh pr merge` 被拒绝。

→ [testing.md](testing.md#ci-分工--ci-jobs) · `.github/workflows/ci.yml`

## D30 v0.3.0：拆分、分层、收口 `unsafe`

单体大文件（`model.rs` 2532 / `ui.rs` 3095 / `logging.rs` 1522 / `keys.rs` 1828 / `app.rs` 1602 行）拆成一层一职责的小文件，单文件生产行 ≤ 400（`tests/safety_guards.rs` 断言）；把重复实现收敛到 `src/text.rs` 与 `src/eventlog.rs`；`eventlog` 上移修掉"底层依赖上层"；`unsafe` 收口到 `src/logging/crash.rs`；文档按 agent 信息架构重建（入口 [index.md](index.md)）。

→ [architecture.md](architecture.md#模块地图--module-map) · [agents.md](agents.md)

## D31 v0.3.1：新建转发弹窗加宽

表单弹窗原本固定 74 列 —— 内宽 70，值列 49，导入面板的「原始指令」列 23 列。于是
`RemoteForward 9000 127.0.0.1:9001`（33 列）被画成 `RemoteForward 9000 12…`，恰好截掉
用户辨认一行所依据的那一列；而新建/编辑转发是这个界面**唯一**定义隧道的地方。改为固定
110 列（内宽 106，「原始指令」列 33 列，正好完整），窄终端仍由 `centered` 夹到帧内，
所以只增不减。宽度是 `layout::FORM_POPUP_W` 一个常量，两个测试看护：布局层按真实字符串
长度断言各列放得下，渲染层断言 120 列终端里渲染出的文本**不含** `…` 且窄终端里弹窗被夹住。

→ [tui.md](tui.md#浮层--overlays) · `src/ui/layout.rs`、`src/ui/form.rs`
