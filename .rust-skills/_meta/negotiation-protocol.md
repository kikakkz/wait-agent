# Sub-Agent Context Negotiation Protocol

> Protocol for structured communication between orchestrator and sub-agents.

## Problem Statement

Sub-agent summaries often miss critical details because they lack the full semantic context behind the original request. Like an employee summarizing a meeting without knowing the boss's priorities, the summary may omit exactly what's needed.

## Core Principle

**Every sub-agent response is a negotiation, not a final answer.**

The orchestrator evaluates each response against the original intent and can refine queries until sufficient information is gathered.

---

## Protocol Overview

### 1. Negotiation Request

When dispatching to an agent, the orchestrator MAY enable negotiation mode:

```
negotiation: true
```

This signals the agent to return a structured response instead of a plain answer.

### 2. Negotiation Response

Agent returns structured response with:
- **Findings**: What was discovered
- **Confidence**: How reliable the findings are
- **Gaps**: What couldn't be found or verified
- **Context Needed**: Questions for the orchestrator

### 3. Orchestrator Evaluation

Orchestrator checks:
1. Does confidence meet threshold?
2. Do gaps affect the original intent?
3. Can context questions be answered?

### 4. Refinement Loop

If insufficient:
1. Provide additional context
2. Re-query with refined parameters
3. Maximum 3 rounds before synthesis

---

## Negotiation Triggers

### Always Enable Negotiation

| Query Pattern | Example | Reason |
|---------------|---------|--------|
| Comparative | "Compare X and Y" | Requires data from multiple sources |
| Cross-domain | "E0382 in trading system" | Needs both technical and domain context |
| Ambiguous scope | "tokio performance" | Unclear what aspect to measure |
| Synthesis | "best practices for X" | Requires aggregation from multiple sources |
| Multi-faceted | "how to design auth" | Multiple valid approaches |

### Skip Negotiation

| Query Pattern | Example | Reason |
|---------------|---------|--------|
| Single lookup | "tokio latest version" | Direct answer possible |
| Error code | "what is E0382" | Defined meaning |
| Simple definition | "what is Send trait" | Factual lookup |

---

## Confidence Levels

### HIGH

Agent found comprehensive information with multiple sources or official documentation.

**Criteria:**
- Primary source available (official docs, release notes)
- Core data complete
- No conflicting information

**Action:** Accept response, proceed to synthesis

### MEDIUM

Agent found partial information, some gaps exist but don't block core understanding.

**Criteria:**
- Some source available
- Core data found, but incomplete
- Minor gaps identified

**Action:** Evaluate if gaps affect intent; may refine or accept

### LOW

Agent found limited information with significant gaps.

**Criteria:**
- Minimal sources
- Core data incomplete
- Significant gaps

**Action:** Refine query with additional context

### UNCERTAIN

Agent couldn't find reliable information or encountered errors.

**Criteria:**
- No reliable sources
- Contradictory information
- Fetch failures

**Action:** Try alternative agent/source or escalate

---

## Negotiation Flow

```
User Question
     │
     ▼
┌─────────────────────────────────────────┐
│ [1] Router Analysis                     │
│     - Parse intent                      │
│     - Check if negotiation needed       │
│     - If single-lookup: skip to direct  │
└─────────────────────────────────────────┘
     │
     ▼ (negotiation needed)
┌─────────────────────────────────────────┐
│ [2] Dispatch Agent (negotiation: true)  │
│     - Include original query            │
│     - Include known context             │
└─────────────────────────────────────────┘
     │
     ▼
┌─────────────────────────────────────────┐
│ [3] Agent Returns Structured Response   │
│     - Findings                          │
│     - Confidence                        │
│     - Gaps                              │
│     - Context Needed                    │
└─────────────────────────────────────────┘
     │
     ▼
┌─────────────────────────────────────────┐
│ [4] Orchestrator Evaluation             │
│     - Map findings to original intent   │
│     - Check confidence threshold        │
│     - Assess gap impact                 │
└─────────────────────────────────────────┘
     │
     ├──► (sufficient) ──► [6] Synthesize
     │
     ▼ (insufficient, rounds < 3)
┌─────────────────────────────────────────┐
│ [5] Refine Query                        │
│     - Answer agent's context questions  │
│     - Narrow scope                      │
│     - Try alternative agent             │
└─────────────────────────────────────────┘
     │
     └──► Loop to [2]

If rounds = 3 and still insufficient:
     │
     ▼
┌─────────────────────────────────────────┐
│ [6] Synthesize Best-Effort Answer       │
│     - Combine all findings              │
│     - Explicitly state gaps             │
│     - Disclose confidence level         │
└─────────────────────────────────────────┘
```

