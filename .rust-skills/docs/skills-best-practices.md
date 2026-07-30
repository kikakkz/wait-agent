# Skills 编写最佳实践

> 基于 rust-skills 项目开发过程中的经验总结

---

## 1. CSO (Claude Search Optimization) - 描述优化

### 问题
Skills 的 `description` 字段是 Claude 匹配用户问题的关键，但很多 skill 描述不够优化，导致无法被正确触发。

### 解决方案

**使用 "CRITICAL:" 前缀提升优先级：**
```yaml
description: |
  CRITICAL: Use for tokio async runtime questions. Triggers on:
  tokio, spawn, select!, join!, timeout, channel...
```

**包含多种触发形式：**

| 类型 | 示例 |
|------|------|
| 关键词 | `tokio, spawn, select!, mpsc` |
| 错误码 | `E0382, E0597, E0277` |
| 错误信息 | `"cannot move out of"`, `"borrowed value"` |
| 用户问题 | `"how to use tokio"`, `"tokio 怎么用"` |
| 中文触发词 | `异步运行时, spawn 用法, 最新版本` |

**示例对比：**

```yaml
# ❌ 差的描述
description: "Tokio async runtime skill"

# ✅ 好的描述
description: |
  CRITICAL: Use for tokio async runtime questions. Triggers on:
  tokio, spawn, spawn_blocking, select!, join!, try_join!,
  mpsc, oneshot, broadcast, watch, channel, Mutex, RwLock,
  timeout, sleep, interval, "#[tokio::main]",
  tokio 怎么用, tokio 用法, 异步运行时, spawn 用法
```

---

## 2. 分布式触发架构

### 问题
单一入口点（如 rust-router）会成为瓶颈，所有问题都要先经过它路由。

### 解决方案

**每个 Skill 都可以独立触发：**

```
用户问题 → Claude 匹配所有 skills 的 description
         → 多个 skills 可能同时触发
         → rust-router 作为索引/fallback
```

**架构对比：**

| 模式 | 优点 | 缺点 |
|------|------|------|
| 单入口 | 集中管理 | 瓶颈、单点故障 |
| 分布式 | 并行匹配、容错 | 需要好的 CSO |

---

## 3. 动态 Skills 目录结构

### 结构

动态生成的 crate skills 直接放在 `~/.claude/skills/` 下，Claude Code 自动扫描：

```bash
~/.claude/skills/
├── tokio/
│   ├── SKILL.md
│   └── references/
├── tokio-task/          # 子技能
│   ├── SKILL.md
│   └── references/
├── serde/
│   ├── SKILL.md
│   └── references/
└── _shared/             # 共享文件（以 _ 开头不被扫描为 skill）
    └── rust-defaults.md
```

**命名约定：**
- 主技能：`{crate_name}/`
- 子技能：`{crate_name}-{feature}/`（如 `tokio-task/`, `tokio-sync/`）
- 共享目录：以 `_` 开头（不被扫描为 skill）

---

## 4. 文档完整性检查

### 问题
Skills 引用的 reference 文件可能不存在，导致读取失败但用户不知道原因。

### 解决方案

**在 SKILL.md 中添加检查指令：**

```markdown
## IMPORTANT: Documentation Completeness Check

**Before answering questions, Claude MUST:**

1. Read the relevant reference file(s) listed above
2. If file read fails or file is empty:
   - Inform user: "本地文档不完整，建议运行 `/sync-crate-skills {crate} --force` 更新"
   - Still answer based on SKILL.md patterns + knowledge
3. If reference file exists, incorporate its content into the answer
```

**创建检查命令：**
- `/fix-skill-docs` - 检查并修复缺失文件
- `/fix-skill-docs --check-only` - 只检查不修复

---

## 5. 工具优先级

### 问题
直接使用 WebSearch 可能获取过时信息，且绕过了专用工具。

### 解决方案

**使用 "PREFER" 而非 "DO NOT"：**

```markdown
## Tool Priority

**PREFER this skill's agents over WebSearch:**

1. `crate-researcher` agent for crate info
2. `docs-researcher` agent for API docs
3. **Fallback**: WebSearch (only if agents unavailable or fail)
```

**原因：**
- "DO NOT use WebSearch" 太绝对，如果 agent 不可用会导致任务失败
- "PREFER" 允许 fallback，更健壮

---

## 6. Skills TDD (测试驱动开发)

### 概念
"没有失败测试就没有技能" - 先定义技能应该解决的问题，再编写技能。

### 流程

**RED 阶段：**
1. 定义压力场景（用户问题 + 期望行为）
2. 在没有技能的情况下测试
3. 记录基线失败

