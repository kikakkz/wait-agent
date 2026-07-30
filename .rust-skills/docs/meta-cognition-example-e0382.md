# Meta-Cognition 示例: E0382 交易系统

> 演示三层认知模型如何将表面错误追溯到领域约束，提供更深层次的解决方案。

## 用户问题

> "我的交易系统报 E0382，数据被 move 了"

---

## 传统回答 (Bad)

```
"用 .clone() 解决"
```

**问题**: 只修复表面症状，不理解根本原因。

---

## Meta-Cognition 追溯过程

### Step 1: 识别入口层

| 信号 | 入口层 | 追溯方向 |
|------|--------|----------|
| E0382 | **Layer 1** (语言机制) | 向上追溯 ↑ |

**初始 Skill**: m01-ownership

---

### Step 2: Layer 1 分析 (m01-ownership)

**核心问题**: 谁应该拥有这个数据？

| Error | 不要只说 | 而要问 |
|-------|----------|--------|
| E0382 | "Clone it" | **谁应该拥有这个数据？** |

**思考提示**:
1. 这个数据的领域角色是什么？ → "交易记录" = Transaction Record
2. 所有权设计是有意的吗？ → 需要检查领域约束
3. 修复症状还是重新设计？ → 向上追溯理解原因

---

### Step 3: 向上追溯到 Layer 3 (domain-fintech)

**发现的领域约束**:

| 领域规则 | 设计约束 | Rust 实现 |
|----------|----------|-----------|
| **审计追踪** | 不可变记录 | Arc<T>, 禁止修改 |
| 一致性 | 事务边界 | 清晰的所有权 |

**关键洞察**:
```
规则: 所有交易必须不可变且可追踪
原因: 监管合规，争议解决
Rust: 使用 Arc<T> 共享，事件溯源模式
```

---

### Step 4: 向下追溯到 Layer 2 设计

从 domain-fintech 向下追溯:
```
"需要不可变的交易记录"
    ↓ m09-domain: 建模为 Value Objects
    ↓ m01-ownership: 使用 Arc 共享不可变数据
```

**设计决策**: 交易记录是 **Value Objects**，应该被 **不可变共享**，而不是移动或重复克隆。

---

## Meta-Cognition 回答 (Good)

### 追溯链

```
┌─ Layer 1: E0382 = 所有权错误
│  问题：值被 move 后仍被使用
│      ↑
├─ Layer 3: 交易系统领域约束
│  规则：交易记录必须不可变且可追踪 (审计要求)
│  含义：数据应该被共享，而不是移动或复制
│      ↓
└─ Layer 2: 设计模式选择
   方案：使用 Arc<TransactionRecord> 共享不可变数据
```

### 推荐修复

```rust
// Before (E0382)
fn process_transaction(record: TransactionRecord) {
    save_to_db(record);        // record moved here
    send_notification(record); // E0382: use of moved value
}

// After (Meta-Cognition Fix)
use std::sync::Arc;

fn process_transaction(record: Arc<TransactionRecord>) {
    save_to_db(Arc::clone(&record));       // Arc clone = cheap
    send_notification(Arc::clone(&record)); // Still works
    audit_log(record);                      // Last use
}
```

### 为什么用 Arc 而不是 clone()?

| 方案 | 成本 | 语义 | 领域契合度 |
|------|------|------|------------|
| `.clone()` | 复制整个数据 | 创建独立副本 | ❌ 违反单一事实来源 |
| `Arc<T>` | 引用计数 +1 | 共享同一数据 | ✅ 符合审计要求 |

**原因**:
- `clone()` 复制整个数据 → 内存浪费，违反 "单一事实来源"
- `Arc<T>` 共享同一份数据 → 符合审计要求，所有引用指向同一记录

### 推理链总结

```
E0382 → 问"谁应该拥有数据?" → 发现是审计记录 →
领域约束要求不可变+可追踪 → 设计为 Arc<T> 共享 →
不是语法修复，而是设计改进
```

---

## 对比验证

| 评判标准 | 传统回答 | Meta-Cognition |
|----------|----------|----------------|
| 修复错误 | ✅ | ✅ |
| 解释原因 | ❌ | ✅ |
| 考虑领域 | ❌ | ✅ |
| 建议设计 | ❌ | ✅ |
| 预防未来问题 | ❌ | ✅ |

---

## 关键学习

### 1. 不要停在 Layer 1

表面错误（E0382）只是症状，真正的问题可能在设计层或领域层。

### 2. 领域约束决定设计

金融领域的审计要求决定了数据必须不可变且可追踪，这直接影响所有权设计。

### 3. Arc vs Clone 的选择

| 场景 | 选择 |
|------|------|
| 数据需要独立演化 | `clone()` |
| 数据是共享的事实 | `Arc<T>` |
| 金融审计记录 | `Arc<T>` (单一事实来源) |

---

## 相关技能

| Skill | 作用 |
|-------|------|
| m01-ownership | Layer 1 入口，所有权机制 |
| m02-resource | Arc/Rc 智能指针选择 |
| m09-domain | Value Object vs Entity 建模 |
| domain-fintech | 金融领域约束 |

---

## 参考

- `_meta/reasoning-framework.md` - 完整追溯框架
- `skills/m01-ownership/SKILL.md` - 所有权技能
- `skills/domain-fintech/SKILL.md` - 金融领域约束
