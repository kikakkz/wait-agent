# 裸 Windows 原生应用支持路线图

总目标：让 WaitAgent 在不依赖 WSL/Cygwin/MSYS 的裸 Windows 上完整运行，同时保持 Linux/macOS 功能不降级。

## 当前进度

- 阶段 2 已完成：本地 IPC 从 Unix Domain Socket 切换到跨平台的 `LocalListener`/`LocalStream`。
  - Unix 继续走 UDS。
  - Windows 走 `127.0.0.1:<port>` TCP 回环 + marker 文件做服务发现。
  - `node_is_running` / `list` / `stop` / `detach` 等命令在 Windows 下通过 marker 文件工作。
  - Linux 验证：`cargo test --release`、`cargo clippy -- -D warnings`、`cargo fmt --check` 全绿。

- 阶段 3 已完成：Windows 进程/session 隔离 + 启动锁。
  - `platform/process.rs` 的 Windows `spawn_detached` 使用 `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`。
  - `platform/file_lock.rs` 的 Windows `StartupLock` 使用 `LockFileEx`。
  - 添加了 `windows-sys` 作为 Windows-only 依赖。
  - Linux 验证：`cargo test --release`、`cargo clippy -- -D warnings`、`cargo fmt --check` 全绿。
  - Windows 行为需在真实 Windows 环境或 CI 上验证（阶段 8）。

## 剩余阶段

### 阶段 4：Windows 本地 PTY

- 充实 `platform/pty.rs` 跨平台抽象；Unix 实现使用 `rustix_openpty`，Windows 为 stub。
- 替换 `ratatui_node/authority_host_session.rs` 里对 `rustix_openpty` 的依赖，改用 `platform::pty`。
- 替换 `ratatui_node/authority_host_io_loop.rs` 里 PTY resize/set_nonblocking 的 Unix 直接调用。
- 新增 `platform/wake_pipe.rs`，把 `authority_host_io_loop` 的 shutdown/request-wake `UnixStream::pair()` 替换为跨平台实现。
- `alacritty_terminal` 提到通用依赖，Windows 下也能使用其终端模拟器。
- `local_session.rs` 已平台化：Unix 走 `SHELL` 并注册 process monitor；Windows 走 `powershell.exe`/`cmd.exe` 并跳过 monitor（阶段 7 再补）。
- `authority_host_session.rs` 的 Unix child spawn 已收敛到 `#[cfg(unix)]`；Windows 返回明确错误，待阶段 4b 实现 ConPTY。

验收标准：
- Windows 上能创建本地 shell session 并看到终端输出。
- TUI 能正确渲染本地 session。
- Linux 验证全绿。

### 阶段 4b：Windows authority-host PTY ✅（可选，可与阶段 5/6 并行）

- 在 Windows 上实现 `authority_host_session.rs` 的 ConPTY child spawn。
- 让 `--connect` peer 在 Windows 上也能 hosting 远程 viewer 的 session。

