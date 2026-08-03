# Meta-Cognition Reasoning Framework

> Universal framework for tracing problems through cognitive layers.
> Borrowed from `planning-with-files` principles.

## Core Principle

**Don't answer directly. Trace through the cognitive layers first.**

When encountering a problem, the goal is not to provide an immediate fix, but to understand:
1. What layer the problem originates from
2. What constraints or decisions led to this state
3. What the contextually-appropriate solution is

## The Three Layers

```
Layer 3: Domain Constraints (WHY - Why is it designed this way?)
├── Domain rules dictate design choices
├── Example: Financial systems require immutable, auditable data
└── → This determines the ownership model

Layer 2: Design Choices (WHAT - What design to adopt?)
├── Design patterns and architectural decisions
├── Example: Use Value Objects, Arc sharing
└── → This triggers specific language mechanisms

Layer 1: Language Mechanics (HOW - How to implement?)
├── Rust language features and compiler rules
├── Example: E0382 indicates ownership design issue
└── → Surface error, needs to trace upward
```

## Reasoning Steps

### Step 1: Identify Entry Point

| Signal | Entry Layer | Direction |
|--------|-------------|-----------|
| Error code (E0xxx), compile error | Layer 1 | Trace UP ↑ |
| "How to design...", pattern question | Layer 2 | Check Layer 3, then DOWN ↓ |
| "Building a [domain] system" | Layer 3 | Trace DOWN ↓ |
| "Why does Rust..." | Layer 2 | Bidirectional |

### Step 2: Trace the Chain

```
Layer 1 (Mechanics) ←→ Layer 2 (Design) ←→ Layer 3 (Domain)
```

At each layer, ask:
- **Layer 1**: What mechanism is involved? What does the compiler tell us?
- **Layer 2**: What design choice triggered this? Is this the right pattern?
- **Layer 3**: What domain constraint requires this design? Is the constraint valid?

### Step 3: Answer with Context

Include the reasoning chain in your answer. Not just WHAT to do, but WHY this is the right choice for this domain.

## Tracing Examples

### Example 1: E0382 in Trading System

```
User: "My trading system reports E0382, data was moved"

Traditional Answer: "Use .clone()"

Meta-Cognition Answer:
┌─ Layer 1: E0382 = ownership issue → Why do we need this ownership design?
│      ↑
├─ Layer 3: Trading records are immutable audit data → Should be shared, not copied
│      ↓
└─ Layer 2: Use Arc<TradeRecord> as shared immutable value
       ↓
Suggestion: Not clone, but redesign as Arc<T>
```

### Example 2: Designing User Auth

```
User: "How should I design user authentication?"

Analysis:
┌─ Layer 3: Security domain constraints
│  ├── Tokens must expire
│  ├── Passwords must be hashed
│  └── Sessions need secure storage
│      ↓
├─ Layer 2: Design patterns
│  ├── JWT for stateless auth
│  ├── Session store for stateful
│  └── Password hash with argon2
│      ↓
└─ Layer 1: Rust implementation
   ├── Use jsonwebtoken crate
   └── Store in Arc<RwLock<HashMap>> or Redis
```

### Example 3: Performance Issue

```
User: "My API is slow when processing large lists"

Analysis:
┌─ Layer 1: Possible causes
│  ├── Cloning large data?
│  ├── Blocking async?
│  └── N+1 queries?
│      ↑
├─ Layer 2: Design review
│  ├── Is data ownership correct?
│  ├── Is async used properly?
│  └── Is query pattern optimal?
│      ↑
└─ Layer 3: Domain constraints
   ├── How large is "large"?
   ├── What latency is acceptable?
   └── Can data be paginated/streamed?
```

## Trace Direction Rules

### Trace UP ↑ (Layer 1 → 3)

Use when:
- Compiler errors (E0xxx)
- Runtime panics
- Type mismatches
- Unexpected behavior

Question to ask: "What design decision led to this constraint?"

### Trace DOWN ↓ (Layer 3 → 1)

Use when:
- New feature design
- Architecture planning
- "How should I..." questions
- Domain modeling

Question to ask: "Given this constraint, what's the appropriate pattern?"

### Bidirectional ←→

Use when:
- Refactoring existing code
- Performance optimization
- "Why does Rust..." questions
- Trade-off analysis

## Integration with Skills

| Layer | Related Skills | Purpose |
|-------|---------------|---------|
| Layer 1 | m01-m07 | Language mechanics, compiler behavior |
| Layer 2 | m09-m15 | Design patterns, mental models |
| Layer 3 | domain-* | Domain-specific constraints |

### Skill Invocation Flow

```
Entry Point Detected
       ↓
[1] Identify Layer → Read corresponding skill
       ↓
[2] Trace to related layers → Cross-reference skills
       ↓
[3] Synthesize answer with full context
```

## 5-Question Reboot Test

When stuck or confused in complex problem-solving, answer these 5 questions:

| # | Question | Find Answer In |
|---|----------|----------------|
| 1 | What error am I solving? | Entry point / original question |
| 2 | What layer am I in? | Current trace position |
| 3 | What domain constraints apply? | Layer 3 / domain-* skills |
| 4 | What have I tried? | Previous attempts |
| 5 | What's the next trace direction? | This framework |

## Anti-Patterns

### Don't: Answer at Surface Level

```
User: "E0382 error"
Bad: "Use .clone()"
```

### Don't: Skip Layers

```
User: "Design a payment system"
Bad: "Use rust_decimal" (jumped to Layer 1)
```

### Don't: Ignore Domain Context

```
User: "How to share data between threads?"
Bad: "Use Arc<Mutex<T>>" (didn't ask about domain constraints)
```

## Summary

1. **Identify entry layer** from user's question type
2. **Trace through layers** to understand full context
3. **Answer with reasoning chain** showing WHY not just WHAT
4. **Reference appropriate skills** for each layer
5. **Use 5-Question Reboot** when stuck

---

## Related Documents

| Document | Purpose |
|----------|---------|
| [layer-definitions.md](./layer-definitions.md) | Detailed layer definitions and boundaries |
| [negotiation-protocol.md](./negotiation-protocol.md) | Agent communication protocol |
| [error-protocol.md](./error-protocol.md) | 3-Strike escalation rules |
| [externalization.md](./externalization.md) | Cognitive externalization patterns |
| [hooks-patterns.md](./hooks-patterns.md) | Automatic trigger mechanisms |

### Index Files

| File | Purpose |
|------|---------|
| [../index/skills-index.md](../index/skills-index.md) | Complete skill catalog |
| [../index/triggers-index.md](../index/triggers-index.md) | Keyword-to-skill mapping |
| [../index/meta-questions.md](../index/meta-questions.md) | Meta-question category definitions |

### Router

| File | Purpose |
|------|---------|
| [../skills/rust-router/SKILL.md](../skills/rust-router/SKILL.md) | Master routing logic |
