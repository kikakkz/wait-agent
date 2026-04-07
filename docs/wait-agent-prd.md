# WaitAgent PRD

版本：`v1.0`  
状态：`Draft`  
日期：`2026-04-07`  
工作名：`WaitAgent`

## 1. 产品定义

一句话定义：

> 让多个 AI Agent session 共享一个终端，而不是让用户在多个终端之间来回切换；这些 session 可以来自同一台机器，也可以来自多台机器。

WaitAgent 的定位不是 Agent、不是 IDE、不是 Orchestrator，而是一个：

> Terminal-native interaction scheduler

它位于用户与多个 Agent session 之间，负责：

- 当前显示哪个 session
- 当前输入发给哪个 session
- 哪个 session 更值得用户现在处理
- 如何在不破坏 TTY 行为的前提下完成自动调度

WaitAgent 支持两种部署形态：

- `本地模式`：多个 session 运行在同一台机器，由本机 WaitAgent 聚合
- `网络模式`：用户只需给某个 WaitAgent 实例配置一个接入点；本机 session 会自然同步到 Server 侧可见，同时本地 CLI 仍保持完整可交互能力，Server 侧与 CLI 侧的交互自动同步

## 2. 背景与问题

多 Agent 工作流已经成为现实。开发者会同时运行多个 AI Agent 来完成并行任务，例如：

- 修复 bug
- 跑测试并自动修复
- 重构某个模块
- 审查 diff
- 执行部署或环境检查

真正的瓶颈不再是“能不能跑多个 agent”，而是：

> 当多个 agent 并行运行，并且都可能在不同时间点停下来等待人工确认时，用户如何用一个终端、低认知负担、零误输入地接管它们。

当前典型痛点：

- 需要维护多个 terminal/tab/pane
- 多个 agent 同时可能在等输入
- 多台机器上的 session 无法统一接管
- 用户很难知道当前最该处理哪个
- 为了看一眼后台进展，必须主动切上下文
- 输入很容易发错 session
- 用户注意力被多个等待中的 session 来回拉扯

WaitAgent 解决的不是“让 agent 更强”，而是：

> 把多 agent 的并发执行，压缩成人类可以稳定处理的串行确认流。

## 3. 产品目标

### 3.1 目标

- 在一个终端中承载多个独立 agent session
- 支持来自多台机器的 session 被统一接入同一个调度平面
- 任意时刻只暴露一个 session 给用户交互
- 本地模式和网络模式保持一致的使用体验，不要求用户学习两套交互模型
- 自动发现“可能正在等用户输入”的 session
- 在用户完成一轮输入后，最多自动切换一次到下一个待处理 session
- 不改变 agent 行为，不改变用户命令习惯，不破坏原始 TTY 语义
- 用户只需设置一个接入点，session 即可自然在 Server 端可见并可交互
- 在网络模式下，本地 CLI 和 Server 端都可以交互，并且状态自动同步

### 3.2 成功标准

- 用户不再需要维护多 terminal 心智模型
- 用户可以在一台 Server 机器上统一处理多台 Client 机器上的 session
- 用户不需要为了接入网络版而放弃本地 CLI 交互能力
- 用户知道自己的输入一定发给当前焦点 session
- waiting session 能被及时发现
- 自动切换不会打断当前 session 的连续对话
- 整个系统对 agent 是透明的

## 4. 非目标

MVP 暂不做：

- Dashboard
- 多 panel UI
- diff 可视化 UI
- orchestration rules
- prompt / 语义分析
- AI 总结
- 自动审批
- Web inbox / 浏览器控制台
- 团队协作审批
- MCP 集成

## 5. 目标用户

核心用户：

- 使用 `Claude Code / Codex CLI / Kilo / 其他 CLI Agent` 的开发者
- 高频使用终端的工程师
- 同时运行 `2~10+` 个 agent session 的用户
- 在多台开发机、远程主机、容器宿主机之间分散运行 agent 的用户
- 在开发流程中经常需要人工确认 diff、命令、修复结果、部署动作的用户

这些用户的共性是：

