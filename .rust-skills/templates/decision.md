# Decision Record Template

> Use this template to document final decisions and their rationale.
> Copy to `_reasoning/decision.md` when a significant design decision is made.
> This serves as an Architecture Decision Record (ADR) for the problem.

---

# Decision Record

## Metadata
- **Date**: <!-- YYYY-MM-DD -->
- **Status**: <!-- Proposed / Accepted / Deprecated / Superseded -->
- **Decision Makers**: <!-- who was involved -->

---

## Context

### Problem Statement
<!-- What is the problem being addressed? -->

### Background
<!-- What led to this decision being needed? -->

### Constraints
<!-- What constraints must be satisfied? -->
- Domain: <!-- from Layer 3 -->
- Technical: <!-- from Layer 1-2 -->
- Other: <!-- time, resources, etc. -->

---

## Decision

### Summary
<!-- One-sentence summary of the decision -->

### Details
<!-- Detailed description of what was decided -->

---

## Rationale

### Layer 3 (Domain Constraints)

| Domain Rule | How Decision Satisfies |
|-------------|----------------------|
| <!-- rule --> | <!-- how it's satisfied --> |
| <!-- rule --> | <!-- how it's satisfied --> |

**Domain Fit Score**: <!-- Low / Medium / High -->

### Layer 2 (Design Choice)

| Design Principle | How Decision Applies |
|------------------|---------------------|
| <!-- principle --> | <!-- application --> |
| <!-- principle --> | <!-- application --> |

**Pattern Used**: <!-- pattern name from m09-m15 -->
**Skill Reference**: <!-- skill file and section -->

### Layer 1 (Implementation)

| Rust Mechanism | Usage |
|----------------|-------|
| <!-- mechanism --> | <!-- how used --> |
| <!-- mechanism --> | <!-- how used --> |

**Implementation Approach**: <!-- brief description -->
**Skill Reference**: <!-- skill file and section -->

---

## Consequences

### Positive
- <!-- benefit 1 -->
- <!-- benefit 2 -->
- <!-- benefit 3 -->

### Negative (Accepted Trade-offs)
- <!-- trade-off 1 -->
- <!-- trade-off 2 -->

### Neutral (Side Effects)
- <!-- side effect 1 -->
- <!-- side effect 2 -->

---

## Alternatives Considered

### Alternative 1: <!-- name -->
- **Description**: <!-- what it would involve -->
- **Why Rejected**: <!-- reason -->
- **When Might Reconsider**: <!-- future scenario -->

### Alternative 2: <!-- name -->
- **Description**: <!-- what it would involve -->
- **Why Rejected**: <!-- reason -->
- **When Might Reconsider**: <!-- future scenario -->

### Alternative 3: <!-- name -->
- **Description**: <!-- what it would involve -->
- **Why Rejected**: <!-- reason -->
- **When Might Reconsider**: <!-- future scenario -->

---

## Implementation Notes

### Key Files to Modify
| File | Change |
|------|--------|
| <!-- file path --> | <!-- what changes --> |
| <!-- file path --> | <!-- what changes --> |

### Code Pattern
```rust
// Key implementation pattern
// code example
```

### Testing Strategy
- <!-- how to verify the decision works -->
- <!-- edge cases to test -->

---

## Validation Criteria

### Success Metrics
- [ ] <!-- criterion 1 -->
- [ ] <!-- criterion 2 -->
- [ ] <!-- criterion 3 -->

### Failure Indicators
- <!-- what would indicate this decision was wrong -->
- <!-- when to reconsider -->

---

## Related Decisions

| Decision | Relationship |
|----------|-------------|
| <!-- link to other decision --> | <!-- how related --> |
| <!-- link to other decision --> | <!-- how related --> |

---

## Review Notes

### Lessons Learned
<!-- What was learned from this decision process? -->

### Process Improvements
<!-- How could the decision-making process be improved? -->

### Skills to Revisit
<!-- Which skills should be updated based on this experience? -->

---

## Approval

- [ ] Domain constraints verified
- [ ] Design pattern appropriate
- [ ] Implementation feasible
- [ ] Trade-offs acceptable
- [ ] Decision documented
