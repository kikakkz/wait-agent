# What Is a Skill?

> A precise definition based on rust-skills architecture

## The Wrong Mental Model

```
Skill ≠ Knowledge Database
Skill ≠ Documentation
Skill ≠ FAQ Collection
Skill ≠ Code Snippets Library
```

Claude already knows Rust. Teaching it "ownership rules" or "how Arc works" adds no value.

---

## The Right Mental Model

### Definition

> **A Skill is a Cognitive Protocol that shapes HOW Claude thinks about a problem, not WHAT it knows.**

### Components

```
Skill = Routing Logic
      + Reasoning Template
      + Constraint Set
      + Attention Director
      + Decision Framework
```

---

## Anatomy of a Skill (rust-skills)

### 1. Routing Logic

**What it does**: Classifies the problem and determines which thinking mode to activate.

```
rust-router:
  "E0382" → Layer 1 entry → m01-ownership
  "Web API" → Layer 3 context → domain-web
  "How to design" → Layer 2 question → m09-domain
```

**Not**: "Here's what E0382 means"
**But**: "This is a Layer 1 problem, trace UP to find why"

### 2. Reasoning Template

**What it does**: Provides a structured thinking process, not answers.

```markdown
## Core Question
**Who should own this data?**  ← Forces Claude to ASK, not TELL

## Error → Design Question
| Error | Don't Just Say | Ask Instead |
| E0382 | "Clone it" | "Who should own this?" |
```

**Not**: "E0382 means value was moved, use clone"
**But**: "Before fixing, ask: Is this ownership design intentional?"

### 3. Constraint Set

**What it does**: Defines boundaries that shape valid solutions.

```markdown
## Domain Constraints (domain-web)
| Rule | Constraint | Implication |
| Stateless HTTP | No request globals | State via extractors |
| Concurrency | Many connections | Must be Send + Sync |
```

**Not**: "Web apps use async"
**But**: "Web domain REQUIRES thread-safe state, this constrains your options"

### 4. Attention Director

**What it does**: Points Claude's attention to what matters for THIS context.

```markdown
## Trace Up ↑
When you see Send/Sync error in Web context:
  → Load domain-web
  → Find: "Handlers run on any thread"
  → This constraint explains WHY Arc is needed
```

**Not**: "Arc is thread-safe"
**But**: "In THIS context, look at domain-web constraints FIRST"

### 5. Decision Framework

**What it does**: Provides decision trees, not conclusions.

```markdown
## Decision Flowchart
Need shared data?
├─ Yes → Multi-thread?
│        ├─ Yes → Arc<T>
│        └─ No → Rc<T>
└─ No → Owned value
```

**Not**: "Use Arc for shared data"
**But**: "Here's how to DECIDE what to use"

---

## Skill Types in rust-skills

### Type 1: Mechanism Skills (Layer 1)

**Purpose**: Provide thinking frameworks for language mechanics.

```
m01-ownership: "Who should own this?"
m02-resource:  "What ownership model?"
m07-concurrency: "CPU-bound or I/O-bound?"
```

These are not "ownership tutorials" but **ownership reasoning protocols**.

### Type 2: Design Skills (Layer 2)

**Purpose**: Provide design decision frameworks.

```
m09-domain: "How do domain rules become types?"
m10-performance: "Where are the bottlenecks?"
m15-anti-pattern: "What cognitive traps to avoid?"
```

These are not "design patterns catalog" but **design thinking protocols**.

### Type 3: Domain Skills (Layer 3)

**Purpose**: Define domain-specific constraints that shape all lower decisions.

```
domain-web: "Handlers on any thread" → Forces Arc, not Rc
domain-fintech: "Audit trail required" → Forces immutability
domain-embedded: "no_std constraint" → Limits available patterns
```

These are not "domain knowledge" but **domain constraint systems**.

### Type 4: Router Skills

**Purpose**: Meta-level routing that orchestrates other skills.

```
rust-router:
  1. Identify entry layer
  2. Detect domain context
  3. Load appropriate skills
  4. Enforce tracing direction
```

This is a **cognitive traffic controller**.

---

## What Skills Actually Do

### Before (No Skills)

```
User: "Web API reports Rc cannot be sent"

Claude's thinking:
  → I know Rc is !Send
  → I know Arc is Send
  → Answer: "Use Arc"
```

