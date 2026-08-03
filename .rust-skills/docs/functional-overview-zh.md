# Rust-Skills 功能概览

> 按功能分类的能力总结

## 三大功能分类




|     中文 |    英文 |   作用  |
|------|---------|---------|
|元认知类|Meta-Cognition|提升语义识别，追溯问题本质|
|动态 Skills 类|Dynamic Skills|按需生成 Skills，热加载 crate skills |
|信息获取类|Info Fetching|获取最新信息，紧跟 Rust 前沿 |


---

## 1. 元认知类 (Meta-Cognition)

### 核心价值

> 提升 Claude Code 对 Rust 学习和实践中遇到问题的**语义识别**能力，从表面错误追溯到问题本质。

### 解决的问题

| 问题 | 传统 AI | 元认知 AI |
|------|---------|-----------|
| E0382 错误 | "加 .clone()" | 追溯所有权设计，给出领域正确方案 |
| 类型选择 | 泛泛建议 | 基于领域约束的决策 |
| 设计问题 | 通用模式 | 符合领域最佳实践 |

### Skills 组成

#### 路由与框架

| Skill | 功能 |
|-------|------|
| **rust-router** | 入口路由，识别问题层级和领域 |
| **_meta/reasoning-framework** | 三层追溯方法论 |
| **_meta/layer-definitions** | L1/L2/L3 定义 |

#### Layer 1: 语言机制 (HOW)

识别 Rust 语言层面的问题类型：

| Skill | 元问题 | 识别信号 |
|-------|--------|----------|
| **m01-ownership** | 谁应该拥有这个数据？ | E0382, E0597, move, borrow |
| **m02-resource** | 需要什么所有权模型？ | Box, Rc, Arc, 智能指针 |
| **m03-mutability** | 可变性边界在哪里？ | E0499, E0502, mut, Cell |
| **m04-zero-cost** | 编译器能优化什么？ | E0277, generic, trait |
| **m05-type-driven** | 类型如何编码约束？ | newtype, PhantomData |
| **m06-error-handling** | 失败是预期还是异常？ | Result, Error, panic |
| **m07-concurrency** | 如何保证编译期安全？ | Send, Sync, async |

#### Layer 2: 设计选择 (WHAT)

识别架构和设计层面的问题：

| Skill | 元问题 |
|-------|--------|
| **m09-domain** | 领域规则如何变成类型？ |
| **m10-performance** | 性能瓶颈在哪里？ |
| **m11-ecosystem** | 如何与现有系统集成？ |
| **m12-lifecycle** | 资源生命周期模式？ |
| **m13-domain-error** | 失败恢复策略？ |
| **m14-mental-model** | 正确的心智模型？ |
| **m15-anti-pattern** | 常见认知陷阱？ |

#### Layer 3: 领域约束 (WHY)

识别领域特定的约束和规则：

| Skill | 领域 | 核心约束 |
|-------|------|----------|
| **domain-fintech** | 金融 | 审计追踪、精度、不可变 |
| **domain-web** | Web | 无状态、线程安全、异步 |
| **domain-cli** | 命令行 | 单线程、用户交互 |
| **domain-embedded** | 嵌入式 | no_std、资源限制 |
| **domain-cloud-native** | 云原生 | 分布式、可观测 |
| **domain-iot** | 物联网 | 低资源、遥测 |
| **domain-ml** | 机器学习 | 张量、推理优化 |

### 工作流程

```
用户问题: "Web API 报错 Rc cannot be sent"
    │
    ▼
┌─────────────────────────────────────────┐
│ rust-router 语义识别                     │
│ ├─ 检测: "Web API" → 领域: domain-web   │
│ ├─ 检测: "Send" 错误 → 机制: m07        │
│ └─ 决策: 双技能加载                      │
└─────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────┐
│ 三层追溯                                 │
│ L1: Rc 不是 Send (表面)                 │
│  ↑                                       │
│ L3: Web handlers 在任意线程运行 (约束)   │
│  ↓                                       │
│ L2: 使用 Arc + State extractor (设计)   │
└─────────────────────────────────────────┘
    │
    ▼
领域正确的架构方案
```

---

## 2. 动态 Skills 类 (Dynamic Skills)

### 核心价值