完成内容：
- `platform/pty/windows.rs`：`ConPty`（两对 `CreatePipe` 匿名管道 + `CreatePseudoConsole`，`Drop` 里 `ClosePseudoConsole`）与 `ConPtyChild`（`WaitForSingleObject(0)` + `GetExitCodeProcess` 实现 `try_wait`/`id`）。`spawn_shell` 用 `CreateProcessW` + `STARTUPINFOEXW` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` 直接 spawn（stable Rust 的 `Command` 无法表达该 attribute，`raw_attribute`/`spawn_with_attributes` 仍为 nightly-only）；spawn 成功后关闭子进程端管道句柄，保证 output 管道能到达 EOF。`Cargo.toml` 增加 `Win32_System_Console`/`Win32_System_Threading` features。
- `authority_host_io_loop.rs`：按平台 cfg——Unix 路径（Poller + SIGCHLD）字节级不变；Windows 无 poller，每 session 一个阻塞 `ReadFile` reader 线程，经新增 `PtyOutput` 请求变体把字节回传 IO 循环（线程不碰 SharedState）；循环线程阻塞在 `recv_timeout(200ms)` 上周期跑 `check_child_exits`；输入在循环线程上阻塞写 ConPTY input。`SessionState`/`RegisterSession` 的 PTY 字段按平台 cfg（unix=`File`，windows=`ConPty`），child 字段统一为 `SessionChild` 类型别名。
- `authority_host_session.rs`：Windows `spawn` 真实实现（`default_shell()` 复用 local_session 的 powershell/COMSPEC 逻辑，signal env 经 `apply_to_hashmap` 进入环境块）；`pty_master` 字段按平台替换为 `conpty: Option<ConPty>`。
- `state_loop.rs` 注册路径按平台 cfg（unix 克隆 master fd，windows `take()` ConPTY）；测试模块与 `/dev/null` 构造点补 `cfg(unix)` 门控。

验收状态：Linux 全部验证全绿（`cargo test --release` 602+3+2、`cargo clippy -- -D warnings`、`cargo fmt --check`）。Windows 侧按代码审计达标，未经真实 Windows 编译/运行验证，留待阶段 8。

### 阶段 5：Windows agent signal ✅

- 替换 `platform/signal.rs` 里 Unix datagram socket 的实现。
- Unix 继续使用 UDS；Windows 使用命名管道（`\\.\pipe\waitagent-signal-<port>`）。
- 新增 `platform::signal::default_signal_endpoint(port)`，根据平台生成合适的 endpoint。
- `runtime.rs` 改用 `default_signal_endpoint` 生成 signal endpoint。
- 修改 `bin/waitagent-agent-signal-send.rs`，Windows 下通过命名管道发送 signal。
- `build.rs` 里跳过 agent signal sender C 编译的逻辑保留：Windows 下用 Rust 实现，不需要 C bundle。

验收标准：
- agent hooks（claude/codex/kimi）在 Windows 上能向 node server 发送 lifecycle signal。
- Linux 验证全绿。

### 阶段 6：远程 node ingress / authority 的 UDS 替换 ✅

已完成子步骤：
- 6a：`platform/remote_ipc.rs` 跨平台控制 socket 抽象（`RemoteControlAddr`/`RemoteControlStream`/`RemoteControlListener` + tokio 异步版本；Unix 走 UDS 且文件名与历史一致，Windows 走 `127.0.0.1` TCP 回环派生端口 + ephemeral 端口 ready 握手）。
- 6b：`remote_runtime_owner_runtime.rs` 的 owner control socket 全部迁移到 remote_ipc（含 tokio event loop、sidecar ready 握手、启动锁）。
- 6c：`remote_node_ingress_server_runtime.rs` 的 owner control socket 迁移（可用性探测、workspace 注册、create-session 控制通道、ready 握手；单机模式 acceptor 通过 `OwnerControlAccept` trait 同时兼容 UDS 与新抽象）。
- 6d-1：session-sync owner socket 迁移；无调用方的过渡 listener `#[cfg(unix)]` 保留。
- 6d-2：authority transport bridge 全链路跨平台化——remote_ipc 新增 authority endpoint 抽象（FNV 哈希命名 + Windows `.port` marker 文件发布 ephemeral 端口 + 目录扫描发现）；`RemoteAuthorityTransportRuntime`、ingress `bridges: HashMap<RemoteControlAddr, …>`、`RatatuiRemoteSession::open` 全部迁移；owner 注册协议 Unix 下字节不变。
- 6d-3a/3b：`remote_session_creation_service` 残留 connect 迁移；publication/tmux 遗留/session runtime 死代码 `#[cfg(unix)]` 门控（逐项 grep 验证）；存活路径 `UnixStream::pair()` 迁移到 `socket_pair()`（Unix=UDS pair，Windows=TCP 回环 pair）；全仓残留扫描确认其余 UDS 点均在 `cfg(unix)` 内或属平台文件/4b 区域。

验收状态：Linux 全部验证全绿（`cargo test --release` 602+5、`cargo clippy -- -D warnings`、`cargo fmt --check`）。Windows 侧行为（TCP 回环替代 UDS、marker 文件发现）按代码审计达标，未经真实 Windows 编译/运行验证（环境限制，aws-lc-sys 阻塞交叉检查），留待阶段 8。

### 阶段 7：Windows 进程监控 ✅

