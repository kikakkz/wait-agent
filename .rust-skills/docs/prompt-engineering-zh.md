# Prompt 约束技巧

> 如何通过 Prompt 有效约束大模型行为

## 核心原理

### 大模型的行为模式

```
大模型本质: 概率预测下一个 token

输入: "Rc cannot be sent"
模型思考: P("用 Arc") > P("分析领域约束后...")

默认行为: 选择概率最高的路径 (通常是最短、最常见的回答)
```

**问题**: 最高概率 ≠ 最佳答案

### 约束的作用

```
约束 = 改变概率分布

无约束: P("用 Arc") = 0.7, P("分析后回答") = 0.3
有约束: P("用 Arc") = 0.2, P("分析后回答") = 0.8

约束词提高了"遵循指令"路径的概率
```

---

## 约束词层级

### 强度等级

| 等级 | 约束词 | 效果 | 使用场景 |
|------|--------|------|----------|
| **L5 最强** | `CRITICAL`, `MUST`, `NEVER`, `MANDATORY` | 几乎 100% 遵循 | 核心规则，不可违反 |
| **L4 强** | `IMPORTANT`, `REQUIRED`, `ALWAYS` | 90%+ 遵循 | 重要规则 |
| **L3 中** | `should`, `recommended`, `prefer` | 70%+ 遵循 | 建议性规则 |
| **L2 弱** | `can`, `may`, `consider` | 50% 遵循 | 可选项 |
| **L1 最弱** | `optionally`, `if needed` | 30% 遵循 | 边缘情况 |

### 使用示例

```markdown
# L5: 核心规则
CRITICAL: You MUST load both L1 and L3 skills.
NEVER skip the domain constraint analysis.
This is MANDATORY and NON-NEGOTIABLE.

# L4: 重要规则
IMPORTANT: Always include the reasoning chain.
You are REQUIRED to reference domain rules.

# L3: 建议
You should trace through all layers.
It is recommended to provide code examples.

# L2: 可选
You can include additional context.
You may reference related skills.

# L1: 边缘
Optionally, mention performance implications.
```

---

## 有效约束模式

### 1. CRITICAL + MUST 组合

```markdown
CRITICAL: You MUST follow the meta-cognition framework.

效果:
- CRITICAL 提升注意力
- MUST 强制执行
- 组合使用 > 单独使用
```

### 2. 正反对比

```markdown
DO:
- Load both L1 and L3 skills
- Output reasoning chain
- Reference domain constraints

DON'T:
- Skip domain analysis
- Output only "use Arc"
- Ignore the context

效果: 明确边界，减少歧义
```

### 3. 示例对比

```markdown
CORRECT Response:
```
### Reasoning Chain
+-- Layer 1: Send/Sync Error
+-- Layer 3: Web Domain constraint
+-- Layer 2: Design decision
```

WRONG Response:
```
Use Arc instead of Rc.
```

效果: 具体示例 > 抽象描述
```

### 4. 条件约束

```markdown
**IF** domain keywords are present,
**THEN** you MUST load BOTH L1 and L3 skills.

**IF** error code detected,
**THEN** start from Layer 1 and trace UP.

效果: 清晰的触发条件
```

### 5. 步骤强制

```markdown
## STEP 1: IDENTIFY (MANDATORY)
...

## STEP 2: LOAD SKILLS (MANDATORY)
...

## STEP 3: OUTPUT (MANDATORY)
...

效果: 步骤编号 + MANDATORY 确保顺序执行
```

---

## 格式化增强

### 大写强调

```markdown
# 效果递增
should do this          → 弱
SHOULD do this          → 中
You SHOULD do this      → 中强
You MUST do this        → 强
CRITICAL: You MUST...   → 最强
```

### 结构化布局

```markdown
# 好 - 清晰的视觉层级
## STEP 1: IDENTIFY LAYER

### Layer 1 Signals:
- Error codes: E0382, E0597
- Keywords: borrow, lifetime

### Layer 3 Signals:
- Domain keywords: Web API, HTTP

---

# 差 - 无结构
First identify the layer. Look for error codes like E0382
or keywords like borrow. Also check for domain keywords...
```

### 表格约束

```markdown
# 好 - 表格清晰
| Keywords | Action |
|----------|--------|
| Web API, HTTP | Load domain-web |
| payment | Load domain-fintech |

# 差 - 文字描述
If you see Web API or HTTP, load domain-web.
If you see payment, load domain-fintech.
```

---

## 常用约束词汇表

### 强制执行类

| 词 | 含义 | 示例 |
|---|------|------|
| `MUST` | 必须 | You MUST include... |
| `NEVER` | 绝不 | NEVER skip... |
| `ALWAYS` | 总是 | ALWAYS check... |
| `REQUIRED` | 必需 | This is REQUIRED |
| `MANDATORY` | 强制 | MANDATORY step |
| `NON-NEGOTIABLE` | 不可商量 | This is NON-NEGOTIABLE |

### 禁止类

| 词 | 含义 | 示例 |
|---|------|------|
| `NEVER` | 绝不 | NEVER output only... |
| `DO NOT` | 不要 | DO NOT skip... |
| `AVOID` | 避免 | AVOID generic answers |
| `FORBIDDEN` | 禁止 | This is FORBIDDEN |
| `NOT ACCEPTABLE` | 不可接受 | Partial compliance is NOT ACCEPTABLE |