> Rust 生态中有大量 crate，无法为每个都预置 skills。利用 Claude Code 的**热加载特性**，按需生成 crate 专属 skills。

### 解决的问题

| 问题 | 解决方案 |
|------|----------|
| Crate 数量庞大 | 按需生成，而非预置 |
| 版本频繁更新 | 从最新文档生成 |
| 项目依赖各异 | 支持全局和项目级 |

### 存储策略

```
~/.claude/skills/           ← 全局 (常用 crate)
├── tokio/
│   ├── SKILL.md
│   └── references/
├── serde/
├── ratatui/
└── std/                    ← 标准库

项目/.claude/skills/        ← 项目级 (特定依赖)
├── sqlx/
├── sea-orm/
└── my-company-crate/
```

### 使用场景

| 场景 | 存储位置 | 示例 |
|------|----------|------|
| 常用 crate | 全局 `~/.claude/skills/` | tokio, serde, ratatui, std |
| 项目特定依赖 | 项目 `.claude/skills/` | sqlx, 业务 crate |
| 临时学习 | 项目级，用完删除 | 试用新 crate |

### Skills 组成

| Skill | 功能 |
|-------|------|
| **core-dynamic-skills** | 动态 skills 生成框架 |
| **rust-skill-creator** | 从文档生成 skills |

### 命令

| 命令 | 功能 |
|------|------|
| `/sync-crate-skills` | 从 Cargo.toml 批量生成 |
| `/update-crate-skill <crate>` | 更新指定 crate |
| `/clean-crate-skills` | 清理动态 skills |
| `/create-llms-for-skills <urls>` | 从 URL 生成 llms.txt |
| `/create-skills-via-llms <crate> <path>` | 从 llms.txt 创建 skill |

### 工作流程

```
方式一：从 Cargo.toml 自动生成
┌─────────────────────────────────────────┐
│ /sync-crate-skills                      │
│     │                                    │
│     ▼                                    │
│ 解析 Cargo.toml 依赖                     │
│     │                                    │
│     ▼                                    │
│ 检查 actionbook 是否有 llms.txt         │
│     │                                    │
│     ├─ 有 → 直接生成 skill              │
│     └─ 无 → 从 docs.rs 抓取生成         │
│     │                                    │
│     ▼                                    │
│ 写入 ~/.claude/skills/{crate}/          │
└─────────────────────────────────────────┘

方式二：手动为特定 crate 生成
┌─────────────────────────────────────────┐
│ /create-llms-for-skills <docs_url>      │
│     │                                    │
│     ▼                                    │
│ 抓取文档，生成 llms.txt                  │
│     │                                    │
│     ▼                                    │
│ /create-skills-via-llms tokio ./llms.txt│
│     │                                    │
│     ▼                                    │
│ 生成高质量 skill                         │
└─────────────────────────────────────────┘
```

### 生成的 Skill 结构

```
~/.claude/skills/tokio/
├── SKILL.md              # 主 skill 文件
│   ├── 触发关键词
│   ├── 核心概念
│   ├── 常用模式
│   └── 文档引用
└── references/           # 详细参考
    ├── runtime.md
    ├── task.md
    ├── sync.md
    └── io.md
```

### Skill 继承模式

对于复杂 crate，采用**父子结构**：

```
~/.claude/skills/
├── tokio/                 # 父 Skill: 广泛触发，概览
│   ├── SKILL.md
│   └── references/
│       └── rust-defaults.md  ← 共享规则
│
├── tokio-task/            # 子 Skill: 精确触发，深入
│   └── references/ → symlink
├── tokio-sync/
└── tokio-time/
```

**优势**:
- 触发精度：父广泛匹配，子精确匹配
- 规则复用：共享 `rust-defaults.md`
- 上下文节省：只加载需要的子 Skill

详见 `docs/architecture-zh.md` 中的"Skill 继承模式"章节。

---

## 3. 信息获取类 (Info Fetching)

### 核心价值

> 让 Claude Code 精准获取 Rust 语言和生态的**最新信息**，让用户紧跟 Rust 最前沿。

### 解决的问题

| 问题 | 解决方案 |
|------|----------|
| AI 知识截止日期 | 实时抓取最新信息 |
| 版本更新频繁 | 后台 agents 定期获取 |
| 信息源分散 | 聚合多个权威来源 |

