# Error Protocol (3-Strike Rule)

> Systematic approach to handling errors with escalation.

## Core Principle

**If the same approach fails 3 times, escalate to the next level.**

Errors are not just problems to fix—they are signals about design appropriateness.

---

## The 3-Strike Rule

```
Strike 1: Fix at current layer
Strike 2: Question the approach, try alternative
Strike 3: Escalate to next layer up
```

### Why 3 Strikes?

| Strike | Purpose |
|--------|---------|
| 1 | Try obvious fix (maybe simple mistake) |
| 2 | Try alternative approach (maybe wrong method) |
| 3 | Question the design (maybe wrong approach entirely) |

---

## Strike Tracking

### In trace.md

```markdown
## Error Log

### E0382: use of moved value
- Strike 1: Added .clone() → Still fails (different location)
- Strike 2: Changed to &T borrow → Lifetime error E0597
- Strike 3: → ESCALATE: Question ownership design

### Escalation to Layer 2
- Question: Why is data being moved multiple times?
- Finding: Data needs to be shared, not copied
- New approach: Use Arc<T> for shared ownership
```

### Tracking Format

```markdown
## Current Error: [error code/description]

### Strike 1
- Attempt: [what was tried]
- Result: [pass/fail]
- If fail: [why]

### Strike 2
- Attempt: [different approach]
- Result: [pass/fail]
- If fail: [why]

### Strike 3
- Attempt: [another approach]
- Result: [pass/fail]
- If fail: → ESCALATE

### Escalation
- From Layer: [1/2/3]
- To Layer: [2/3]
- Question to answer: [what needs reconsideration]
```

---

## Escalation Paths

### Layer 1 → Layer 2

**Trigger:** Language mechanic errors persist after 3 attempts.

**Question to ask:** "Is the design pattern appropriate for this use case?"

**Examples:**

| Persistent Error | Escalation Question |
|-----------------|---------------------|
| E0382 repeated | Should ownership be shared instead of moved? |
| E0597 repeated | Is the scope boundary in the right place? |
| E0277 (Send) repeated | Is async the right choice for this task? |

### Layer 2 → Layer 3

**Trigger:** Design pattern doesn't fit after 3 attempts.

**Question to ask:** "Is the domain constraint correctly understood?"

**Examples:**

| Persistent Issue | Escalation Question |
|-----------------|---------------------|
| Can't model data correctly | What are the actual domain invariants? |
| Performance always poor | What are the actual performance requirements? |
| Error handling unclear | What are the actual failure modes in this domain? |

---

## Error Classification

### Compile-Time Errors

| Error Type | Typical Strike 1 | Escalate After |
|-----------|------------------|----------------|
| E0382 (moved value) | Clone, borrow | Ownership redesign |
| E0597 (lifetime) | Extend scope | Scope redesign |
| E0277 (trait bound) | Add bound | Type redesign |
| E0308 (type mismatch) | Cast, convert | Interface redesign |

### Runtime Errors

| Error Type | Typical Strike 1 | Escalate After |
|-----------|------------------|----------------|
| Panic (unwrap) | Handle None/Err | Error strategy redesign |
| Index out of bounds | Check bounds | Data structure redesign |
| Deadlock | Reorder locks | Concurrency redesign |

### Logic Errors

| Error Type | Typical Strike 1 | Escalate After |
|-----------|------------------|----------------|
| Wrong result | Fix algorithm | Requirements clarification |
| Missing case | Add case | Domain model redesign |
| Race condition | Add sync | Architecture redesign |

---

## Escalation Protocol

### Step 1: Document Current State

Before escalating, ensure trace.md has:
- [ ] All 3 attempts documented
- [ ] Why each attempt failed
- [ ] What layer we're currently in
- [ ] What skill was used

### Step 2: Ask Escalation Question

| From | To | Question Template |
|------|-----|-------------------|
| L1 | L2 | "What design choice led to this constraint being violated?" |
| L2 | L3 | "What domain rule makes this design necessary?" |

### Step 3: Load Appropriate Skill

| Escalating To | Load |
|---------------|------|
| Layer 2 | m09-m15 (design pattern skills) |
| Layer 3 | domain-* (domain constraint skills) |

### Step 4: Re-Trace Downward

After understanding higher layer:
1. Record new understanding in findings.md
2. Trace back down with new insight
3. Implement with new approach
4. Reset strike counter

---

## Special Cases

### Early Escalation

Sometimes escalate before 3 strikes:

| Signal | Action |
|--------|--------|
| Error explicitly mentions design issue | Escalate immediately |
| User says "I've tried everything" | Start at Layer 2 |
| Problem is architectural | Start at Layer 3 |

### Cross-Layer Errors

Some errors span multiple layers:

| Error | Layers Involved | Approach |
|-------|----------------|----------|
| "Trait not satisfied for async context" | L1 (trait) + L2 (async design) | Address both |
| "Can't share data between threads" | L1 (Send) + L3 (concurrency requirement) | Start at L3 |

### Non-Escalating Errors

Some errors don't escalate—they're just bugs:

| Error Type | Fix | Don't Escalate If |
|-----------|-----|-------------------|
| Typo | Fix spelling | One-time mistake |
| Missing import | Add import | Just forgot |
| Syntax error | Fix syntax | Just typo |

---

## Integration with 5-Question Reboot

When escalation happens, use 5-Question Reboot:

1. **What error am I solving?** → Original error + escalation context
2. **What layer am I in?** → Escalated-to layer
3. **What domain constraints apply?** → Re-read domain-* skill
4. **What have I tried?** → All 3+ strikes
5. **What's the next trace direction?** → Down from new layer

