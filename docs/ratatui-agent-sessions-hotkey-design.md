# WaitAgent Ratatui Agent Sessions 热键设计

Version: `v2.0`  
Status: `Implemented`  
Date: `2026-08-08`

> v2.0 更新：实现已从"统一弹窗"改为"直接发送各 agent 自己的 session 管理 slash command"。本设计文档描述当前实际行为。

## 1. 目的

为 `waitagent --ratatui` 增加一个热键，让用户能够快速唤起当前 active session 所属 agent 工具（Kimi、Codex、Claude）自带的 session 管理界面。

当前行为：用户需要在另一个终端手动运行 `kimi --session`、`codex /resume` 或 `claude /resume` 来管理历史 session。本次设计在 TUI 内直接发送对应的 slash command 到当前 pane 的输入区，由 agent 自己处理列表、选择和恢复。

## 2. 范围

### 2.1 包含

- 热键触发并发送 agent 专属的 session 管理命令。
- 支持 Kimi Code CLI / Kimi Desktop、OpenAI Codex CLI / Codex Desktop、Anthropic Claude Code / Claude Desktop 三者。
- 支持 **local session** 和 **remote peer session** 两种目标：
  - local session：命令直接发给本机 local server。
  - remote peer session：命令经 local server 通过 gRPC 转发到 remote peer node 的 agent。
- 命令注入后自动追加回车执行。

### 2.2 不包含

- 不在 waitagent 内部维护统一的 session 列表弹窗。
- 不在 waitagent 内部读取或解析各 agent 的 session 数据文件。
- 不管理 session 的删除、重命名、归档等操作。
- 不处理 SSH remote host session（`ConnectRemoteHostPaneRuntime` 创建的 shell）的 agent session 管理。

## 3. 术语

- **Agent**：指 Kimi、Codex、Claude 这类 AI coding agent CLI/Desktop app。
- **AgentDetector**：现有插件 trait，负责从进程名和 pane 文本识别当前 session 的 agent 类型和状态。
- **Active session**：当前 TUI main pane 中处于焦点的 session。

## 4. 命令映射

| Agent | 发送的命令 | 说明 |
|---|---|---|
| Kimi | `/sessions` | Kimi Code CLI 内置的 session 列表 slash command |
| Codex | `/resume` | Codex CLI 内置的 resume slash command |
| Claude | `/resume` | Claude Code 内置的 resume slash command |

实现位于 `src/ratatui_node/client_runtime.rs` 的 `send_agent_session_command`。它根据当前 active session 的 `agent_command_name` 决定发送哪个命令。

## 5. 模块设计

### 5.1 关键代码

- `src/ratatui_node/client_runtime.rs`
  - `send_agent_session_command(stream, snapshot, status_message)`：根据 `snapshot.active_target` 找到对应 `SessionView`，按 `agent_command_name` 选择 slash command，通过 `send_paste_text` 注入输入区，再发送一个 `Enter` 键事件执行。
  - 事件循环中 `Ctrl+H` 绑定时调用上述函数。

### 5.2 与现有 AgentDetector 的关系

- `AgentDetector` 继续负责运行时识别 agent。
- `agent_command_name` 字段由 `AgentDetector` 输出决定；只有被识别为 `"kimi"`、`"codex"`、`"claude"` 的 session 才会触发对应命令。
- 识别为普通 shell 或其他未知命令的 session 会提示 "no session shortcut for ..."。

### 5.3 远程 peer session 路径

当 active session 是 `SessionTransport::RemotePeer` 时，命令经以下路径转发：

```
TUI client
    │
    │ PASTE_TEXT <target_id> <command>
    ▼
local ratatui node server
    │
    │ 判断 target_id 是 local 还是 remote peer
    ├─ local  → 直接注入 local session 的 PTY
    └─ remote → 通过 NodeSessionService 转发 PASTE_TEXT 到 remote peer node
                    │
                    ▼
            remote peer ratatui node
                    │
                    │ 注入 remote peer 上对应 session 的 PTY
                    ▼
            agent 收到 /sessions 或 /resume
```

远程文件粘贴使用独立的 `PASTE_FILE` 协议；session 管理命令复用现有文本输入转发机制。

## 6. UI 行为

### 6.1 热键

- 默认热键：`Ctrl+H`（Hotkey / History）。
- 只在 `Focus::Main` 时触发。
- 已有 overlay（sidebar focus、error log、help 等）打开时不触发。

### 6.2 触发后的动作

1. 按下热键后，TUI client 获取当前 active session 的 `target_id`。
2. 在 `snapshot.sessions` 中查找对应 `SessionView`。
3. 如果 `agent_command_name` 为空或不是已知 agent，底部状态栏显示 "no session shortcut for ..."。
4. 否则把对应 slash command（`/sessions` 或 `/resume`）作为键盘输入发送给目标 session，并追加一个 `Enter` 事件执行。
5. 后续列表渲染、选择、恢复完全由 agent 自己完成。

## 7. 错误处理

| 场景 | 行为 |
|---|---|
| 没有 active session | 状态栏提示 "no active session" |
| active session 不在 snapshot 中 | 状态栏提示 "active session not found" |
| 当前 session 不是已知 agent | 状态栏提示 "no session shortcut for ..." |
| 远程 peer 离线 | 命令发送失败由现有远程转发机制处理 |
| agent 不支持该 slash command | agent 自身报错，waitagent 不干预 |

## 8. 测试策略

### 8.1 单元测试

- `src/ratatui_node/client_runtime.rs` 中保留对 `send_agent_session_command` 的间接验证：确保 `Ctrl+H` 事件最终写出正确的 `PASTE_TEXT` + `INPUT` 命令序列。
- `src/domain/agent_detector.rs` 中验证 `accepts_at_reference` 正确包含 `kimi`、`claude`、`codex`，确保 `agent_command_name` 能被正确设置。

### 8.2 集成测试

- 在真实 Kimi/Codex/Claude session 中按 `Ctrl+H`，验证输入区出现 `/sessions` 或 `/resume` 并执行。
- 在 remote peer session 中按 `Ctrl+H`，验证命令经 gRPC 转发到远端并执行。

## 9. 限制与后续可能

- 各 agent 的 slash command 行为由 agent 自身决定；waitagent 只负责触发。
- 如果未来某个 agent 修改或移除了 `/sessions`/`/resume`，需要同步更新 `send_agent_session_command` 中的映射。
- 未来如需恢复统一弹窗，需要重新引入 `AgentSessionProvider`、协议消息和弹窗渲染代码；当前这些代码已清理。

## 10. 历史版本

- `v1.0`（2026-08-07）：原计划实现统一 session 列表弹窗，含 `AgentSessionProvider` trait、`AgentSessionRegistry`、`LIST_AGENT_SESSIONS` gRPC 协议和远程转发。实现过程中改为更简单的 slash command 方案，v1.0 相关代码已移除。