---

## Integration with Existing Systems

### With Meta-Cognition Framework

Negotiation extends the L1/L2/L3 tracing:

```
Standard:  User → Router → Skill → [Answer]

Negotiation:
           User → Router → Skill/Agent → [Structured Response]
                              ↑                    ↓
                              └──── [Evaluate] ◄──┘
                                       ↓
                                 [Sufficient?]
                                  ↓         ↓
                                Yes        No
                                  ↓         ↓
                           [Synthesize] [Refine]
```

### With 3-Strike Rule

Negotiation follows the same escalation principle:

```
Strike 1: Initial query returns LOW confidence
  → Refine with more context

Strike 2: Refined query still LOW
  → Try alternative agent/source

Strike 3: Still insufficient
  → Synthesize best-effort answer
  → Report gaps to user
```

See `error-protocol.md` for the extended negotiation rules.

---

## Orchestrator Responsibilities

### 1. Intent Preservation

Always track the original user intent. Map each agent response back to:
- What aspect of the question does this answer?
- What aspects remain unanswered?

### 2. Context Accumulation

Across negotiation rounds, accumulate:
- Confirmed facts
- Ruled-out options
- Remaining uncertainties

### 3. Gap Assessment

For each identified gap, ask:
- Does this gap block answering the user's question?
- Can this gap be filled with another query?
- Is partial answer acceptable?

### 4. Final Synthesis

When synthesizing:
- Combine findings from all rounds
- State confidence level
- Disclose any remaining gaps
- Provide source attribution

---

## Agent Responsibilities

### 1. Honest Assessment

Report confidence honestly:
- Don't inflate confidence
- Acknowledge limitations
- Identify gaps proactively

### 2. Structured Response

Follow response format exactly:
- All sections required when negotiation enabled
- Clear categorization of confidence
- Specific gap identification

### 3. Context Questions

Ask relevant questions:
- Don't ask obvious questions
- Focus on blockers
- Prioritize by impact on answer quality

---

## Example Scenarios

### Scenario 1: Comparative Query

**Query:** "Compare tokio and async-std for web servers"

**Round 1 (tokio agent):**
```
Confidence: MEDIUM
Gaps: No web-specific benchmarks, no async-std comparison
Context Needed: Which web framework? Is multi-threaded runtime needed?
```

**Orchestrator Evaluation:**
- Intent: Compare two runtimes for web use
- Gap impact: HIGH - need both runtimes
- Action: Answer context, query async-std

**Round 2 (with context: axum/tide, yes multi-threaded):**
```
Confidence: HIGH
Gaps: No formal benchmarks (resolved: documented characteristics)
```

**Synthesis:**
- Combine findings
- Note: No formal benchmarks, based on documented characteristics

### Scenario 2: Cross-Domain Query

**Query:** "E0382 in my trading system"

**Round 1 (error lookup):**
```
Confidence: HIGH for E0382 definition
Gaps: No trading-specific context
Context Needed: What data is being moved? Is this shared state?
```

**Orchestrator Evaluation:**
- Technical answer sufficient for error meaning
- Domain context needed for appropriate fix
- Action: Provide trading context, invoke domain-fintech

**Round 2 (with trading context):**
```
Confidence: HIGH
Finding: Trading records need Arc<T> for audit compliance
```

**Synthesis:**
- Error meaning: Use of moved value
- Domain-appropriate fix: Arc<TradeRecord> for shared immutable audit data

---

## Related Documents

- `_meta/reasoning-framework.md` - Cognitive layer tracing
- `_meta/error-protocol.md` - 3-Strike escalation (extended for negotiation)
- `agents/_negotiation/response-format.md` - Standard response template
- `agents/_negotiation/confidence-rubric.md` - Confidence assessment criteria
