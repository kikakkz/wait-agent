# Externalized Cognition Principles

> Borrowed from `planning-with-files`: Use the filesystem as external memory.

## Core Insight

```
Context Window = RAM (volatile, limited)
Filesystem = Disk (persistent, unlimited)

→ Important cognitive processes should be externalized to files
```

## The 3-File Pattern

For complex problems, create a reasoning directory:

```
_reasoning/
├── trace.md      # Layer tracing record
├── findings.md   # Discovered constraints and patterns
└── decision.md   # Final decision and rationale
```

### When to Use 3-File Pattern

| Scenario | Use Pattern? |
|----------|--------------|
| Simple E0xxx fix | No, inline reasoning |
| Multi-file refactor | Yes |
| Architecture decision | Yes |
| Debugging complex issue | Yes |
| Performance optimization | Yes |
| Design review | Yes |

---

## File Templates

### trace.md

```markdown
# Reasoning Trace

## Entry Point
- Signal: [error code / question type]
- Layer: [1/2/3]

## Trace UP ↑ (if applicable)
### Layer 1 → Layer 2
- Question: What design choice led to this?
- Finding: [discovered pattern]

### Layer 2 → Layer 3
- Question: What domain constraint requires this?
- Finding: [discovered constraint]

## Trace DOWN ↓ (if applicable)
### Layer 3 → Layer 2
- Constraint: [domain rule]
- Implication: [design choice]

### Layer 2 → Layer 1
- Pattern: [chosen design]
- Implementation: [Rust approach]

## Attempts
1. [First attempt] - [result]
2. [Second attempt] - [result]

## Current Status
- [ ] Problem understood
- [ ] Root cause identified
- [ ] Solution designed
- [ ] Solution implemented
- [ ] Solution verified
```

### findings.md

```markdown
# Findings

## Domain Constraints (Layer 3)
- [constraint 1]
- [constraint 2]

## Design Patterns (Layer 2)
- [pattern 1]: [why appropriate]
- [pattern 2]: [why not appropriate]

## Implementation Details (Layer 1)
- [mechanism 1]: [how it applies]
- [mechanism 2]: [how it applies]

## Cross-References
- Skill: [skill name] - [relevant section]
- Docs: [link] - [relevant info]

## Trade-offs Identified
| Option | Pros | Cons |
|--------|------|------|
| A | | |
| B | | |
```

### decision.md

```markdown
# Decision Record

## Context
[Brief description of the problem]

## Decision
[What was decided]

## Rationale
### Layer 3 (Domain)
[Why this fits domain constraints]

### Layer 2 (Design)
[Why this pattern was chosen]

### Layer 1 (Implementation)
[How this is implemented in Rust]

## Consequences
- Positive: [benefits]
- Negative: [trade-offs accepted]

## Alternatives Rejected
- [Alternative 1]: [why rejected]
- [Alternative 2]: [why rejected]
```

---

## 2-Action Rule

> After every 2 key operations, update external files.

### Why?

Prevents information loss when:
- Context resets
- Multi-modal information (code, errors, docs) accumulates
- Reasoning chain becomes long

### Key Operations

| Category | Operations |
|----------|-----------|
| Build | cargo build, cargo run, cargo test |
| Read | Reading docs, source code, error messages |
| Write | Creating/modifying code |
| Debug | Adding logs, inspecting state |

### Implementation

```
Operation 1: cargo build → error
Operation 2: read error, identify issue
→ UPDATE: Add to trace.md

Operation 3: attempt fix
Operation 4: cargo build → different error
→ UPDATE: Add to trace.md, update findings.md

Operation 5: read docs for pattern
Operation 6: implement pattern
→ UPDATE: Add to findings.md, update trace.md
```

---

## Attention Manipulation

### Pre-Action Reading

Before key decisions, re-read relevant files:

```
Before writing code:
1. Re-read trace.md (current understanding)
2. Re-read findings.md (constraints identified)
3. Re-read relevant skill (patterns to apply)
```

### Post-Action Recording

After key actions, update files:

```
After running code:
1. Record result in trace.md
2. Update findings.md if new info learned
3. Update decision.md if decision made
```

### Context Refresh Points

| Trigger | Action |
|---------|--------|
| Before starting new task | Read previous state |
| After 50+ tool calls | Re-read goal and progress |
| After any error | Update trace with error |
| Before committing | Review decision record |

---

## Error Persistence

### Principle

> Errors should not be hidden. Keep them in context for learning.

### Implementation

```markdown
## Error Log (in trace.md)

### Error 1
- Time: [when]
- Error: [full error message]
- Analysis: [what went wrong]
- Fix: [how resolved]

### Error 2
- Time: [when]
- Error: [full error message]
- Analysis: [what went wrong]
- Fix: [pending/how resolved]
```

### Benefits

1. Prevents repeating same mistakes
2. Builds pattern recognition
3. Documents learning process
4. Helps debugging if issue recurs

---

## Working Memory Management

### Problem

LLM context window is limited. Long conversations lose early information.

### Solution

Use external files as "saved registers":

| File | Contains | Refresh When |
|------|----------|--------------|
| trace.md | Current reasoning state | Every 2 operations |
| findings.md | Accumulated knowledge | When new info learned |
| decision.md | Final decisions | When decision made |

### Retrieval Strategy

When context feels "lost":
1. Read trace.md to restore state
2. Read findings.md to recall constraints
3. Read decision.md to remember choices
4. Continue from documented state

---

## Integration with Skills

### Skill Reading Triggers

| Situation | Read Skill |
|-----------|-----------|
| Layer 1 error | m01-m07 skill for that error |
| Design question | m09-m15 skill for that pattern |
| Domain context | domain-* skill for that domain |
| Before writing fix | Related skill(s) |

### Skill → File Flow

```
Read Skill
    ↓
Extract relevant patterns/rules
    ↓
Record in findings.md
    ↓
Apply to trace.md reasoning
    ↓
Document in decision.md
```

---

## Summary Checklist

Before complex problem-solving:
- [ ] Create _reasoning/ directory
- [ ] Initialize trace.md with entry point
- [ ] Identify relevant skills to read

During problem-solving:
- [ ] Update trace.md every 2 operations
- [ ] Record findings in findings.md
- [ ] Keep errors in trace.md (don't hide)
- [ ] Re-read files at refresh points

After problem-solving:
- [ ] Complete decision.md with rationale
- [ ] Review trace.md for learning
- [ ] Archive or clean up _reasoning/