### Skills 组成

| Skill | 功能 |
|-------|------|
| **rust-learner** | 版本和 crate 信息路由 |
| **rust-daily** | Rust 生态新闻聚合 |

### Agents (后台研究员)

| Agent | 数据源 | 获取内容 |
|-------|--------|----------|
| **rust-changelog** | releases.rs | Rust 版本特性、破坏性变更 |
| **crate-researcher** | lib.rs, crates.io | Crate 版本、features、依赖 |
| **docs-researcher** | docs.rs | 第三方 crate API 文档 |
| **std-docs-researcher** | doc.rust-lang.org | 标准库文档 |
| **clippy-researcher** | rust-clippy | Lint 规则解释 |
| **rust-daily-reporter** | Reddit, TWIR, Blog | 生态动态新闻 |

### 命令

| 命令 | 功能 |
|------|------|
| `/rust-features [version]` | 查询 Rust 版本特性 |
| `/crate-info <crate>` | 查询 crate 信息 |
| `/docs <crate> [item]` | 获取 API 文档 |
| `/rust-daily [day\|week\|month]` | Rust 生态新闻 |

### 信息源

```
┌─────────────────────────────────────────────────────────┐
│                    信息获取网络                          │
├─────────────────────────────────────────────────────────┤
│                                                          │
│  Rust 官方                    Rust 生态                  │
│  ┌──────────────┐            ┌──────────────┐           │
│  │ releases.rs  │            │ crates.io    │           │
│  │ 版本发布     │            │ lib.rs       │           │
│  └──────┬───────┘            └──────┬───────┘           │
│         │                           │                    │
│  ┌──────┴───────┐            ┌──────┴───────┐           │
│  │doc.rust-lang │            │ docs.rs      │           │
│  │标准库文档     │            │ crate 文档   │           │
│  └──────────────┘            └──────────────┘           │
│                                                          │
│  社区动态                                                │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐   │
│  │ Reddit       │  │ TWIR         │  │ Blog         │   │
│  │ r/rust       │  │ This Week    │  │ 官方博客     │   │
│  └──────────────┘  └──────────────┘  └──────────────┘   │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

### 缓存策略

| 数据类型 | TTL | 说明 |
|----------|-----|------|
| Rust 版本 | 168h | 发布不频繁 |
| Crate 信息 | 24h | 更新较频繁 |
| API 文档 | 72h | 相对稳定 |
| Clippy Lints | 168h | 跟随 Rust 版本 |

### 工作流程

```
用户: "tokio 最新版本有什么新特性？"
    │
    ▼
┌─────────────────────────────────────────┐
│ rust-learner 路由                        │
│ 识别: crate 版本查询 → crate-researcher │
└─────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────┐
│ crate-researcher agent                   │
│ 1. 检查缓存 (24h TTL)                   │
│ 2. 若过期，从 lib.rs 获取               │
│ 3. 返回: 版本、features、changelog      │
└─────────────────────────────────────────┘
    │
    ▼
最新、准确的 crate 信息
```

### rust-learner 详解

`rust-learner` 是信息获取类的**核心路由 Skill**，负责调度后台 Agents 获取实时信息。

```
用户问题 (版本/文档/特性)
        │
        ▼
┌───────────────────────────────────────────┐
│           rust-learner (路由层)            │
│                                            │
│  识别查询类型 → 选择对应 Agent → 后台执行  │
└───────────────────────────────────────────┘
        │
        ├──────────────────┬──────────────────┐
        ▼                  ▼                  ▼
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│rust-changelog│  │crate-        │  │docs-         │
│              │  │researcher    │  │researcher    │
│ releases.rs  │  │ lib.rs       │  │ docs.rs      │
└──────────────┘  └──────────────┘  └──────────────┘
```

**Agent 路由表**:

| 查询类型 | 调度的 Agent | 数据源 |
|----------|--------------|--------|
| Rust 版本特性 | `rust-changelog` | releases.rs |
| Crate 版本/信息 | `crate-researcher` | lib.rs, crates.io |
| 标准库文档 (Send, Arc...) | `std-docs-researcher` | doc.rust-lang.org |
| 第三方 crate 文档 | `docs-researcher` | docs.rs |
| Clippy lint 规则 | `clippy-researcher` | rust-clippy |
| Rust 生态新闻 | `rust-daily-reporter` | Reddit, TWIR |

**触发关键词**:

```
# 英文
latest version, what's new, changelog, Rust 1.x,
crate info, docs.rs, API documentation, which crate