- 替换 `process_monitor/linux.rs` 里基于 Linux netlink proc connector 的实现。
- Windows 下使用 WMI、ETW 或轮询等替代方案监控子进程生命周期。
- 保持 `ProcessEvent::Fork` / `Exec` / `Exit` 语义不变。
- 完成内容：`process_monitor/mod.rs` 平台缝重构（`PlatformPtyFd` 类型别名、event_loop 按 linux/macos/windows 拆分、timerfd/`OwnedFd` 全部 `#[cfg(target_os = "linux")]` 化）；`windows.rs` 用 Toolhelp32 快照差分实现（新 pid→`Fork`、exe 路径变化→`Exec`、pid 消失→`Exit`，基线快照防误报，`Win32_System_Diagnostics_ToolHelp` 加入依赖）；`local_session.rs` 的 Windows 路径接上 monitor 注册。Linux/macOS 的 netlink/macos event_loop 逻辑未动。
- 残留修复：`local_session_sync_backends.rs` 的无条件 `use std::os::fd::AsRawFd` 加 `#[cfg(unix)]`，`authority_host_supports_at` 的 /proc 前台进程探测包 `#[cfg(unix)]`（Windows 桩返回 false，只依赖 catalog 记录）。全仓 `std::os::unix`/`std::os::fd` 残留审计完成：除 `authority_host_io_loop.rs`（属阶段 4b ConPTY 范围）外，其余均在 `cfg(unix)` 内或平台文件内。

验收状态：Linux 全部验证全绿（`cargo test --release` 602+3+2、`cargo clippy -- -D warnings`、`cargo fmt --check`，本机亲自复核）。Windows 侧 Toolhelp32 实现按代码审计达标，未经真实 Windows 编译/运行验证，留待阶段 8。

### 阶段 8：端到端 Windows 验证 ✅

- 在 Windows 真机或 CI 上跑通完整流程：启动 node server、创建本地 session、运行 agent、连接远程 host。
- 修复在真实 Windows 环境中暴露的兼容性问题。
- 更新构建文档和发布流程。

验收标准：
- Windows 上 `cargo test --release` 通过（或至少与 Linux 同等的测试覆盖）。
- Windows 安装包/可执行文件能正常生成和运行。
- Linux/macOS 功能没有退化。

完成内容：
- `.github/workflows/ci.yaml` 新增 `windows-check`（`cargo check --all-targets` + `cargo clippy -- -D warnings` + `cargo fmt --check`）与 `windows-test`（`cargo test --release`，533 测试）两个 `windows-latest` job。
- Rust 工具链 1.86 → 1.88（russh 的 Windows 依赖 pageant 0.2.1 使用 let-chains，1.88 才稳定）。
- 经 5 轮 CI 反馈修复真实 Windows 编译/测试问题：bin crate 路径（`crate::` → 自包含命名管道发送）；windows-sys 命名管道项实际位于 `Win32::System::Pipes`/`Foundation`；Unix-only import/测试代码按 `cfg(unix)`/`cfg(all(test, unix))` 门控；`ConPtyChild` 持裸 `HANDLE` 的 Send/Sync 问题；alacritty_terminal 0.25.1 Windows `Pty` API 差异（`child_watcher().pid()`）；Windows 侧 clippy 警告清零；测试 helper 用线程名拼临时文件名（含 `::`，Windows 非法字符）；`extract_agent_signal_sender` 在 Windows 上查找伴生 exe（含测试 deps 目录场景）；剪贴板粘贴测试按宿主平台选 `PlatformContext`。
- 本地交叉检查工具链：WSL + mingw（`x86_64-w64-mingw32-gcc`）使 `cargo check --target x86_64-pc-windows-gnu [--all-targets]` 可在 Linux 本机 1-2 分钟完成一轮验证，后续 Windows 改动不必等 CI。

验收状态：GitHub Actions `windows-latest` 上 6 个 job 全绿（run 对 `47be311`，含 windows-test 533 passed / 0 failed）。`cargo test --release` 在真实 Windows 上通过。诚实备注：交互式人工端到端（真机 TUI 里启动 server、attach session、跑 agent、连远程 host）未执行——CI 测试已覆盖其中的可编程部分（本地 PTY spawn、signal env、bundle 提取、粘贴分发等），剩余为人工体验验证，发现问题按阶段 8 流程继续修。

## 通用约束

- 每次阶段改动必须保持 Linux/macOS 功能完整。
- 每次阶段结束都要跑：
  - `cargo fmt --check`
  - `cargo clippy -- -D warnings`
  - `cargo test --release`
- 优先使用 `src/platform` 下的跨平台抽象，业务代码不直接调用 `std::os::unix`、`libc`、`rustix` 等 OS-specific API。
