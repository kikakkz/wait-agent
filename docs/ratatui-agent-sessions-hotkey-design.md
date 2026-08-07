# WaitAgent Ratatui Agent Sessions 热键设计

Version: `v1.0`  
Status: `Draft`  
Date: `2026-08-06`

## 1. 目的

为 `waitagent --ratatui` 增加一个热键，让用户能够快速列出并恢复当前 active session 所属 agent 工具（Kimi、Codex、Claude）在本机的历史 session。

当前行为：用户需要在另一个终端手动运行 `kimi --session <id>`、`codex resume <id>` 或 `claude -r <id>` 来恢复历史 session。本次设计在 TUI 内直接弹出 session 列表，选中后自动构造恢复命令并注入当前 pane 的输入区。

## 2. 范围

### 2.1 包含

- 热键触发 session 列表弹窗。
- 支持 Kimi Code CLI / Kimi Desktop、OpenAI Codex CLI / Codex Desktop、Anthropic Claude Code / Claude Desktop 三者的本地 session 列表。
- 从各 agent 的本地存储读取 session 元数据（id、标题、工作目录、最后更新时间）。
- 支持 **local session** 和 **remote peer session** 两种目标：
  - local session：TUI client 所在机器直接读取 agent 数据文件。
  - remote peer session：请求经 local server 转发到 remote peer node，由 remote peer node 读取其本地 agent 数据文件并返回列表。
- 选中 session 后，构造并注入对应的 CLI 恢复命令。

### 2.2 不包含

- 不读取云端同步的 session 历史（例如新版 Codex Desktop 的云线程）。
- 不读取加密的 app 内部数据库（例如 Codex `state_5.sqlite` 的完整结构、Claude Desktop 的 IndexedDB）。
- 不在 waitagent 内部恢复或运行 agent，只负责把恢复命令注入目标 pane。
- 不管理 session 的删除、重命名、归档等操作。
- 不支持 shell session 的历史命令列表（只针对 agent 工具）。
- 不处理 SSH remote host session（`ConnectRemoteHostPaneRuntime` 创建的 shell）的 agent session 列表；只覆盖 ratatui node 之间的 `RemotePeer` 会话。

## 3. 术语

- **Agent**：指 Kimi、Codex、Claude 这类 AI coding agent CLI/Desktop app。
- **AgentDetector**：现有插件 trait，负责从进程名和 pane 文本识别当前 session 的 agent 类型和状态。
- **AgentSessionProvider**：本设计新增插件 trait，负责从文件系统读取某个 agent 的 session 列表。
- **AgentSessionRegistry**：本设计新增 registry，按 agent name 分派到对应的 `AgentSessionProvider`。
- **Active session**：当前 TUI main pane 中处于焦点的 session。

## 4. 各 agent 的 session 来源

### 4.1 Kimi

来源：本机实际文件 + [Kimi Code CLI 官方文档](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/configuration/data-locations.md)。

| 项目 | 说明 |
|---|---|
| 数据目录解析顺序 | `KIMI_CODE_HOME` → `KIMI_HOME` → `~/.kimi-code` → `~/.kimi`（旧版/legacy） |
| Session 索引 | `<home>/session_index.jsonl` |
| 索引字段 | `sessionId`, `sessionDir`, `workDir` |
| Session 存储 | `<home>/sessions/<workDirKey>/<sessionId>/` |
| Session 元数据 | `<sessionDir>/state.json` 含 `title`, `lastPrompt`, `createdAt`, `updatedAt`, `forkedFrom` |
| 官方恢复命令 | `kimi --session <id>` 或 `kimi -c` |
| 格式 | JSON / JSONL；`workDirKey` 格式为 `wd_<slug>_<sha256-前12位>` |

### 4.2 Codex

