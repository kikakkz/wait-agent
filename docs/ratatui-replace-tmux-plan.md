# WaitAgent Ratatui 替换 tmux 计划

Version: `v1.0`  
Status: `Accepted`  
Date: `2026-07-27`

## 1. Purpose

本文档记录把 WaitAgent 从 tmux-first 架构迁移到 ratatui-first 架构的决策、顺序和边界。

触发原因：
- tmux 作为显示 pane 带来大量多进程通信异常、一致性处理、geometry 协调等复杂问题
- 现有 vendored tmux 构建、子进程生命周期、hooks/session-options 等机制过重
- ratatui + crossterm + 现成 terminal emulator 库已经足以支撑单进程 TUI

## 2. 决策摘要

### 2.1 不“在 tmux 上打补丁”，开并行路径

通过 `--ratatui` flag 启动一条完全由 ratatui 接管的新路径。tmux 路径继续保留，直到 ratatui 路径稳定后再切换默认并移除 tmux backend。

### 2.2 迁移顺序

```
Phase 0: --ratatui flag + 完整占位布局 + server/client skeleton + detach
Phase 1: footer 状态条真实数据
Phase 2: sidebar session catalog + 导航
Phase 3: 主 pane PTY + terminal emulator
Phase 4: 移除 tmux backend
Phase 5: 云端能力（local↔cloud、web client deferred）
```

### 2.3 保留分布式协议

`proto/waitagent/remote/v1/node_session.proto` 已经具备 node、authority、console、attachment 等分布式原语，继续复用。新架构把 monolithic binary 拆成 node server + TUI client，协议自然支撑 local→local、local→cloud、cloud→local 等拓扑。

## 3. 架构目标

```
waitagent-node (server / authority)
  ├─ 持有本地 PTY、terminal emulator、session catalog
  ├─ 实现 NodeSessionService gRPC server
  ├─ 支持 detach 后继续运行
  └─ 远程 ingress / session sync sidecars 保留

waitagent-cli (TUI client)
  ├─ ratatui 绘制 sidebar/footer/popup
  ├─ 主 pane 用 alacritty_terminal 渲染
  ├─ 通过本地 gRPC / Unix socket 连接 node
  └─ 支持 attach/detach/reconnect
```

## 4. 关键选型

| 组件 | 选型 | 说明 |
|---|---|---|
| UI 框架 | ratatui 0.27 | 已存在于 Cargo.toml |
| 事件/输入 | crossterm 0.27 | 已存在 |
| Terminal emulator | alacritty_terminal | 最成熟，server/client 各一份同步解析 |
| PTY 创建 | portable-pty 或 nix | 跨平台优先 portable-pty |
| 网络协议 | tonic/gRPC | 已存在，复用 node_session.proto |
| Web client | deferred | 后续用 xterm.js + WebSocket gateway |

## 5. 第一阶段目标（Phase 0）

最小可验证闭环：

1. `waitagent --ratatui` 启动完整三区域占位布局
2. client 退出后 node server 继续运行
3. 再次启动 `waitagent --ratatui` 能重新 attach
4. 按 `q` 正常退出并恢复终端

## 6. 与现有代码的衔接

| 现有模块 | 新角色 |
|---|---|
| `src/ui/*.rs` | 改为 ratatui widgets |
| `src/terminal/engine.rs` | TUI client 端 emulator 可选，server 端用 alacritty_terminal |
| `src/runtime/event_driven/*` | 复用事件状态机，输入源改为 crossterm + PTY fd |
| `src/runtime/workspace/*` | 逐步替换 tmux layout/control/main_slot_runtime |
| `src/infra/tmux_backend.rs` | 最终删除，中间阶段保留为 fallback |
| `proto/waitagent/remote/v1/node_session.proto` | 继续作为 server/client 和 node 间协议 |

## 7. 退出条件

当 `--ratatui` 路径功能完备且稳定后：
- `--ratatui` 设为默认
- 保留 `--tmux` 作为 fallback 一段时间
- 最终删除 tmux backend 和 vendored tmux submodule
