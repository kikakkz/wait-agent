# Context Optimization Guide

> Rust Skills 上下文优化策略与效果

## 概述

Rust Skills 采用三种方法优化上下文消耗，综合可节省约 **68%** 的 token 使用量。

| 优化方法 | 类型 | 适用场景 | 节省比例 |
|---------|------|---------|---------|
| **Skill 内容拆分** | 静态 | 大型参考 skill | 50-60% |
| **context: fork** | 动态 | 任务执行型 skill | 75-85% |
| **三层并行 Fork** | 动态 | 多 skill 协作分析 | 65-75% |

---

## 方法一：Skill 内容拆分（静态优化）

### 原理

将大型 Skill 的非核心内容拆分到子文件，主 SKILL.md 只保留核心路由逻辑，其他内容按需加载。

### 实施案例：rust-router

| 指标 | 优化前 | 优化后 | 节省 |
|------|--------|--------|------|
| 文件大小 | 18.7 KB | 8.1 KB | **56%** |
| 约 Token | ~4,700 | ~2,000 | **~2,700 tokens** |

### 文件结构

```
skills/rust-router/
├── SKILL.md (8.1KB - 核心路由，始终加载)
├── patterns/
│   └── negotiation.md (协商协议，按需加载)
├── examples/
│   └── workflow.md (工作流示例，按需加载)
└── integrations/
    └── os-checker.md (集成说明，按需加载)
```

### 移出内容详情

| 内容 | 移动到 | 大小 |
|------|--------|------|
| Negotiation Protocol | `patterns/negotiation.md` | 4.5 KB |
| Workflow Example | `examples/workflow.md` | 2.3 KB |
| OS-Checker Integration | `integrations/os-checker.md` | 1.3 KB |
| Skill File Paths | 删除（冗余） | 1.5 KB |

### 关键点：自动触发不受影响

Claude Code 的自动触发机制仅依赖 frontmatter 中的 `description` 字段：

```yaml
---
name: rust-router
description: "CRITICAL: Use for ALL Rust questions...
Triggers on: Rust, cargo, rustc, E0382, E0597..."
---
```

SKILL.md body 内容是触发**之后**的指导逻辑，移出到子文件不影响触发。

### 适用场景

- 包含大量参考内容的 Skill
- 有多种使用场景的 Skill
- 包含详细示例/模板的 Skill

---

## 方法二：context: fork 隔离执行（动态优化）

### 原理

使用 `context: fork` 让 Skill 在隔离的 subagent 上下文中执行，中间过程不污染主上下文，只返回摘要结果。

### 配置方式

```yaml
---
name: my-task-skill
description: "Task description"
context: fork
agent: general-purpose  # 或 Explore
---
```

### 实施案例

| Skill | 典型执行 Token | Fork 后主上下文 | 节省 |
|-------|---------------|----------------|------|
| `rust-skill-creator` | ~3,000 | ~500 (摘要) | **~83%** |
| `core-dynamic-skills` | ~2,000 | ~400 | **~80%** |
| `core-fix-skill-docs` | ~1,500 | ~300 | **~80%** |
| `rust-daily` | ~2,500 | ~500 | **~80%** |

### Fork 特性

| 特性 | 说明 |
|------|------|
| 隔离执行 | Skill 在新的独立上下文中运行 |
| 无对话历史 | Subagent **不能访问**主对话历史 |
| 结果摘要 | 输出被摘要后返回主对话 |
| 环境继承 | 工作目录、CLAUDE.md、环境变量继承 |

### 继承关系

```
主上下文 (Main Context)
├── 对话历史 ──────────────► ❌ 不继承
├── 当前工作目录 ──────────► ✅ 继承
├── CLAUDE.md ─────────────► ✅ 继承 (作为参考)
├── 预加载的 skills ────────► ✅ 可访问
└── 环境变量 ──────────────► ✅ 继承
```

### 适用场景

- 独立执行的任务（创建文件、同步数据等）
- 不需要对话历史的操作
- 探索/研究类任务

### 不适用场景

- 需要交互式追问用户
- 需要完整推理过程可见
- 结果细节很重要，不能被摘要

---

## 方法三：三层并行 Fork（实验性）

### 原理

基于元认知框架的三层模型，将分析任务并行分发到三个隔离的 layer analyzer，各自独立分析后返回摘要，在主上下文进行跨层综合。