# 中文
最新版本, 版本号, 新特性, crate 信息, 文档, 依赖
```

**关键设计原则**:

| 原则 | 说明 |
|------|------|
| **后台执行** | 所有 Agent 用 `run_in_background: true` |
| **不猜版本** | 永远通过 Agent 获取真实数据 |
| **禁用 WebSearch** | 不用 WebSearch 查 crate 信息 |
| **Fallback 机制** | actionbook → agent-browser → WebFetch |

### 核心依赖：Actionbook MCP

**Actionbook** 是 rust-skills 的**核心基础设施**，提供预计算的网页选择器。

#### 为什么 Actionbook 是核心依赖？

```
传统方式 (无 Actionbook):
┌─────────────────────────────────────────────────────────┐
│ 1. 访问 lib.rs                                           │
│ 2. 获取整个 HTML (可能 100KB+)                           │
│ 3. 解析 DOM，猜测选择器                                  │
│ 4. 提取数据 (可能失败，选择器不对)                        │
│ 5. 重试... 消耗大量 tokens 和时间                        │
└─────────────────────────────────────────────────────────┘

有 Actionbook:
┌─────────────────────────────────────────────────────────┐
│ 1. 查询 actionbook: "lib.rs crate info"                 │
│ 2. 获得精确选择器:                                       │
│    {                                                     │
│      "version": ".crate-version",                       │
│      "description": ".crate-description",               │
│      "features": ".crate-features li"                   │
│    }                                                     │
│ 3. 直接用选择器提取数据                                  │
│ 4. 一次成功，高效准确                                    │
└─────────────────────────────────────────────────────────┘
```

#### Actionbook 工作机制

```
┌─────────────────────────────────────────────────────────┐
│                    Actionbook MCP                        │
├─────────────────────────────────────────────────────────┤
│                                                          │
│  预计算选择器数据库                                       │
│  ┌─────────────────────────────────────────────────┐    │
│  │ lib.rs:                                          │    │
│  │   version: ".crate-version"                      │    │
│  │   features: ".crate-features li"                 │    │
│  │                                                  │    │
│  │ docs.rs:                                         │    │
│  │   signature: ".fn-signature"                     │    │
│  │   description: ".docblock"                       │    │
│  │                                                  │    │
│  │ releases.rs:                                     │    │
│  │   changelog: ".release-notes"                    │    │
│  │   features: ".language-features li"              │    │
│  └─────────────────────────────────────────────────┘    │
│                                                          │
│  MCP 接口                                                │
│  ├── search_actions(query) → 搜索匹配的站点             │
│  └── get_action_by_id(id) → 获取完整选择器              │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

**MCP 工具**:

| 工具 | 参数 | 返回 |
|------|------|------|
| `search_actions` | query, type, limit | action IDs, 预览, 相关度 |
| `get_action_by_id` | id | URL, 选择器, 元素类型 |

#### 为什么是"核心依赖"？

| 维度 | 无 Actionbook | 有 Actionbook |
|------|---------------|---------------|
| **准确性** | 选择器可能失效 | 预计算，经过验证 |
| **效率** | 解析整页 HTML | 精确选择器提取 |
| **Token 消耗** | 高 (传输大量 HTML) | 低 (只传选择器) |
| **可靠性** | 网站改版就失败 | 集中维护，快速更新 |
| **可扩展性** | 每个站点单独适配 | 统一接口，一次接入 |

**rust-skills 依赖 Actionbook 的场景**:

```
rust-learner
    ├── crate-researcher → actionbook: lib.rs 选择器
    ├── docs-researcher → actionbook: docs.rs 选择器
    ├── rust-changelog → actionbook: releases.rs 选择器
    └── std-docs-researcher → actionbook: doc.rust-lang.org 选择器
```

### agent-browser 浏览器自动化

**agent-browser** 是执行层工具，配合 Actionbook 的选择器进行数据提取。

```bash
# 基本工作流
agent-browser open <url>           # 打开页面
agent-browser get text <selector>  # 用 actionbook 选择器提取
agent-browser close                # 关闭
```