- 愿意接受 CLI 工作流
- 非常在意 TTY 保真
- 不想被迫迁移到重 UI 的 IDE/面板产品
- 对“输入是否发错对象”极其敏感

## 6. 设计原则

### 6.1 P0：不可破坏

- `100% TTY 透传`
- `不解析语义`
- `不修改 agent 行为`
- `不改变用户命令习惯`

### 6.2 P1：体验原则

- `单焦点`：永远只有一个 session 可见
- `自动但可控`：系统帮忙调度，但不抢控制权
- `输入保护`：不丢输入、不误输入、不串写
- `连续交互保护`：当前 session 还在持续对话时不切走
- `极简界面`：不把终端做成 IDE
- `部署一致性`：本地模式和网络模式使用同一种交互模型
- `双端镜像`：接入网络后，本地 CLI 与 Server 端对同一 session 的交互结果自动同步

## 7. 核心概念

### 7.1 Session

一个独立 PTY 对应一个 agent 进程实例。

在网络模式下，session 归属于某个 client node，但仍然由 server 统一调度。

### 7.2 Focus

当前唯一可见、唯一接收用户输入的 session。

这是“每个附着控制台”内的唯一焦点。

说明：

- 一个 WaitAgent UI 实例在任意时刻只有一个 focus
- 在网络模式下，Server 控制台和 Client 本地控制台各自维持自己的 focus
- 同一个 session 可以同时被多个控制台附着观察和交互

### 7.3 Waiting

系统基于非侵入启发式推断某个 session 很可能正在等待用户输入。

这不是协议真值，而是一个调度信号。

### 7.4 Waiting Queue

所有进入 waiting 状态的 session 按时间进入 FIFO 队列，供调度器选择。

### 7.5 Switch Lock

自动切换发生一次后立即加锁，防止连续自动跳转。

### 7.6 Peek

只读查看后台 session 的最近画面，不改变当前 focus，不接管输入。

### 7.7 Node

一个运行 WaitAgent Client 的机器节点。

Node 可以是：

- 本地开发机
- 远程服务器
- 容器宿主机
- CI / 沙箱环境宿主机

### 7.8 Server

WaitAgent Server 是统一控制面，负责：

- 汇聚多个 node 上报的 session
- 维护全局 session 注册表和聚合视图
- 为 Server 自己的控制台维护一套 focus 和 waiting queue
- 接收来自各 attached console 的事件并向对应 node 转发
- 把目标 session 的输出同步给所有已附着控制台

### 7.9 Client

WaitAgent Client 运行在 session 所在机器上，负责：

- 在本机创建和维护 PTY
- 把本机 session 的输出和状态同步到 Server
- 接收来自 Server 或本地 CLI 的输入并写回本地 PTY
- 在配置接入点后，仍然保留本地完整交互能力

### 7.10 Session 地址

在网络模式下，每个 session 必须具有全局唯一标识。

建议格式：

`<node-id>/<session-id>`

示例：

`devbox-1/claude-3`

### 7.11 Access Point

网络模式下，用户只需要为某个 WaitAgent 实例配置一个接入点。

接入后：

- 本机 session 自动注册到远端 Server
- 本地 CLI 保持原有可交互体验
- Server 侧可看到并交互这些 session
- 双方的输出、输入结果、状态变化自动同步

### 7.12 Attached Console

一个附着到 WaitAgent 的交互终端实例。

它可以是：

- Client 本地 CLI
- Server 端 CLI
- 同一机器上的另一个附着视图

每个 attached console 都遵循相同的单焦点规则，但不同 console 之间可以同时附着到同一 session。

## 8. 核心交互模型

### 8.1 单焦点模型

同一时刻只允许一个 session：

- 渲染到当前终端
- 接收用户输入
- 成为当前调度上下文

其他 session：

- 在后台继续运行
- 输出被缓冲保存
- 不直接渲染到屏幕

在网络模式下，这条规则仍然是全局成立的：

- 对任意一个 attached console，同一时刻只允许一个 session 获取该控制台的输入
- Server 端和 Client 端使用同一种单焦点交互模型
- 网络模式不会取消 Client 本地 CLI 的交互能力

### 8.2 自动调度规则

