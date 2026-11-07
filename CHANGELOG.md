# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

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

[0.1.1]: https://github.com/can2049/sshpf/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/can2049/sshpf/releases/tag/v0.1.0