**核心命令**:

| 命令 | 功能 |
|------|------|
| `open <url>` | 导航到页面 |
| `snapshot -i` | 获取可交互元素 (带 ref) |
| `get text <selector>` | 提取文本 |
| `click @ref` | 点击元素 |
| `fill @ref "text"` | 填充输入 |
| `screenshot` | 截图 |

### 工具链协作

三层工具链形成完整的信息获取管道：

```
┌─────────────────────────────────────────────────────────┐
│                     信息获取管道                          │
├─────────────────────────────────────────────────────────┤
│                                                          │
│  Layer 1: 路由决策 (rust-learner)                        │
│  ┌─────────────────────────────────────────────────┐    │
│  │ 用户: "tokio 最新版本"                            │    │
│  │ 决策: crate 查询 → crate-researcher agent        │    │
│  └─────────────────────────────────────────────────┘    │
│                          │                               │
│                          ▼                               │
│  Layer 2: 选择器获取 (Actionbook MCP)                    │
│  ┌─────────────────────────────────────────────────┐    │
│  │ search_actions("lib.rs crate")                   │    │
│  │ get_action_by_id("lib.rs/crates")               │    │
│  │ 返回: { version: ".crate-version", ... }        │    │
│  └─────────────────────────────────────────────────┘    │
│                          │                               │
│                          ▼                               │
│  Layer 3: 数据提取 (agent-browser)                       │
│  ┌─────────────────────────────────────────────────┐    │
│  │ agent-browser open lib.rs/crates/tokio          │    │
│  │ agent-browser get text ".crate-version"         │    │
│  │ 返回: "1.49.0"                                   │    │
│  └─────────────────────────────────────────────────┘    │
│                          │                               │
│                          ▼                               │
│  输出: tokio 1.49.0, features, 文档链接                  │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

**优先级 Fallback**:

```
actionbook MCP → agent-browser CLI → WebFetch (仅备用)
     │                  │                │
     ▼                  ▼                ▼
获取预计算选择器   执行浏览器抓取    最后手段 (无选择器)
```

**为什么这个顺序？**

| 工具 | 优点 | 缺点 |
|------|------|------|
| Actionbook + agent-browser | 精确、高效、可靠 | 需要预计算选择器 |
| WebFetch | 简单、无依赖 | 获取整页、解析困难 |

---

## 功能协作

三类功能相互配合：

```
┌─────────────────────────────────────────────────────────┐
│                     用户问题                             │
└─────────────────────────┬───────────────────────────────┘
                          │
          ┌───────────────┼───────────────┐
          │               │               │
          ▼               ▼               ▼
    ┌───────────┐   ┌───────────┐   ┌───────────┐
    │ 元认知类   │   │ 动态 Skills│   │ 信息获取类 │
    │           │   │           │   │           │
    │ 语义识别   │   │ crate 知识 │   │ 最新信息   │
    │ 追溯本质   │   │ 按需加载   │   │ 实时获取   │
    └─────┬─────┘   └─────┬─────┘   └─────┬─────┘
          │               │               │
          └───────────────┼───────────────┘
                          │
                          ▼
            ┌─────────────────────────┐
            │ 领域正确 + 最新准确的   │
            │ 架构方案                │
            └─────────────────────────┘
```

### 协作示例

**问题**: "用 tokio 1.40 写一个 Web 服务，处理并发请求"

```
1. 元认知类: 识别 Web + 并发 → 加载 domain-web + m07
2. 动态 Skills: 加载 tokio skill (最新 API 模式)
3. 信息获取类: 确认 tokio 1.40 特性和最佳实践

→ 输出: 基于最新 tokio 版本、符合 Web 领域约束的方案
```

---

## 总结

| 功能类 | 核心价值 | Skills/Agents |
|--------|----------|---------------|
| **元认知** | 语义识别，追溯本质 | rust-router, m01-m15, domain-* |
| **动态 Skills** | 按需生成，热加载 | core-dynamic-skills, rust-skill-creator |
| **信息获取** | 最新信息，紧跟前沿 | rust-learner, 8 个 agents |

**rust-skills 的目标**：

让 Claude Code 成为一个**理解问题本质**、**掌握最新生态**、**按需扩展知识**的 Rust 开发助手。