### 条件类

| 词 | 含义 | 示例 |
|---|------|------|
| `IF...THEN` | 如果...则 | IF domain detected, THEN load... |
| `WHEN` | 当 | WHEN error code present... |
| `UNLESS` | 除非 | UNLESS explicitly asked... |
| `ONLY IF` | 仅当 | ONLY IF user requests... |

### 强调类

| 词 | 含义 | 示例 |
|---|------|------|
| `CRITICAL` | 关键 | CRITICAL: This is essential |
| `IMPORTANT` | 重要 | IMPORTANT: Note that... |
| `NOTE` | 注意 | NOTE: This affects... |
| `WARNING` | 警告 | WARNING: Do not... |

---

## rust-skills 中的实践

### Hook 脚本约束

```bash
=== MANDATORY: META-COGNITION ROUTING ===

CRITICAL: You MUST follow the COMPLETE meta-cognition framework.
Partial compliance (only loading L1 skill) is NOT ACCEPTABLE.

# 使用技巧:
# 1. 标题用 === 包围，视觉突出
# 2. CRITICAL + MUST 组合
# 3. 明确说明什么是不可接受的
```

### Skill Description 约束

```yaml
description: "CRITICAL: Use for ALL Rust questions. Triggers on: ..."

# 使用技巧:
# 1. 以 CRITICAL 开头
# 2. 明确触发条件
# 3. 关键词用 Triggers on: 标记
```

### 输出格式约束

```markdown
## STEP 3: MANDATORY OUTPUT FORMAT

Your response MUST include ALL of these sections:

### Reasoning Chain
```
+-- Layer 1: [specific error]
|       ^
+-- Layer 3: [domain constraint]
|       v
+-- Layer 2: [design decision]
```

# 使用技巧:
# 1. MANDATORY + ALL 双重强调
# 2. 提供精确模板
# 3. 用代码块展示格式
```

### 正反对比约束

```markdown
CORRECT Response:
```
### Reasoning Chain
+-- Layer 1: Send/Sync Error
...
```

WRONG Response (stops at L1):
```
Problem: Rc is not Send
Solution: Use Arc
```

# 使用技巧:
# 1. 并列展示正确/错误
# 2. 标注 (stops at L1) 说明错在哪
# 3. 让模型清楚知道什么是不想要的
```

---

## 常见错误

### 1. 约束太弱

```markdown
# 差
You should probably include the reasoning chain.
It would be nice to reference domain constraints.

# 好
You MUST include the reasoning chain.
CRITICAL: Reference domain constraints.
```

### 2. 约束太模糊

```markdown
# 差
Follow the meta-cognition framework.

# 好
STEP 1: Identify entry layer (L1/L2/L3)
STEP 2: Load appropriate skills using Skill() tool
STEP 3: Trace through layers (UP or DOWN)
STEP 4: Output with reasoning chain format
```

### 3. 没有示例

```markdown
# 差
Output should include reasoning chain.

# 好
Output MUST include reasoning chain:
```
### Reasoning Chain
+-- Layer 1: [error]
+-- Layer 3: [constraint]
+-- Layer 2: [decision]
```
```

### 4. 没有反例

```markdown
# 差
Include domain analysis.

# 好
Include domain analysis.

WRONG (without domain analysis):
  "Use Arc instead of Rc"

CORRECT (with domain analysis):
  "From domain-web: handlers run on any thread,
   therefore Arc + State extractor is recommended"
```

---

## 约束效果验证

### 测试方法

```
1. 准备测试问题
   "Web API 报错 Rc cannot be sent"

2. 无约束测试
   → 观察默认输出

3. 加约束测试
   → 观察是否遵循

4. 调整约束强度
   → 找到最小有效约束
```

### 检查清单

```
□ 是否加载了指定的 Skills?
□ 是否执行了领域检测?
□ 是否输出了推理链?
□ 是否引用了领域约束?
□ 是否遵循了输出格式?
```

---

## 最佳实践总结

### 1. 约束强度选择

| 情况 | 推荐强度 |
|------|----------|
| 核心流程 | CRITICAL + MUST |
| 重要规则 | IMPORTANT / REQUIRED |
| 建议性 | should / recommended |
| 可选项 | can / may |

### 2. 约束结构

```markdown
1. 先声明约束级别 (CRITICAL/IMPORTANT)
2. 再描述具体要求 (You MUST...)
3. 提供正确示例 (CORRECT:)
4. 提供反面示例 (WRONG:)
5. 条件触发 (IF...THEN...)
```

### 3. 格式化

```markdown
✓ 使用大写强调关键词
✓ 使用表格组织规则
✓ 使用编号步骤
✓ 使用代码块展示格式
✓ 使用分隔线划分区域
```

### 4. 迭代优化

```
观察输出 → 识别偏差 → 增强约束 → 再次测试

常见调整:
- 约束词不够强 → 升级到 CRITICAL/MUST
- 规则不够具体 → 添加示例
- 遗漏情况 → 添加条件分支
```

---

## 一句话总结

> **约束的本质是改变模型的概率分布，让"遵循指令"的路径概率高于"默认行为"的路径。**

关键技巧:
1. **强度递增**: should < IMPORTANT < CRITICAL + MUST
2. **正反对比**: CORRECT vs WRONG 示例
3. **格式模板**: 具体格式 > 抽象描述
4. **条件触发**: IF...THEN 明确边界