### After (With Skills)

```
User: "Web API reports Rc cannot be sent"

Skill-guided thinking:
  → rust-router: Detect "Web API" → Load domain-web
  → rust-router: Detect "Send" error → Load m07-concurrency
  → m07-concurrency: "Don't just fix, trace UP"
  → domain-web: "Handlers run on any thread" (constraint)
  → domain-web: "Rc in state" is Common Mistake (validation)
  → m07-concurrency: "Multi-thread + shared → Arc" (decision tree)
  → Answer: Arc + State extractor (domain best practice)
```

---

## The Skill Contract

Every skill in rust-skills follows this contract:

```markdown
# Skill Name

> Layer X: Category

## Core Question
[The meta-question that reframes the problem]

## Error → Design Question
[Transforms surface symptoms into deeper questions]

## Trace Up ↑
[When and how to escalate to higher layers]

## Trace Down ↓
[How to implement from design decisions]

## Decision Framework
[Trees/tables for making choices, not prescriptions]

## Anti-Patterns
[What NOT to do, and why]
```

---

## Formal Definition

### Skill (n.)

> A **Cognitive Protocol** consisting of:
>
> 1. **Classification Rules** - How to categorize this problem
> 2. **Reasoning Templates** - What questions to ask
> 3. **Constraint Definitions** - What boundaries apply
> 4. **Attention Directives** - Where to look for context
> 5. **Decision Frameworks** - How to choose between options
>
> That **shapes the reasoning process** rather than providing pre-computed answers.

### In Code Terms

```rust
struct Skill {
    /// Routes problem to appropriate thinking mode
    routing: fn(Problem) -> Layer,

    /// Questions to ask, not answers to give
    core_question: MetaQuestion,

    /// Boundaries that constrain valid solutions
    constraints: Vec<Constraint>,

    /// What to pay attention to in this context
    attention: Vec<AttentionDirective>,

    /// Decision trees for making choices
    decisions: Vec<DecisionFramework>,

    /// Links to related skills for tracing
    trace_up: Vec<SkillRef>,
    trace_down: Vec<SkillRef>,
}

impl Skill {
    /// Skills don't answer, they guide reasoning
    fn apply(&self, problem: Problem) -> ReasoningProcess {
        // NOT: return Answer
        // BUT: return HowToThinkAboutThis
    }
}
```

---

## Why This Matters

### Knowledge-Based Approach (Limited)

```
Input: E0382
Output: "Use clone()"
Result: Compiles, but may be wrong design
```

### Skill-Based Approach (Powerful)

```
Input: E0382 + "Web API" context
Process:
  1. Route to m01-ownership + domain-web
  2. Ask "Who should own this?"
  3. Check domain constraint "thread-safe state"
  4. Decide via framework "shared + multi-thread → Arc"
  5. Apply domain pattern "State<Arc<T>>"
Output: Architecturally correct solution
Result: Right design for this domain
```

---

## Summary

| Aspect | Knowledge Base | Skill |
|--------|---------------|-------|
| Contains | Facts, answers | Protocols, frameworks |
| Provides | What to do | How to think |
| Output | Solutions | Reasoning processes |
| Adapts to | Nothing | Context, domain |
| Value-add | Recall | Judgment |

### One-Line Definition

> **A Skill is a reusable reasoning protocol that transforms how Claude thinks about a class of problems, not what it knows about them.**

---

## rust-skills Architecture Summary

```
┌─────────────────────────────────────────────────┐
│                  rust-router                     │
│         (Cognitive Traffic Controller)           │
└─────────────────┬───────────────────────────────┘
                  │
        ┌─────────┼─────────┐
        │         │         │
        ▼         ▼         ▼
   ┌─────────┬─────────┬─────────┐
   │ Layer 1 │ Layer 2 │ Layer 3 │
   │Mechanism│ Design  │ Domain  │
   │ Skills  │ Skills  │ Skills  │
   └────┬────┴────┬────┴────┬────┘
        │         │         │
        │    Reasoning      │
        │    Templates      │
        │         │         │
        └─────────┼─────────┘
                  │
                  ▼
        ┌─────────────────┐
        │ Context-Aware   │
        │ Reasoning       │
        │ Process         │
        └─────────────────┘
```

**rust-skills is not a Rust knowledge base.**
**rust-skills is a Rust reasoning system.**
