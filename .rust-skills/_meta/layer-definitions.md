# Cognitive Layer Definitions

> Defines the three cognitive layers for meta-cognition reasoning.

## Overview

```
┌─────────────────────────────────────────────────────────────┐
│  Layer 3: Domain Constraints (WHY)                          │
│  ├── Business rules, regulatory requirements                │
│  ├── Performance/reliability SLAs                           │
│  └── Domain-specific invariants                             │
├─────────────────────────────────────────────────────────────┤
│  Layer 2: Design Choices (WHAT)                             │
│  ├── Architecture patterns, DDD concepts                    │
│  ├── API design, module structure                           │
│  └── Trade-off decisions                                    │
├─────────────────────────────────────────────────────────────┤
│  Layer 1: Language Mechanics (HOW)                          │
│  ├── Ownership, borrowing, lifetimes                        │
│  ├── Type system, trait bounds                              │
│  └── Compiler errors and fixes                              │
└─────────────────────────────────────────────────────────────┘
```

---

## Layer 1: Language Mechanics (HOW)

### Definition

The implementation layer dealing with Rust's language features, compiler rules, and runtime behavior.

### Scope

| Category | Examples |
|----------|----------|
| Ownership | Move semantics, borrowing, lifetimes |
| Type System | Generics, traits, bounds, associated types |
| Memory | Stack vs heap, smart pointers, RAII |
| Concurrency | Send, Sync, async/await, channels |
| Error Handling | Result, Option, panic, ? operator |
| Unsafe | Raw pointers, FFI, transmute |

### Entry Signals

| Signal | Interpretation |
|--------|---------------|
| E0382 | Ownership issue - value moved |
| E0597 | Lifetime issue - reference outlives owner |
| E0277 | Trait bound not satisfied |
| E0308 | Type mismatch |
| Compile error | Language rule violation |
| Runtime panic | Runtime invariant violation |

### Related Skills

```
m01-ownership    → Ownership, borrowing, lifetimes
m02-resource     → Smart pointers, RAII
m03-mutability   → Interior mutability, Cell/RefCell
m04-zero-cost    → Generics, traits, monomorphization
m05-type-driven  → Type state, newtype, PhantomData
m06-error-handling → Result, Option, error patterns
m07-concurrency  → Async, threads, Send/Sync
```

### Key Questions

- What does the compiler error mean?
- What language rule is being violated?
- What's the idiomatic Rust solution?

---

## Layer 2: Design Choices (WHAT)

### Definition

The architectural layer dealing with design patterns, system structure, and trade-off decisions.

### Scope

| Category | Examples |
|----------|----------|
| Domain Modeling | Entities, Value Objects, Aggregates |
| Architecture | Modules, layers, boundaries |
| API Design | Public interfaces, ergonomics |
| Patterns | Builder, State machine, Repository |
| Trade-offs | Performance vs safety, flexibility vs simplicity |
| Mental Models | How to think about the problem |

### Entry Signals

| Signal | Interpretation |
|--------|---------------|
| "How to design..." | Architecture question |
| "What pattern..." | Design pattern question |
| "Trade-off between..." | Decision question |
| "Best practice for..." | Convention question |
| "Why does Rust..." | Mental model question |

### Related Skills

```
m09-domain       → DDD, domain modeling
m10-performance  → Optimization patterns
m11-ecosystem    → Crate integration
m12-lifecycle    → Resource lifecycle, RAII patterns
m13-domain-error → Domain error handling
m14-mental-model → How to think in Rust
m15-anti-pattern → Common mistakes to avoid
```

### Key Questions

- What's the appropriate pattern for this problem?
- What are the trade-offs of this design?
- How does this fit into the larger architecture?

---

## Layer 3: Domain Constraints (WHY)

### Definition

The context layer dealing with business rules, regulatory requirements, and domain-specific invariants.

### Scope

| Category | Examples |
|----------|----------|
| Business Rules | Validation, workflows, policies |
| Regulatory | Audit, compliance, security |
| Performance | SLAs, latency, throughput |
| Domain Invariants | Consistency rules, constraints |
| Environment | Deployment, infrastructure |
| Users | UX requirements, accessibility |

### Entry Signals

| Signal | Interpretation |
|--------|---------------|
| "Building [domain] app" | Domain context |
| "Business requirement..." | Constraint specification |
| "Must be auditable..." | Regulatory constraint |
| "Users expect..." | UX constraint |
| "Production needs..." | Deployment constraint |

### Related Skills

```
domain-fintech   → Financial domain constraints
domain-web       → Web service constraints
domain-cli       → CLI application constraints
domain-embedded  → Embedded system constraints
domain-iot       → IoT device constraints
domain-ml        → Machine learning constraints
domain-cloud-native → Cloud infrastructure constraints
```

### Key Questions

- What domain rules apply here?
- What constraints can't be violated?
- What trade-offs does the domain allow?

---

## Layer Interactions

### Downward Flow (Design Time)

```
Layer 3: "Financial transactions must be auditable"
    ↓ implies
Layer 2: "Use immutable event sourcing pattern"
    ↓ implements as
Layer 1: "Arc<T> for shared immutable references"
```

### Upward Flow (Debug Time)

```
Layer 1: "E0382: value moved"
    ↑ asks
Layer 2: "Why is ownership structured this way?"
    ↑ asks
Layer 3: "What domain constraint led to this design?"
```

### Bidirectional Flow (Refactor Time)

```
Layer 3 ←→ Layer 2 ←→ Layer 1
   "Are current constraints still valid?"
   "Does the design serve the domain?"
   "Is the implementation optimal?"
```

---

## Layer Mapping Table

| Question Type | Entry | Direction | Skill Type |
|---------------|-------|-----------|------------|
| Compiler error | L1 | UP ↑ | m01-m07 |
| "How to fix..." | L1 | UP ↑ | m01-m07 |
| "What pattern..." | L2 | DOWN ↓ | m09-m15 |
| "Best practice..." | L2 | BOTH ↕ | m09-m15 |
| "Building [domain]..." | L3 | DOWN ↓ | domain-* |
| "Why in Rust..." | L2 | UP ↑ | m14-mental-model |
| Performance issue | L1 | UP ↑ | m10-performance |
| Design review | L2 | BOTH ↕ | m09-m15 |

---

## Extensibility

This framework is designed to be extended:

### For Rust Skills
- Layer 1: Add new m0x skills for new language features
- Layer 2: Add new m1x skills for new patterns
- Layer 3: Add new domain-* skills for new domains

### For Other Languages/Frameworks
- Layer 1: Replace with framework-specific mechanics
- Layer 2: Keep patterns (mostly language-agnostic)
- Layer 3: Keep constraints (fully domain-specific)

### Example: Makepad Extension

```
Layer 3: UI domain constraints (60fps, accessibility)
    ↓
Layer 2: Widget patterns, layout patterns
    ↓
Layer 1: Makepad-specific mechanics + Rust basics
```
