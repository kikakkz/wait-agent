# Reasoning Trace Template

> Use this template to track your reasoning process through the three-layer cognitive model.
> Copy to `_reasoning/trace.md` when starting a complex problem.

---

# Reasoning Trace

## Problem Statement
<!-- Brief description of the problem -->

## Entry Point
- **Signal**: <!-- error code / question type / user request -->
- **Entry Layer**: <!-- 1 (Mechanics) / 2 (Design) / 3 (Domain) -->
- **Initial Skill**: <!-- m0x / m1x / domain-* -->

---

## Trace UP ↑

<!-- Use when starting from an error or implementation question -->

### Layer 1 → Layer 2
- **Question**: What design choice led to this?
- **Skill Consulted**: <!-- m09-m15 -->
- **Finding**: <!-- discovered pattern or design issue -->

### Layer 2 → Layer 3
- **Question**: What domain constraint requires this design?
- **Skill Consulted**: <!-- domain-* -->
- **Finding**: <!-- discovered constraint -->

---

## Trace DOWN ↓

<!-- Use when starting from domain constraints or design questions -->

### Layer 3 → Layer 2
- **Constraint**: <!-- domain rule that applies -->
- **Skill Consulted**: <!-- m09-m15 -->
- **Design Implication**: <!-- pattern choice based on constraint -->

### Layer 2 → Layer 1
- **Pattern**: <!-- chosen design pattern -->
- **Skill Consulted**: <!-- m01-m07 -->
- **Implementation**: <!-- Rust mechanism to use -->

---

## Attempts Log

### Attempt 1
- **Time**: <!-- timestamp -->
- **Approach**: <!-- what was tried -->
- **Result**: <!-- success / failure + details -->
- **Learning**: <!-- what was learned -->

### Attempt 2
- **Time**: <!-- timestamp -->
- **Approach**: <!-- what was tried -->
- **Result**: <!-- success / failure + details -->
- **Learning**: <!-- what was learned -->

### Attempt 3 (Escalation Point)
<!-- If reaching 3 attempts, escalate per error-protocol.md -->
- **Time**: <!-- timestamp -->
- **Escalation**: <!-- which direction: L1→L2 or L2→L3 -->
- **New Approach**: <!-- approach after escalation -->
- **Result**: <!-- outcome -->

---

## Error Log

<!-- Keep errors visible for learning -->

### Error 1
- **Code**: <!-- E0xxx or error type -->
- **Message**: <!-- full error message -->
- **Analysis**: <!-- what went wrong -->
- **Fix**: <!-- how resolved or pending -->

### Error 2
- **Code**: <!-- E0xxx or error type -->
- **Message**: <!-- full error message -->
- **Analysis**: <!-- what went wrong -->
- **Fix**: <!-- how resolved or pending -->

---

## Current Status

- [ ] Problem understood
- [ ] Entry layer identified
- [ ] Trace direction chosen (UP/DOWN)
- [ ] Related constraints found
- [ ] Design pattern selected
- [ ] Implementation approach decided
- [ ] Solution implemented
- [ ] Solution verified

---

## Skills Referenced

| Skill | Section | Key Insight |
|-------|---------|-------------|
| <!-- skill name --> | <!-- section --> | <!-- what was learned --> |
| <!-- skill name --> | <!-- section --> | <!-- what was learned --> |

---

## Notes

<!-- Any additional observations or context -->