---

## Examples

### Example 1: E0382 Escalation

```markdown
## Error: E0382 (use of moved value)

### Strike 1
- Attempt: Added .clone() at line 42
- Result: Fail - Now E0382 at line 67 (different move)
- Why: Data is moved in multiple places

### Strike 2
- Attempt: Changed function to take &Data instead of Data
- Result: Fail - E0597 (borrowed value doesn't live long enough)
- Why: Caller's data goes out of scope

### Strike 3
- Attempt: Used Cell<Data> for interior mutability
- Result: Fail - Data doesn't implement Copy
- Why: Wrong tool for this job

### Escalation to Layer 2
- Question: Why is this data being moved/borrowed so much?
- Load: m09-domain skill
- Finding: This data is shared state across multiple components
- New approach: Use Arc<Data> for shared ownership

### Resolution
- Wrapped Data in Arc
- Clone Arc (cheap) instead of Data (expensive)
- All components share reference
- E0382 resolved
```

### Example 2: Performance Escalation

```markdown
## Error: Slow response (>1s latency)

### Strike 1
- Attempt: Added caching
- Result: Still >500ms
- Why: Cache miss rate is high

### Strike 2
- Attempt: Optimized hot path with Vec instead of HashMap
- Result: Still >300ms
- Why: Most time in database query

### Strike 3
- Attempt: Added database connection pooling
- Result: Still >200ms
- Why: Query itself is N+1

### Escalation to Layer 3
- Question: What are the actual latency requirements?
- Load: domain-web skill
- Finding: SLA is actually 500ms, current is acceptable
- But also: The N+1 query is a design smell

### Resolution
- Current performance meets SLA (no immediate fix needed)
- Created tech debt item: Refactor to batch query
- Documented in decision.md
```

---

## Summary

1. **Track strikes** in trace.md
2. **Escalate after 3** failed attempts at same layer
3. **Ask the right question** for the escalation
4. **Load appropriate skill** for new layer
5. **Re-trace downward** with new understanding
6. **Reset counter** after successful escalation

---

## Negotiation 3-Strike Rule

The 3-Strike Rule extends to agent negotiation responses.

### Confidence-Based Strikes

| Agent Response | Strike Action |
|----------------|---------------|
| HIGH confidence, covers intent | No strike - accept |
| MEDIUM confidence, covers intent | No strike - accept with gaps |
| MEDIUM confidence, partial intent | Strike 1 - refine query |
| LOW confidence | Strike 1 - refine query |
| UNCERTAIN | Strike 1 - try alternative |

### Negotiation Strike Tracking

```markdown
## Negotiation Log: [Query]

### Round 1 (Strike 1)
- Agent: crate-researcher
- Confidence: LOW
- Gaps: [list]
- Action: Refine with context

### Round 2 (Strike 2)
- Agent: crate-researcher (refined)
- Confidence: MEDIUM
- Gaps: [fewer]
- Action: Still need comparison data, try docs-researcher

### Round 3 (Strike 3)
- Agent: docs-researcher
- Confidence: MEDIUM
- Action: Synthesize best-effort answer
```

### Negotiation Escalation Protocol

```
Strike 1: Initial query returns LOW/UNCERTAIN confidence
  ┌─────────────────────────────────────┐
  │ - Review agent's context questions  │
  │ - Provide additional context        │
  │ - Narrow scope if ambiguous         │
  │ - Re-query same agent               │
  └─────────────────────────────────────┘

Strike 2: Refined query still LOW, or gaps block intent
  ┌─────────────────────────────────────┐
  │ - Try alternative agent             │
  │ - Try different source              │
  │ - Combine with other data           │
  └─────────────────────────────────────┘

Strike 3: Still insufficient
  ┌─────────────────────────────────────┐
  │ - Synthesize best-effort answer     │
  │ - Explicitly list remaining gaps    │
  │ - Disclose confidence level to user │
  │ - Suggest manual verification       │
  └─────────────────────────────────────┘
```

### Negotiation vs Error 3-Strike Comparison

| Aspect | Error 3-Strike | Negotiation 3-Strike |
|--------|----------------|----------------------|
| Trigger | Repeated failure | Low confidence |
| Target | Layer escalation | Query refinement |
| Reset | After successful fix | After sufficient answer |
| Final action | Redesign question | Best-effort synthesis |

### Integration Points

**With Meta-Cognition:**
- Negotiation happens within layers, not across
- Layer escalation still follows standard 3-Strike
- Negotiation is for data gathering, not design

**With Router:**
- Router decides if negotiation is needed
- Router evaluates negotiation responses
- Router manages refinement loop

### Example: Negotiation 3-Strike Sequence

```markdown
## Query: "Best practices for async error handling in web APIs"

### Strike 1
- Agent: docs-researcher (tokio error handling)
- Confidence: MEDIUM
- Finding: General tokio error patterns
- Gap: No web-specific patterns
- Action: Need web context

### Strike 2
- Agent: docs-researcher (axum error handling)
- Confidence: MEDIUM
- Finding: Axum error response patterns
- Gap: No integration with tokio patterns
- Action: Try to synthesize

### Strike 3
- Synthesize from both rounds
- Answer: Combined tokio + axum patterns
- Disclosed gaps:
  - No official "best practice" document exists
  - Patterns compiled from multiple sources
- Confidence: MEDIUM (synthesized)
```

### Related Documents

- `negotiation-protocol.md` - Full negotiation specification
- `negotiation-templates.md` - Response templates
- `reasoning-framework.md` - Layer tracing