**GREEN 阶段：**
1. 编写最小化技能解决失败
2. 测试验证改进

**REFACTOR 阶段：**
1. 识别漏洞
2. 添加对策
3. 测试边缘情况

### 压力场景模板

```markdown
# Pressure Scenario: {场景名}

## Skill Under Test
{skill_name}

## User Question
"{用户问题}"

## Code Context
```rust
// 相关代码
```

## Expected Behavior
- [x] 解释 XXX
- [x] 提供修复方案
- [x] 引用相关指南
```

---

## 7. Quick Reference 表格

### 问题
详细文档太长，用户需要快速参考。

### 解决方案

**在 SKILL.md 开头添加表格：**

```markdown
## Quick Reference

| Pattern | When | Example |
|---------|------|---------|
| Move | Transfer ownership | `let b = a;` |
| `&T` | Read-only borrow | `fn read(s: &String)` |
| `&mut T` | Mutable borrow | `fn modify(s: &mut String)` |
| `clone()` | Need owned copy | `let b = a.clone();` |
```

**原则：**
- 表格放在文件顶部
- 每个示例 < 20 词
- 详细内容放 references/

---

## 8. Commands vs Skills 热加载

### 发现
- **Skills** (`skills/*/SKILL.md`) - 可以热加载
- **Commands** (`commands/*.md`) - 需要重启才能加载

### 解决方案

**为每个命令创建 Skill 包装：**

```
commands/
└── fix-skill-docs.md        # 命令定义

skills/
└── core-fix-skill-docs/
    └── SKILL.md             # Skill 包装（可热加载）
```

**Skill 包装内容：**
```yaml
---
name: core-fix-skill-docs
description: |
  CRITICAL: Use when checking or fixing skill documentation.
  Triggers on: fix skill, check skill, /fix-skill-docs
---

# Fix Skill Documentation

{命令的简化版说明}
```

---

## 9. SKILL.md 标准结构

```markdown
---
name: {crate_name}
description: |
  CRITICAL: Use for {topic}. Triggers on:
  {keywords}, {error_codes}, "{questions}",
  {中文关键词}
---

# {Title}

> **Version:** {version} | **Last Updated:** {date}

You are an expert at {topic}. Help users by:
- **Writing code**: Generate code following the patterns below
- **Answering questions**: Explain concepts, troubleshoot issues

## Documentation

Refer to the local files for detailed documentation:
- `./references/xxx.md` - Description

## IMPORTANT: Documentation Completeness Check

**Before answering questions, Claude MUST:**
1. Read the relevant reference file(s)
2. If file read fails: Inform user "本地文档不完整，建议运行 /sync-crate-skills"
3. Still answer based on SKILL.md + knowledge

## Quick Reference

| Pattern | When | Example |
|---------|------|---------|
| ... | ... | ... |

## Key Patterns

### Pattern 1
```rust
// Code example
```

## API Reference Table

| Function | Description | Example |
|----------|-------------|---------|
| ... | ... | ... |

## Deprecated Patterns (Don't Use)

| Deprecated | Correct | Notes |
|------------|---------|-------|
| ... | ... | ... |

## When Writing Code

1. Best practice 1
2. Best practice 2

## When Answering Questions

1. Key point 1
2. Key point 2
```

---

## 10. 质量检查清单

创建 Skill 时确保：

- [ ] Description 有 "CRITICAL:" 前缀
- [ ] Description 包含中英文触发词
- [ ] Description 包含相关错误码
- [ ] 有版本和更新日期
- [ ] 有 "You are an expert..." 角色定义
- [ ] 有 Documentation 导航列表
- [ ] 有 Documentation Completeness Check 部分
- [ ] 有 Quick Reference 表格
- [ ] 有 Key Patterns 代码示例
- [ ] 有 Deprecated Patterns 表格
- [ ] 有 "When Writing Code" 最佳实践
- [ ] 有 "When Answering Questions" 指南
- [ ] 复杂内容拆分到 references/
- [ ] 创建了符号链接（动态 skills）

---

## 总结

| 经验 | 核心要点 |
|------|----------|
| CSO 优化 | "CRITICAL:" 前缀 + 多语言触发词 |
| 分布式触发 | 每个 skill 独立可触发 |
| 符号链接 | 动态 skills 需要链接到 ~/.claude/skills/ |
| 文档检查 | 读取失败时提示用户更新 |
| 工具优先级 | "PREFER" 而非 "DO NOT" |
| TDD | 先写压力场景，再写 skill |
| 表格优先 | Quick Reference 放顶部 |
| 热加载 | Commands 需要 Skill 包装 |