来源：本机实际文件 + [OpenAI Codex 源码 `codex-rs/message-history/src/lib.rs`](https://github.com/openai/codex/blob/main/codex-rs/message-history/src/lib.rs)。

| 项目 | 说明 |
|---|---|
| 数据目录解析顺序 | `CODEX_HOME` → `~/.codex` |
| Session 索引 | `<home>/history.jsonl` |
| 索引字段（Rust struct） | `session_id: String`, `ts: u64`, `text: String` |
| `ts` 含义 | Unix epoch 秒数 |
| `text` 含义 | 该 session 的第一条用户提示文本，可作为标题 fallback |
| Session 存储 | `<home>/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl` |
| 官方恢复命令 | `codex resume <id>` |
| 格式 | JSONL；CLI 与 Desktop 历史可能不互通，Desktop 新版使用云同步 |

### 4.3 Claude

来源：本机实际文件 + 社区反向工程 [moru-ai/agent-schemas](https://github.com/moru-ai/agent-schemas)。

| 项目 | 说明 |
|---|---|
| 数据目录解析顺序 | `~/.claude`（Linux/macOS）、`%USERPROFILE%\.claude`（Windows） |
| Session 索引 | `<home>/history.jsonl` |
| 索引字段 | `display: String`, `pastedContents: Object`, `timestamp: Integer`, `project: String`, `sessionId: UUID` |
| `timestamp` 含义 | Unix epoch **毫秒**数 |
| `display` 含义 | 用户输入文本（可能被截断），作为标题 |
| `project` 含义 | 项目/工作目录绝对路径 |
| `sessionId` 含义 | 对应 `projects/<project>/chat_<uuid>.jsonl` 中的 UUID |
| Session 存储 | `<home>/projects/<project>/chat_<uuid>.jsonl` |
| 官方恢复命令 | `claude -r <id>` |
| 格式 | 未官方文档化的 JSONL；社区 schema 已覆盖 v2.0.76 / v2.1.1；`additionalProperties: true` |

### 4.4 读取优先级

1. 先读官方索引文件（`session_index.jsonl` / `history.jsonl`）。
2. 索引不存在或为空时，fallback 扫描 session 存储目录。
3. 索引文件损坏时，跳过损坏行，保留可解析行，不向用户报错中断。

## 5. 模块设计

### 5.1 新增文件

```
src/domain/
├── agent_session.rs              # trait + registry + 公共类型
├── agent_session_kimi.rs         # Kimi session provider
├── agent_session_codex.rs        # Codex session provider
├── agent_session_claude.rs       # Claude session provider
```

`src/domain/mod.rs` 追加：

```rust
pub mod agent_session;
pub mod agent_session_claude;
pub mod agent_session_codex;
pub mod agent_session_kimi;
```

### 5.2 与现有 AgentDetector 的关系

- `AgentDetector` 保持不变，继续负责运行时识别。
- `AgentSessionProvider` 是独立 trait，只负责文件系统读取。
- `AgentSessionProvider` / `AgentSessionRegistry` 需要能同时运行在 **TUI client 所在机器** 和 **remote peer node** 上；remote peer 通过读取本机文件来服务远端 viewer 的列表请求。
- 两者通过 **agent name 字符串**（`"kimi"`、`"codex"`、`"claude"`）关联。
- UI 层先用 `DetectorRegistry` 得到当前 session 的 agent name，再决定：
  - local session：直接查询本机 `AgentSessionRegistry`。
  - remote peer session：发送 `LIST_AGENT_SESSIONS` 请求给 local server，由 local server 转发到 remote peer。

### 5.3 `AgentSessionProvider` trait

```rust
use std::path::PathBuf;
use thiserror::Error;
use chrono::{DateTime, Utc};

/// 一个可恢复的 agent session 元数据。
#[derive(Debug, Clone)]
pub struct AgentSession {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// 恢复命令。
#[derive(Debug, Clone)]
pub struct ResumeCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Error)]
pub enum AgentSessionError {
    #[error("agent data directory not found")]
    HomeNotFound,
    #[error("failed to read session index: {0}")]
    IndexRead(#[source] std::io::Error),
    #[error("failed to parse session entry: {0}")]
    Parse(String),
}

pub trait AgentSessionProvider: Send + Sync {
    fn name(&self) -> &'static str;

    fn list_sessions(&self) -> Result<Vec<AgentSession>, AgentSessionError>;

    fn resume_command(&self, session: &AgentSession) -> ResumeCommand;
}
```

### 5.4 `AgentSessionRegistry`

```rust
pub struct AgentSessionRegistry {
    providers: Vec<Box<dyn AgentSessionProvider>>,
}

impl AgentSessionRegistry {
    pub fn new() -> Self { ... }
    pub fn register(&mut self, provider: Box<dyn AgentSessionProvider>) { ... }
    pub fn provider_for(&self, agent: &str) -> Option<&dyn AgentSessionProvider>;
    pub fn list_for(&self, agent: &str) -> Result<Vec<AgentSession>, AgentSessionError>;
}

impl Default for AgentSessionRegistry {
    fn default() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(agent_session_kimi::KimiSessionProvider));
        registry.register(Box::new(agent_session_codex::CodexSessionProvider));
        registry.register(Box::new(agent_session_claude::ClaudeSessionProvider));
        registry
    }
}
```

### 5.5 远程 peer session 的读取路径

当 active session 是 `SessionTransport::RemotePeer` 时，agent 数据文件在 remote peer node 上，不在 TUI client 本机。流程如下：

```
TUI client
    │
    │ ClientCommand::ListAgentSessions { target_id }
    ▼
local ratatui node server
    │
    │ 判断 target_id 是 local 还是 remote peer
    ├─ local  → 调用本机 AgentSessionRegistry
    └─ remote → 通过 NodeSessionService 转发 ListAgentSessionsRequest
                    │
                    ▼
            remote peer ratatui node
                    │
                    │ 调用本机 AgentSessionRegistry 读取本机文件
                    ▼
            ListAgentSessionsResponse
                    │
                    ▼
local ratatui node server
    │
    │ 把 AgentSession 列表返回给 client
    ▼
TUI client 渲染弹窗
```

#### 协议消息

在 `proto/waitagent/remote/v1/node_session.proto` 的 `NodeSessionEnvelope.body` oneof 中新增：

```protobuf
ListAgentSessionsRequest list_agent_sessions_request = 90;
ListAgentSessionsResponse list_agent_sessions_response = 91;
ListAgentSessionsRejected list_agent_sessions_rejected = 92;
```

消息定义：

```protobuf
message ListAgentSessionsRequest {
  string target_id = 1;
  string session_id = 2;
  string agent = 3;
}

message AgentSessionEntry {
  string id = 1;
  optional string title = 2;
  optional string cwd = 3;
  optional int64 updated_at_secs = 4;
}

message ListAgentSessionsResponse {
  string target_id = 1;
  string session_id = 2;
  repeated AgentSessionEntry sessions = 3;
}

message ListAgentSessionsRejected {
  string target_id = 1;
  string session_id = 2;
  string reason = 3;
}
```

同时在 `src/ratatui_node/state_event.rs` 的 `ClientCommand` 中新增：

```rust
ListAgentSessions { target_id: String },
```

在 `src/infra/remote_protocol.rs` 的 `ControlPlanePayload` 中新增对应 variant 和 payload struct。

#### 处理职责

- **TUI client**：发送 `ListAgentSessions` 命令；收到结果后渲染弹窗。
- **local server / StateEventLoop**：
  - 判断 `target_id` 对应 session 是 local 还是 remote peer。
  - local：调用 `AgentSessionRegistry::list_for(agent)`，把结果通过 `CommandOutcome::Data` 返回给 client。
  - remote：找到对应的 `RatatuiRemoteSession`，发送 `ListAgentSessionsRequest`。
- **remote peer node**：收到 `ListAgentSessionsRequest` 后，用本机 `AgentSessionRegistry` 读取，返回 `ListAgentSessionsResponse`。
- **local server**：收到 response 后把结果返回给 client。

## 6. 路径解析策略

### 6.1 home 目录发现

使用 `dirs::home_dir()` 或 `std::env::home_dir()` 获取用户主目录。优先读取各 agent 支持的环境变量，再 fallback 到默认路径。

### 6.2 跨平台

| Agent | Linux/macOS | Windows |
|---|---|---|
| Kimi | `~/.kimi-code`（默认），fallback `~/.kimi` | `%APPDATA%\kimi-desktop\...`（Deep path，可选支持） |
| Codex | `~/.codex` | `%USERPROFILE%\.codex` |
| Claude | `~/.claude` | `%USERPROFILE%\.claude` |

v1 先保证 Linux/macOS 稳定可用；Windows Desktop 深层路径作为后续增强。

### 6.3 环境变量

- `KIMI_CODE_HOME`：Kimi Code CLI 官方支持，覆盖整个数据根目录。
- `KIMI_HOME`：现有 hook config service 使用的变量，作为第二优先级。
- `KIMI_DATA_DIR`：旧版/legacy 变量，实现时作为额外 fallback。
- `CODEX_HOME`：Codex 官方支持，覆盖整个数据根目录。

Claude 目前无已知环境变量覆盖路径。

## 7. JSONL 解析策略

### 7.1 通用规则

- 使用 `serde_json::Value` 做防御性解析，未知字段忽略。
- 每一行独立解析；单行失败不中断整文件。
- 时间戳统一转成 `DateTime<Utc>`。

### 7.2 Kimi

**`session_index.jsonl`**（已确认格式）：

```json
{"sessionId":"session_03202c67-6ff9-4c69-afc3-0fb06ce4a241","sessionDir":"/home/user/.kimi-code/sessions/wd_root_94a6b4475803/session_03202c67-...","workDir":"/root"}
```

解析要点：
- `sessionId`：字符串，形如 `session_<uuid>`。
- `sessionDir`：session 目录绝对路径。
- `workDir`：工作目录。
- `title` / `updatedAt` 不在索引中，需读取 `<sessionDir>/state.json`。

**`state.json`**（已确认格式）：

```json
{
  "createdAt": "2026-07-13T09:21:11.863Z",
  "updatedAt": "2026-07-13T09:21:28.024Z",
  "title": "创建 0.0.162 tag，push，然后更新main version",
  "isCustomTitle": false,
  "workDir": "/root/wait-agent",
  "lastPrompt": "创建 0.0.162 tag，push，然后更新main version"
}
```

解析要点：
- `title` 缺失时 fallback 到 `lastPrompt`。
- `updatedAt` 是 ISO-8601 字符串。
- `forkedFrom` 等字段忽略。

### 7.3 Codex

**`history.jsonl`**（源码确认格式）：

```rust
pub struct HistoryEntry {
    pub session_id: String,
    pub ts: u64,
    pub text: String,
}
```

示例：

```json
{"session_id":"019ef493-41cb-7333-9587-b82548ec29df","ts":1782219826,"text":"在本机安装ssh server并启用"}
```

解析要点：
- `session_id` 是 UUID 字符串。
- `ts` 是 **秒级** Unix 时间戳。
- `text` 是用户第一条提示，直接作为标题。
- 同一 `session_id` 可能有多行（resume 后追加），去重后取最新 `ts`。

### 7.4 Claude

**`history.jsonl`**（社区 schema 确认格式）：

```json
{
  "display": "user prompt text",
  "pastedContents": {},
  "timestamp": 1704733200000,
  "project": "/absolute/path/to/project",
  "sessionId": "12345678-1234-1234-1234-123456789abc"
}
```

解析要点：
- `timestamp` 是 **毫秒级** Unix 时间戳。
- `display` 作为标题（可能被截断）。
- `project` 作为 `cwd`。
- `sessionId` 用于构造恢复命令 `claude -r <sessionId>`。
- `pastedContents` 忽略。

**`projects/<project>/chat_<uuid>.jsonl`**：
- 每行是 `UserMessage` / `AssistantMessage` / `SystemMessage` 等之一。
- `UserMessage` 含 `sessionId`, `timestamp`（ISO-8601）, `cwd`, `slug`（人类可读名称）。
- 如果 `history.jsonl` 缺失，扫描 `projects/` 目录，从每个 `chat_*.jsonl` 的第一条 `UserMessage` 提取元数据。

### 7.5 为什么仍需要防御性解析

虽然上面已经根据源码、官方文档和社区 schema 把字段确定下来，但**这些格式都不是我们控制的**，未来可能变化：

- Kimi 的 `state.json` 可能新增 `model`、`provider` 字段。
- Codex 的 `HistoryEntry` 可能把 `ts` 改成毫秒或 ISO 字符串。
- Claude 的社区 schema 基于 v2.1.1，未来版本可能新增消息类型或字段。

因此解析策略保持：

1. **未知字段忽略**：不设置 `deny_unknown_fields`。
2. **字段缺失有 fallback**：`title` 缺失用 `lastPrompt` / `text` / `display`；`updatedAt` 缺失用文件 mtime。
3. **单行失败不中断**：一行 JSON 损坏或字段类型不对，跳过该行。
4. **时间戳多格式兼容**：同时尝试 `u64` 秒、`u64` 毫秒、ISO-8601 字符串。
5. **索引损坏 fallback 扫描目录**：`session_index.jsonl` / `history.jsonl` 损坏时，直接扫描 `sessions/` 或 `projects/`。

## 8. UI 行为

### 8.1 热键

- 默认热键：`Ctrl+H`（Hotkey / History）。
- 只在 `Focus::Main` 时触发。
- 已有 overlay（sidebar focus、error log、help 等）打开时不触发。

### 8.2 弹窗行为

1. 按下热键后，TUI client 获取当前 active session 的 `target_id` 和 agent name。
2. 如果当前 session 不是已知 agent，弹窗提示 "No session list available for bash"。
3. 根据 session 类型选择读取路径：
   - **local session**：直接调用本机 `AgentSessionRegistry::list_for(agent)`。
   - **remote peer session**：发送 `ClientCommand::ListAgentSessions { target_id }` 给 local server，等待返回结果。
4. 如果读取失败（本地文件不存在、remote peer 离线、remote peer 无该 agent 数据等），弹窗显示错误信息，不崩溃。
5. 成功则弹出居中列表弹窗，展示：
   - session 标题（缺失时显示 id 前缀）
   - 工作目录（截断显示）
   - 最后更新时间（相对时间，如 "2h ago"）
6. 用户用 `↑/↓` 选择，`Enter` 确认，`Esc` 关闭。

对于 remote peer session，弹窗左上角可加一个小标记（如 `[remote]`）提示用户这些 session 来自远端节点。

### 8.3 选中后的动作

调用 `provider.resume_command(session)`，得到 `program` + `args` 后：

1. 把命令拼接成字符串（例如 `kimi --session <id>`）。
2. 通过 `PASTE_TEXT` 注入当前 pane 的输入区（相当于用户键盘输入）。
3. 再发送一个 `Enter` 键事件，自动执行注入的命令。

实现决定：自动追加并执行。这样选中 session 后用户无需再按回车。

## 9. 错误处理

| 场景 | 行为 |
|---|---|
| agent 数据目录不存在 | 弹窗提示 "No local sessions found for <agent>" |
| 索引文件不存在 | fallback 扫描 session 目录 |
| 索引文件损坏 | 跳过损坏行，显示可解析行，底部提示 "Some sessions could not be parsed" |
| 当前 session 不是 agent | 弹窗提示不支持 |
| 读取权限不足 | 弹窗显示具体 IO 错误 |
| Desktop app 历史不在本地 | 弹窗提示 "Desktop sessions may not appear" |
| 当前 session 是 remote peer 但远端离线 | 弹窗提示 "Remote node offline" |
| remote peer 读取 agent 数据失败 | 弹窗显示远端返回的具体错误 |
| 协议版本不支持 ListAgentSessions | 弹窗提示 "Remote node does not support agent session listing" |

## 10. 测试策略

### 10.1 单元测试

每个 provider 提供 fixture：

- `agent_session_kimi.rs`：测试 `resolve_kimi_home()` 的环境变量 fallback。
- `agent_session_codex.rs`：测试 `history.jsonl` 解析和目录扫描 fallback。
- `agent_session_claude.rs`：测试 `projects/` 扫描和 `history.jsonl` 解析。

使用临时目录构造 fixture，不依赖真实 `~/.kimi` / `~/.codex` / `~/.claude`。

### 10.2 集成测试

- 在 `tests/cli_smoke.rs` 或 ratatui 测试框架中验证热键触发弹窗。
- 验证非 agent session 下热键给出正确提示。

## 11. 限制与后续可能

- Codex Desktop 新版使用云同步，本地 `history.jsonl` 可能不完整；如需完整列表，需官方 API。
- Claude 没有官方 list CLI，但社区已提供 v2.0.76 / v2.1.1 的 JSON Schema；未来版本若变更字段，需要同步更新解析逻辑。
- Kimi Windows Desktop 路径很深且可能变化，v1 先聚焦 CLI / Linux / macOS。
- 未来可考虑把 session 列表常驻在 sidebar 的一个 panel 中，而非弹窗。

## 12. 任务拆分建议

1. `task.ratatui-agent-sessions-hotkey-phase1-design`：落地本设计文档和公共类型/registry。
2. `task.ratatui-agent-sessions-hotkey-phase2-kimi`：实现 `AgentSessionProvider` trait、`AgentSessionRegistry`、Kimi provider 和单元测试。
3. `task.ratatui-agent-sessions-hotkey-phase3-codex`：实现 Codex session provider 和单元测试。
4. `task.ratatui-agent-sessions-hotkey-phase4-claude`：实现 Claude session provider 和单元测试。
5. `task.ratatui-agent-sessions-hotkey-phase5-protocol`：新增 `ClientCommand::ListAgentSessions` 和 node session gRPC 消息（`ListAgentSessionsRequest/Response/Rejected`），实现 local server 到 remote peer 的转发。
6. `task.ratatui-agent-sessions-hotkey-phase6-ui`：实现热键弹窗、local/remote 路径选择、选择后注入恢复命令。
7. `task.ratatui-agent-sessions-hotkey-phase7-acceptance`：端到端验证（local 和 remote peer）和文档更新。
