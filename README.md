# WaitAgent

WaitAgent 是一个终端级交互调度层，用来解决多 AI Agent 并行运行时的人类接管问题。

它不试图替代 Agent、IDE 或 orchestration 平台，而是聚焦在一个更具体的问题：

> 让多个 AI Agent session 共享一个终端，而不是让用户切多个终端。

## 当前定位

WaitAgent 的核心目标是：

- 在一个终端中承载多个独立 agent session
- 任意时刻只暴露一个 session 给用户交互
- 自动发现可能正在等待用户输入的 session
- 在用户完成一轮输入后，最多自动切换一次到下一个待处理 session
- 保持 100% TTY 透传，不解析语义，不修改 agent 行为

## 支持的部署形态

### 本地模式

多个 session 运行在同一台机器，由本机 WaitAgent 聚合和调度。

### 网络模式

用户只需要为某个 WaitAgent 实例配置一个接入点：

- 本机 session 会自然同步到 Server 侧可见
- 本地 CLI 保持完整可交互能力
- Server 侧也可以交互这些 session
- 两端的终端结果和状态会自动同步

## 核心体验

- 单焦点：每个附着控制台内，同一时刻只交互一个 session
- 自动但可控：只在用户输入提交后触发一次自动调度资格
- 连续交互保护：当前 session 还在继续对话时不切走
- Peek：只读查看后台 session，不接管输入
- 极简 UI：不做多 panel，不做 dashboard，不做摘要面板

## 当前状态

当前仓库主要包含产品设计文档，尚未开始实现。

已完成文档：

- [产品 PRD](docs/wait-agent-prd.md)

## 下一步

建议后续继续补充：

- `docs/architecture.md`
- `docs/protocol.md`
- `docs/mvp-plan.md`

## 为什么做它

现有工具分别解决了不同层的问题：

- `tmux / Zellij` 解决多终端承载
- `Claude Code / Codex CLI` 解决单 agent CLI 能力
- `Codex App / Cursor / Warp` 解决厂商自有多 agent 管理

但“多 agent CLI 工作流中的 human-in-the-loop interaction scheduler”这层仍然缺少一个终端原生、跨厂商、低侵入的解法。
