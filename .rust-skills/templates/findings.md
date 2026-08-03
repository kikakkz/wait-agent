# Findings Template

> Use this template to record discoveries during problem-solving.
> Copy to `_reasoning/findings.md` when starting a complex problem.

---

# Findings

## Problem Context
<!-- Brief reminder of what problem is being solved -->

---

## Layer 3: Domain Constraints

<!-- Constraints discovered from domain-* skills or domain analysis -->

| Constraint | Source | Implication |
|------------|--------|-------------|
| <!-- constraint --> | <!-- domain-* skill or analysis --> | <!-- what this means for design --> |
| <!-- constraint --> | <!-- domain-* skill or analysis --> | <!-- what this means for design --> |

### Domain Rules Identified
- [ ] <!-- Rule 1 -->
- [ ] <!-- Rule 2 -->
- [ ] <!-- Rule 3 -->

---

## Layer 2: Design Patterns

### Patterns Considered

| Pattern | Appropriate? | Reason |
|---------|--------------|--------|
| <!-- pattern name --> | Yes / No / Maybe | <!-- why --> |
| <!-- pattern name --> | Yes / No / Maybe | <!-- why --> |
| <!-- pattern name --> | Yes / No / Maybe | <!-- why --> |

### Selected Pattern
- **Pattern**: <!-- chosen pattern -->
- **Skill Source**: <!-- m09-m15 -->
- **Rationale**: <!-- why this pattern fits the constraints -->

---

## Layer 1: Implementation Details

### Rust Mechanisms Involved

| Mechanism | How It Applies | Skill Reference |
|-----------|----------------|-----------------|
| <!-- mechanism --> | <!-- application --> | <!-- m01-m07 --> |
| <!-- mechanism --> | <!-- application --> | <!-- m01-m07 --> |

### Key Code Patterns
```rust
// Pattern 1: [description]
// code example

// Pattern 2: [description]
// code example
```

---

## Cross-References

### Skills Consulted

| Skill | Section | Key Takeaway |
|-------|---------|--------------|
| <!-- skill --> | <!-- section --> | <!-- takeaway --> |
| <!-- skill --> | <!-- section --> | <!-- takeaway --> |

### External References

| Source | Link/Location | Relevant Info |
|--------|---------------|---------------|
| <!-- source --> | <!-- link --> | <!-- info --> |
| <!-- source --> | <!-- link --> | <!-- info --> |

---

## Trade-offs Identified

### Option A: <!-- name -->
| Aspect | Evaluation |
|--------|------------|
| **Pros** | <!-- benefits --> |
| **Cons** | <!-- drawbacks --> |
| **Fits Domain?** | <!-- yes/no + why --> |
| **Complexity** | <!-- low/medium/high --> |

### Option B: <!-- name -->
| Aspect | Evaluation |
|--------|------------|
| **Pros** | <!-- benefits --> |
| **Cons** | <!-- drawbacks --> |
| **Fits Domain?** | <!-- yes/no + why --> |
| **Complexity** | <!-- low/medium/high --> |

### Option C: <!-- name -->
| Aspect | Evaluation |
|--------|------------|
| **Pros** | <!-- benefits --> |
| **Cons** | <!-- drawbacks --> |
| **Fits Domain?** | <!-- yes/no + why --> |
| **Complexity** | <!-- low/medium/high --> |

---

## Constraints Summary

### Must Have (Non-negotiable)
1. <!-- constraint from domain rules -->
2. <!-- constraint from domain rules -->

### Should Have (Important)
1. <!-- preferred but flexible -->
2. <!-- preferred but flexible -->

### Nice to Have (Optional)
1. <!-- bonus if achievable -->
2. <!-- bonus if achievable -->

---

## Open Questions

- [ ] <!-- Question 1 -->
- [ ] <!-- Question 2 -->
- [ ] <!-- Question 3 -->

---

## Key Insights

<!-- Summary of most important discoveries -->

1. **Domain Insight**: <!-- key domain learning -->
2. **Design Insight**: <!-- key pattern learning -->
3. **Implementation Insight**: <!-- key Rust learning -->