自动调度只允许发生在：

> 用户按下 `Enter` 提交输入之后

每次 `Enter` 会产生一次“调度资格”：

- 最多消费一次
- 若存在 waiting session，则切换到最早进入 waiting 的那个
- 若不存在 waiting session，则保持当前 focus

在网络模式下，Server 控制台维护的是跨 node 的聚合 waiting queue：

- 来自不同 node 的 session 进入同一 waiting queue
- 调度器按全局进入 waiting 的时间做 FIFO 选择
- 不因为 session 所在机器不同而改变调度规则

同时，Client 本地控制台仍可维护本机视角下的 waiting queue，以保持本地体验与单机版一致。

### 8.3 连续交互保护

以下情况视为同一 session 的连续对话，不应切走：

`prompt1 -> 用户输入 -> 当前 session 继续输出 -> prompt2`

因此实现上不应该在 `Enter` 后立刻切换，而应该：

- 用户按 `Enter`
- 当前 session 进入“本轮交互观察期”
- 若当前 session 仍持续输出，则优先视为同一轮交互
- 当这轮输出稳定收敛后，再判断是否消费这次自动调度资格

这条规则用于解决两个同时成立的要求：

- 自动切换只允许发生在用户输入提交之后
- 当前 session 的连续对话不能被其他 waiting session 打断

### 8.4 Switch Lock

每次自动切换发生后：

- 立即加锁
- 锁定期间禁止再次自动切换

解锁条件：

- 用户再次按下 `Enter`
- 用户手动切换 session

目标：

- 保证“一次输入，最多一次自动切换”
- 防止多个 waiting session 连续抢焦点

### 8.5 手动操作

基础操作：

- `Enter`：提交输入
- `Ctrl + Tab`：切换到下一个 session
- `Ctrl + 数字`：切换到指定 session
- `Ctrl + Shift + Tab`：切换到上一个 session

建议附加操作：

- 按 node 过滤 session 列表
- 直接跳转到某个 `<node-id>/<session-id>`

### 8.6 Peek

定义：

> 临时查看某个非焦点 session 的最近屏幕内容，只读，不接管输入，不改变当前 focus，不触发自动调度。

约束：

- stdin 仍只属于当前 focus session
- Peek 期间禁止向目标 session 写入输入
- 不修改 scheduler lock 状态
- 退出 Peek 后恢复原 focus 画面

典型用途：

- 查看某个 agent 是否卡住
- 查看某个 agent 是否已经进入 waiting
- 在不打断当前工作流的前提下做状态确认

### 8.7 双端同步模型

网络模式下，同一 session 可以同时被本地 CLI 和 Server 端附着。

同步原则：

- session 的 stdout / screen state 对所有已附着控制台同步
- 任一控制台发送的 stdin 都会进入同一个底层 PTY
- PTY 产生的结果会再广播回所有附着控制台

这意味着：

- 用户在本地 CLI 的交互，Server 端会看到
- 用户在 Server 端的交互，本地 CLI 也会看到
- WaitAgent 不尝试对多端输入做语义合并，只保证按到达顺序写入 PTY

## 9. Session 状态模型

### 9.1 状态定义

| 状态 | 含义 |
| --- | --- |
| `running` | 最近持续有输出 |
| `waiting_input` | 高概率正在等待用户 |
| `idle` | 无明显活动 |
| `exited` | 进程已退出 |

### 9.2 判断原则

要求：

- 非侵入
- 不依赖 agent 厂商协议
- 不依赖语义解析

MVP 启发式：

- 最近有输出
- 然后停止超过 `X ms`
- 期间没有新的 stdin
- 进程仍然存活

可选增强信号：

- CPU idle
- TTY mode 变化
- 光标稳定
- alt-screen 状态

说明：

`waiting_input` 在 MVP 中是一个高概率状态，不是绝对真值。

## 10. 系统架构

本地模式高层结构：

```text
Shell alias
   ↓
Proxy / PTY Manager
   ↓
Multiple PTYs (one per agent)
   ↓
Session Manager
   ↓
Focus Scheduler
   ↓
Renderer + Input Controller
   ↓
Single Terminal Output
```