### 架构

```
User Question
     │
     ▼
meta-cognition-parallel (协调者)
     │
     ├─── Fork → layer1-analyzer ──► L1 摘要
     │           (语言机制分析)
     │
     ├─── Fork → layer2-analyzer ──► L2 摘要    [并行]
     │           (设计选择分析)
     │
     └─── Fork → layer3-analyzer ──► L3 摘要
                 (领域约束分析)
     │
     ▼
Cross-Layer Synthesis (主上下文)
     │
     └─► 领域正确的架构方案
```

### 上下文消耗对比

**传统方式（主上下文）:**
```
├── 读取 m01-ownership    +1,200 tokens
├── 读取 m02-resource     +1,000 tokens
├── 读取 domain-fintech   +1,500 tokens
├── 中间推理              +2,500 tokens
└── 最终回答              +1,800 tokens
                          ────────────
                          ~8,000 tokens
```

**三层并行 Fork:**
```
├── L1 摘要返回           +600 tokens
├── L2 摘要返回           +600 tokens
├── L3 摘要返回           +600 tokens
└── 跨层综合+回答         +700 tokens
                          ────────────
                          ~2,500 tokens
```

**节省：~69%**

### 相关文件

- `skills/meta-cognition-parallel/SKILL.md` - 协调 Skill
- `agents/layer1-analyzer.md` - 语言机制分析 (m01-m07)
- `agents/layer2-analyzer.md` - 设计选择分析 (m09-m15)
- `agents/layer3-analyzer.md` - 领域约束分析 (domain-*)

### 使用命令

```bash
/meta-parallel <your Rust question>
```

### 测试场景

```bash
# 测试 1: 交易系统
/meta-parallel 交易系统报 E0382，trade record 被 move 了

# 测试 2: Web API
/meta-parallel Web API 中多个 handler 需要共享数据库连接池

# 测试 3: CLI 工具
/meta-parallel CLI 工具如何处理配置文件和命令行参数的优先级
```

---

## 综合效果估算

假设一次典型的 Rust 问答会话：

| 阶段 | 优化前 | 优化后 |
|------|--------|--------|
| rust-router 加载 | 4,700 | 2,000 |
| 多 skill 分析 | 8,000 | 2,500 |
| 任务执行 | 3,000 | 500 |
| **总计** | **15,700** | **5,000** |
| **节省** | - | **~68%** |

---

## 选择决策树

```
问题类型
    │
    ├── 大型参考 Skill?
    │   └── YES → 方法一: 内容拆分
    │             将非核心内容移至子文件
    │
    ├── 独立执行任务?
    │   └── YES → 方法二: context: fork
    │             添加 context: fork 到 frontmatter
    │
    └── 多层协作分析?
        └── YES → 方法三: 三层并行 Fork
                  使用 meta-cognition-parallel
```

---

## 最佳实践

### 1. 内容拆分原则

- 核心路由逻辑保留在 SKILL.md
- 示例、模板移至 `examples/`
- 集成说明移至 `integrations/`
- 详细参考移至 `references/`

### 2. Fork 使用原则

- 只对任务型 Skill 使用 fork
- 参考/指导型 Skill 不用 fork
- 需要用户交互的不用 fork

### 3. 并行分析原则

- 各分析任务应独立，无依赖
- 综合推理在主上下文完成
- 显式传递所有必要信息给 fork

---

## 验证清单

### 方法一验证

- [ ] rust-router 自动触发测试
  ```bash
  claude -p "E0382 错误怎么解决"
  claude -p "比较 tokio 和 async-std"
  ```

### 方法二验证

- [ ] Fork skill 执行测试
  ```bash
  /sync-crate-skills
  /rust-daily
  ```

### 方法三验证

- [ ] 三层并行分析测试
  ```bash
  /meta-parallel 交易系统报 E0382
  ```

---

## 版本历史

| 版本 | 日期 | 优化内容 |
|------|------|---------|
| 2.0.0 | 2025-01-22 | rust-router 内容拆分 (56% 节省) |
| 2.0.4 | 2025-01-22 | 4 个 skills 添加 context: fork (thanks @pinghe) |
| 2.0.5 | 2025-01-22 | 三层并行 Fork 实验性支持 |

---

**Created:** 2025-01-21
**Updated:** 2025-01-22
**Status:** ✅ Implemented (Methods 1-3)
