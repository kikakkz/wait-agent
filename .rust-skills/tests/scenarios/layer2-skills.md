# Layer 2 Skills Test Scenarios

> Layer 2: Design Choices

## m05-type-driven

| Query | Expected Skill | Expected Elements |
|-------|----------------|-------------------|
| `newtype pattern` | m05-type-driven | wrapper, type safety |
| `PhantomData 用法` | m05-type-driven | marker, lifetime |
| `type state pattern` | m05-type-driven | state machine, compile-time |
| `零大小类型 ZST` | m05-type-driven | zero-sized, marker |

### Test Commands

```bash
claude -p "newtype pattern"
claude -p "PhantomData 用法"
```

---

## m09-domain

| Query | Expected Skill | Expected Elements |
|-------|----------------|-------------------|
| `DDD in Rust` | m09-domain | aggregate, entity |
| `domain model 设计` | m09-domain | value object, repository |
| `领域建模` | m09-domain | bounded context |
| `aggregate root` | m09-domain | invariant, consistency |

### Test Commands

```bash
claude -p "DDD in Rust"
claude -p "领域建模"
```

---

## m10-performance

| Query | Expected Skill | Expected Elements |
|-------|----------------|-------------------|
| `Rust 性能优化` | m10-performance | profiling, bottleneck |
| `benchmark 怎么写` | m10-performance | criterion, bench |
| `criterion 用法` | m10-performance | black_box, throughput |
| `零拷贝 zero copy` | m10-performance | Cow, bytes |

### Test Commands

```bash
claude -p "Rust 性能优化"
claude -p "benchmark 怎么写"
```

---

## m11-ecosystem

| Query | Expected Skill | Expected Elements |
|-------|----------------|-------------------|
| `推荐什么 crate` | m11-ecosystem | crates.io, popularity |
| `依赖选择` | m11-ecosystem | maintenance, features |
| `Cargo.toml 依赖管理` | m11-ecosystem | version, workspace |
| `feature flags 用法` | m11-ecosystem | optional, cfg |

### Test Commands

```bash
claude -p "推荐什么 crate"
claude -p "feature flags 用法"
```

---

## m12-lifecycle

| Query | Expected Skill | Expected Elements |
|-------|----------------|-------------------|
| `RAII pattern` | m12-lifecycle | Drop, scope |
| `Drop trait 实现` | m12-lifecycle | destructor, cleanup |
| `资源释放顺序` | m12-lifecycle | drop order, field |
| `scopeguard 用法` | m12-lifecycle | defer, guard |

### Test Commands

```bash
claude -p "RAII pattern"
claude -p "Drop trait 实现"
```

---

## m13-domain-error

| Query | Expected Skill | Expected Elements |
|-------|----------------|-------------------|
| `retry 策略` | m13-domain-error | backoff, exponential |
| `circuit breaker 实现` | m13-domain-error | state, threshold |
| `错误恢复模式` | m13-domain-error | fallback, graceful |
| `错误分类处理` | m13-domain-error | transient, permanent |

### Test Commands

```bash
claude -p "retry 策略"
claude -p "circuit breaker 实现"
```

---

## m14-mental-model

| Query | Expected Skill | Expected Elements |
|-------|----------------|-------------------|
| `怎么学 Rust` | m14-mental-model | ownership, mindset |
| `Rust 思维方式` | m14-mental-model | borrow checker, mental model |
| `从 Java 转 Rust` | m14-mental-model | comparison, transition |
| `为什么 Rust 这样设计` | m14-mental-model | rationale, philosophy |

### Test Commands

```bash
claude -p "怎么学 Rust"
claude -p "Rust 思维方式"
```

---

## m15-anti-pattern

| Query | Expected Skill | Expected Elements |
|-------|----------------|-------------------|
| `常见 Rust 错误` | m15-anti-pattern | pitfall, mistake |
| `code smell Rust` | m15-anti-pattern | refactor, improve |
| `Rust 反模式` | m15-anti-pattern | avoid, better |
| `clone 滥用` | m15-anti-pattern | unnecessary, performance |

### Test Commands

```bash
claude -p "常见 Rust 错误"
claude -p "clone 滥用"
```

---

## Validation Checklist

- [ ] All Layer 2 skills trigger correctly
- [ ] Design-related queries route properly
- [ ] Chinese keywords work
- [ ] No conflicts with Layer 1 skills