网络模式高层结构：

```text
User Terminal
   ↓
WaitAgent Server
   ↓
Global Session Manager + Global Focus Scheduler
   ↓
Persistent Connections
   ↓
WaitAgent Clients (multiple machines)
   ↓
Local PTY Managers
   ↓
Agent Processes on each machine
```

统一体验约束：

- 本地模式和网络模式共用同一个 CLI / UI 交互模型
- 网络模式是在本地 WaitAgent 外增加同步与聚合能力，而不是替换本地体验

### 10.1 PTY Proxy Layer

职责：

- 启动或接管 agent 进程
- 为每个 agent 创建独立 PTY
- 透传 stdin/stdout/stderr
- 处理 ANSI、cursor、raw mode、resize

### 10.2 Session Manager

职责：

- 管理 session 生命周期
- 管理 session 状态和元数据
- 维护屏幕缓冲区
- 处理 session 退出、清理、崩溃隔离

### 10.3 Focus Scheduler

职责：

- 维护当前 focus
- 维护 waiting queue
- 实现 FIFO 调度
- 实现 enter-triggered scheduling
- 实现 switch lock

### 10.4 Input Controller

职责：

- 只把 stdin 写入当前 focus session
- 在用户输入未提交时禁止切换
- 防止输入误投、串写、丢失

### 10.5 Renderer

职责：

- 只渲染当前 focus session
- 切换时恢复目标 session 的完整屏幕上下文
- 不做语义级重绘
- 不做摘要视图

### 10.6 WaitAgent Server

职责：

- 接受多个 Client 的长连接
- 维护全局 session 注册表
- 维护 Server 控制台自己的 waiting queue 和 focus scheduler
- 接收用户输入并路由到目标 Client
- 把来自目标 PTY 的输出同步给所有附着控制台
- 提供跨 node 聚合视图和交互能力

### 10.7 WaitAgent Client

职责：

- 在本机启动 / 接管 agent 进程
- 维护本机 PTY 和屏幕缓冲
- 上报 session 输出、状态变化、生命周期事件
- 接收 Server 下发的输入、resize、attach 请求
- 把本地 CLI 的输入和远端输入统一写入本地 PTY
- 在断网后尽量保持本地 session 存活

### 10.8 Network Transport

要求：

- 支持 Server 与 Client 的持久连接
- 传输内容必须覆盖：stdout 片段、stdin 输入、resize、session 生命周期、状态变化
- 传输层不解释 agent 语义，只传输终端事件与元数据
- 支持重连与会话恢复
- 支持同一 session 被多个 console 附着后的事件广播

### 10.9 Global Session Namespace

在网络模式下，Server 侧必须维护全局命名空间，用于：

- 唯一定位某个远端 session
- 渲染时显示机器来源
- 输入路由与日志归属

## 11. UI 规范

WaitAgent 的 UI 必须极简：

```text
──────────────
[devbox-1/agent-2] active

...原始终端输出...

──────────────
2 sessions waiting
```

必须遵守：

- 不做卡片 UI
- 不做多 panel
- 不做 split view
- 不做 dashboard
- 不改变 agent 的原始输出风格

允许的最小 UI 元素：

- 顶部当前 session 标识
- 必要时显示 node 标识，例如 `devbox-1/claude-2`
- 底部 waiting 数量提示
- 极少量切换反馈

## 12. 关键边界处理

### 12.1 ANSI / 光标控制

- 完全透传
- 不修改 escape sequence
- 不对输出做语义解释

### 12.2 输入保护

- 用户输入进行中但未按 `Enter` 时，禁止切换
- 包括自动切换和手动切换

### 12.3 Resize

- 所有 PTY 同步窗口尺寸
- 保持各 session 行为一致
- 在网络模式下，由 Server 将当前终端尺寸同步到目标 Client，再由 Client 写入本地 PTY

### 12.4 后台输出

- 非焦点 session 的输出继续积累
- 不打断当前前台 session
- 切回时恢复完整上下文

### 12.5 崩溃处理

- session 退出后自动移除
- 不影响其他 session
- 若退出的是当前 focus，则切到下一个可用 session

