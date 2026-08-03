# Skills Index

Complete index of all rust-skills with descriptions.

---

## Meta-Question Skills (Layer 1: Language Mechanics)

| ID | Name | Core Question | Key Concepts |
|----|------|---------------|--------------|
| m01 | Ownership & Lifetimes | "Who owns this memory, when is it freed?" | ownership, borrowing, lifetime, move, E0382, E0597 |
| m02 | Resource Management | "How to balance determinism vs flexibility?" | Box, Rc, Arc, Cell, RefCell, smart pointers |
| m03 | Mutability | "Where are the immutability boundaries?" | mut, interior mutability, E0596, E0499, E0502 |
| m04 | Zero-Cost Abstraction | "What can the compiler optimize away?" | generics, trait, inline, monomorphization, E0277, E0308 |
| m05 | Type-Driven Design | "How do types encode constraints?" | type state, phantom data, newtype |
| m06 | Error Handling | "Are failures expected or exceptional?" | Result, Option, panic, ?, anyhow, thiserror |
| m07 | Concurrency | "How to ensure concurrency safety at compile time?" | Send, Sync, async, await, thread, channel |

> **Note:** m08 (Safety Boundaries) has been merged into **unsafe-checker** skill.

## Meta-Question Skills (Layer 2: Design Choices)

| ID | Name | Core Question | Key Concepts |
|----|------|---------------|--------------|
| m09 | Domain Modeling | "How do domain rules become types?" | DDD, domain model, business logic |
| m10 | Performance Optimization | "What are the performance bottlenecks?" | profiling, optimization, benchmark |
| m11 | Ecosystem Integration | "How to integrate with existing systems?" | crate, interop, bindings, FFI |
| m12 | Resource Lifecycle | "What are domain-specific resource patterns?" | RAII, Drop, resource patterns |
| m13 | Domain Error Strategy | "What are domain failure and recovery strategies?" | domain errors, retry, circuit breaker |
| m14 | Mental Models | "What is the correct mental model?" | mental models, learning, how to think |
| m15 | Anti-Patterns | "What are common cognitive pitfalls?" | anti-patterns, common mistakes, pitfalls |

---

## Core Skills

| Name | Description | Key Triggers |
|------|-------------|--------------|
| rust-router | Master router for ALL Rust questions | Rust, cargo, rustc, crate, error codes |
| rust-learner | Rust version and crate information | version, changelog, crate info |
| coding-guidelines | Code style and best practices | style, naming, clippy, formatting |
| unsafe-checker | Unsafe code review and FFI guidance | unsafe, FFI, raw pointer, transmute |

---

## Domain Skills (Layer 3: Domain Constraints)

| Name | Focus Area | Key Concepts |
|------|------------|--------------|
| domain-fintech | Financial Technology | Decimal, trading, currency, audit |
| domain-web | Web Development | HTTP, REST, axum, actix, handler |
| domain-cli | CLI Applications | clap, terminal, command line |
| domain-cloud-native | Cloud-Native | kubernetes, docker, grpc, microservice |
| domain-embedded | Embedded Systems | no_std, microcontroller, firmware |
| domain-ml | Machine Learning | tensor, model, inference, ndarray |
| domain-iot | Internet of Things | sensor, mqtt, embedded, edge |

---

## Utility Skills

| Name | Description |
|------|-------------|
| core-actionbook | Action book for skill management |
| core-agent-browser | Browser-based agent for web fetching |
| core-dynamic-skills | Dynamic skill loading and management |
| core-fix-skill-docs | Fix and update skill documentation |
| rust-daily | Daily Rust news and updates |
| rust-skill-creator | Create new skills |

---

## Skill Count Summary

| Category | Count |
|----------|-------|
| Meta-Question (L1) | 7 |
| Meta-Question (L2) | 7 |
| Core | 4 |
| Domain (L3) | 7 |
| Utility | 6 |
| **Total** | **31** |

---

## Related Documents

| Document | Purpose |
|----------|---------|
| [triggers-index.md](./triggers-index.md) | Keyword-to-skill mapping |
| [meta-questions.md](./meta-questions.md) | Meta-question category definitions |
| [domain-extensions.md](./domain-extensions.md) | Domain-specific code ranges |

### Framework

| File | Purpose |
|------|---------|
| [../_meta/reasoning-framework.md](../_meta/reasoning-framework.md) | Cognitive layer tracing |
| [../_meta/negotiation-protocol.md](../_meta/negotiation-protocol.md) | Agent communication protocol |

### Router

| File | Purpose |
|------|---------|
| [../skills/rust-router/SKILL.md](../skills/rust-router/SKILL.md) | Master routing logic with priority rules |
