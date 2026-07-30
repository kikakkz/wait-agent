# Meta-Question Categories Index

Based on Meta-Question Oriented Numbering System v2.1

## Format

```
m[XX][YYY][ZZZZZ]
```
- XX: Meta-question category (01-15)
- YYY: Technical subcategory (001-999)
- ZZZZZ: Sequence number

---

## Core Language Meta-Questions (01-07)

| Code | Meta-Question | Core Thinking | Key Concepts |
|------|---------------|---------------|--------------|
| **01** | Memory Ownership & Lifetimes | "Who owns this memory, when is it freed?" | ownership, borrowing, lifetime |
| **02** | Resource Management Balance | "How to balance determinism vs flexibility?" | Box, Rc, Arc, Cell, RefCell |
| **03** | Mutability Boundaries | "Where are the immutability boundaries?" | mut, interior mutability |
| **04** | Zero-Cost Abstractions | "What can the compiler optimize away?" | generics, trait, inline |
| **05** | Type-Driven Design | "How do types encode constraints?" | type state, phantom data |
| **06** | Error Handling Philosophy | "Are failures expected or exceptional?" | Result, panic, recovery |
| **07** | Concurrency Correctness | "How to ensure concurrency safety at compile time?" | Send, Sync, thread safety |

> **Note:** m08 (Safety Boundaries) has been merged into **unsafe-checker** skill.

## Domain Architecture Meta-Questions (09-13)

| Code | Meta-Question | Core Thinking | Application Domain |
|------|---------------|---------------|-------------------|
| **09** | Domain Constraint Mapping | "How do domain rules become types?" | domain modeling |
| **10** | Performance Optimization Model | "What are the performance bottlenecks in this domain?" | profiling, optimization |
| **11** | Ecosystem Integration | "How to integrate with existing systems?" | interop, bindings |
| **12** | Domain Lifecycle | "What are domain-specific resource patterns?" | resource patterns |
| **13** | Domain Error Patterns | "What are domain failure and recovery strategies?" | domain errors |

## Cognitive Learning Meta-Questions (14-15)

| Code | Meta-Question | Core Thinking | Learning Dimension |
|------|---------------|---------------|-------------------|
| **14** | Mental Model Construction | "What is the correct mental model?" | mental models |
| **15** | Error Pattern Recognition | "What are common cognitive pitfalls?" | anti-patterns |

---

## Quick Reference

### By Problem Type

**Compiler Errors**
- E0382 (moved value) → m01
- E0597 (lifetime) → m01
- E0277 (Send/Sync) → m07
- E0596 (mutability) → m03

**Design Questions**
- "Which smart pointer?" → m02
- "Generics vs trait objects?" → m04
- "Error handling strategy?" → m06
- "Thread safety?" → m07
- "FFI design?" → unsafe-checker

**Learning**
- "How to think about X?" → m14
- "Common mistakes?" → m15

### By Domain

- Web Development → m06, m07, m11
- Systems Programming → m01, m07, unsafe-checker
- Embedded → m01, unsafe-checker, m10
- Data Processing → m04, m10, m11

---

## Related Documents

| Document | Purpose |
|----------|---------|
| [skills-index.md](./skills-index.md) | Complete skill catalog with descriptions |
| [triggers-index.md](./triggers-index.md) | Keyword-to-skill mapping |
| [domain-extensions.md](./domain-extensions.md) | Domain-specific code ranges (F*, M*, CN*, IoT*) |

### Framework

| File | Purpose |
|------|---------|
| [../_meta/reasoning-framework.md](../_meta/reasoning-framework.md) | Cognitive layer tracing methodology |
| [../_meta/layer-definitions.md](../_meta/layer-definitions.md) | Detailed layer boundaries |

### Router

| File | Purpose |
|------|---------|
| [../skills/rust-router/SKILL.md](../skills/rust-router/SKILL.md) | Uses meta-questions for routing decisions |