### 12.6 网络断连

- Client 与 Server 断连后，node 标记为 `offline`
- 已注册的远端 session 在 Server 侧保留元数据，但变为不可达
- 若当前 focus session 所在 node 断连，则该控制台立即释放 focus 并切到下一个可用 session
- Client 重连后，应尽可能恢复原 session 关联而不是创建全新身份
- 断连不应影响 Client 本地 CLI 对本机 session 的持续交互

### 12.7 多端附着输入冲突

- 同一 session 允许多个 attached console 同时附着
- 若多个 console 同时输入，字节流会按到达顺序写入同一个 PTY
- WaitAgent 不对多端输入做语义级冲突合并
- 产品层面应尽量提供轻量提示，例如“remote typing”或“another console attached”，但不强制剥夺任一端的交互权

### 12.8 安全与接入控制

- Client 连接 Server 需要显式认证
- 会话输入和输出视为敏感数据，不允许明文匿名接入
- 最低要求应包括：节点身份、连接授权、可撤销凭证

### 12.9 连续 prompt

- `prompt1 -> 输入 -> prompt2` 视为同一 session 的连续交互
- 不允许被 waiting queue 抢走

## 13. MVP 范围

### 13.1 第一阶段：本地单机版

- alias 注入
- PTY proxy
- 多 session 管理
- 单焦点切换
- Enter 后单次自动调度
- 手动切换
- waiting heuristics
- Peek
- resize 同步
- crash isolation

### 13.2 第二阶段：网络版

- WaitAgent Server
- WaitAgent Client
- 多 node session 汇聚
- 全局 session 命名空间
- 跨机器全局 waiting queue
- Server / Client 双端同步交互
- 多 console attach 广播机制
- 接入点配置模型
- 断线重连与 node offline 状态
- 基础认证机制

### 13.3 暂不做

- dashboard
- orchestration
- AI 分析
- diff UI
- Web inbox
- session recording
- 多人协作审批
- auto approve rules
- MCP integration
- agent profiling

## 14. 验收标准

功能验收：

- 可同时运行 `>= 3` 个 session
- 前台输入不会进入后台 session
- waiting session 能正确进入 FIFO 队列
- 用户一次 `Enter` 后最多自动切换一次
- 连续交互场景不误切
- Peek 不改变输入归属
- resize 不破坏各 session
- 单个 session 崩溃不影响其他 session
- 可同时接入 `>= 2` 个 node
- Server 可统一交互多个 node 上的多个 session
- 断开单个 Client 不影响其他 node 的 session 交互
- 配置接入点后，本地 CLI 与 Server 端都可交互同一 session
- 在一端输入后，另一端可以看到自动同步后的终端结果

体验验收：

- 用户切 terminal 的频率明显下降
- 用户不再需要频繁轮询后台 session
- 用户对“当前输入给谁”没有歧义
- 用户不再需要登录多台机器分别接管 session
- 用户不需要因为接入 Server 而改变本地 CLI 使用方式

## 15. 为什么当前工具解决不了这个问题

WaitAgent 解决的不是“能不能跑多个 agent”，而是：

> 多 agent CLI 工作流中的人类交互调度问题。

也就是：

- 谁现在值得我看
- 我的输入该给谁
- 我怎样不切来切去
- 我怎样不把输入发错
- 我怎样在完全保留 TTY 行为的前提下，把并行 agent 变成人类可处理的串行确认流

### 15.1 tmux / Zellij

它们解决的是：

- 多 session 承载
- 多 pane / tab / workspace 管理

它们没有解决的是：

- 哪个 session 正在等用户
- waiting-aware FIFO 调度
- “Enter 后只自动切一次”
- 连续对话保护
- Peek 的只读焦点模型
- 跨机器 session 的统一调度与镜像同步交互

结论：

> tmux / Zellij 是终端复用基础设施，不是交互调度器。

### 15.2 Claude Code / Codex CLI / Kilo 这类 CLI Agent

它们解决的是：

- 单个 agent 的执行能力
- 本地终端代理能力
- agent 内部的工具调用、子任务、自动修改代码

它们没有解决的是：

- 多个独立 session 如何共享一个终端
- 多 session 场景下的人类输入调度
- vendor-neutral 的单焦点终端复用层
- 多台机器上的 session 如何被一个统一入口安全接管

即使某些工具支持 subagents，本质上仍然是 agent 内部 delegation，而不是：

> 多个独立 session 的终端级人类交互复用

### 15.3 Codex App / Cursor Background Agents / Warp / GitHub Copilot Cloud Agent

这些产品解决的是：

- 多 agent 并行执行
- 后台 agent 管理
- UI 面板、任务列表、管理视图
- 异步云端 coding workflow

它们没有解决的是：

- 100% TTY 透传前提下的 session 复用
- 单终端、单焦点的交互模型
- 不改变 CLI 命令习惯的 vendor-neutral proxy layer
- 一个 Server 统一接管多台机器 session 的终端级输入路由

它们通常依赖：

- 自己的 app surface
- 自己的 sidebar / web UI / cloud workflow
- 面板式的多 agent 管理模型

而 WaitAgent 要解决的是另一层：

> 我不想进入一个新的 agent 平台，我只想继续用终端，但让多个 agent 的人工确认过程变得可控。

### 15.4 总结

现有工具分别解决了不同层：

- `tmux / Zellij`：多 terminal 承载
- `Claude Code / Codex CLI`：单 agent CLI 能力
- `Codex App / Cursor / Warp`：厂商自有多 agent 管理
- `Copilot Cloud Agent`：异步后台 PR 工作流

WaitAgent 要解决的，是这些工具之间留下的空白层：

> 多 Agent CLI 工作流中的 human-in-the-loop interaction scheduler

并进一步补上：

> 本地 CLI 与远端聚合控制台之间的镜像交互层

## 16. 市场机会判断

这个产品首先解决的是作者自己的真实问题，而不是一个抽象的大市场命题。

### 16.1 为什么现在值得做

- 多 agent 并行已经成为真实工作流
- 开发者逐渐接受 agent 在后台并发运行
- 但人的确认链路仍然是串行的
- 当前缺少一个终端原生、跨厂商、低侵入的交互调度层
- 当前也缺少一个能跨多台机器统一接管 session 的终端级控制面

### 16.2 机会边界

这不是一个“所有开发者都需要”的产品。

这是一个：

- 面向 CLI 重度用户
- 面向多 agent 并行用户
- 一旦命中，替代成本很高

的高密度问题。

### 16.3 最大风险

- 被 `tmux plugin` 级替代
- 被某家 agent 厂商原生吸收
- 市场过窄

因此产品必须收敛，不能一开始扩张成大而全平台。

## 17. 产品边界判断

WaitAgent 最终不是“多 Agent 平台”，而是：

> 终端级交互调度层

它既可以运行在单机上，也可以运行成：

> 一个 Server 聚合多个 Client 节点的终端级交互调度层

准确表述：

- 不是 Agent 工具
- 不是 IDE
- 不是 Orchestrator
- 是 Terminal 级 Interaction Scheduler

一句话总结：

> 让多个 AI Agent 共享一个终端，而不是让用户切多个终端。

## 18. 后续文档建议

基于这份 PRD，下一步建议继续拆两份文档：

- `architecture.md`
  用于定义 PTY 模型、调度状态机、buffer 与 renderer 设计
- `mvp-plan.md`
  用于定义第一阶段实现路径、迭代顺序和验收项
- `protocol.md`
  用于定义 Server / Client 间的事件协议、鉴权和重连机制

## 19. 参考信息

以下资料用于校验当前工具状态和官方能力边界：

- OpenAI Codex CLI 官方文档
- OpenAI Codex 产品与计划说明
- Anthropic Claude Code 产品页与 Subagents 文档
- Cursor Background Agents 文档
- Warp Agent Platform 文档
- tmux 官方 Wiki
- Zellij 官方 Features 页面
- GitHub Copilot coding agent 官方文档

这些参考主要用于确认：

- 多 agent 并行已经是现实能力
- 厂商已有产品在做并行与后台执行
- 但终端原生、跨厂商、单焦点的人类交互调度层仍然没有被标准化
