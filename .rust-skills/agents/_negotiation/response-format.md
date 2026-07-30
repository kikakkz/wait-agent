# Negotiation Response Format

> Standard response structure for agents in negotiation mode.

## When to Use

This format is REQUIRED when the orchestrator dispatches with `negotiation: true`.

For standard (non-negotiation) queries, use the agent's default output format.

---

## Response Structure

```markdown
## Negotiation Response

### Findings
[Primary query results]

### Confidence
- **Level**: HIGH | MEDIUM | LOW | UNCERTAIN
- **Reason**: [Brief explanation]

### Gaps Identified
- [ ] [Gap 1]
- [ ] [Gap 2]

### Context Needed
- Q1: [Question]
- Q2: [Question]

### Metadata
- **Source**: [Data source]
- **Coverage**: [Coverage assessment]
```

---

## Section Requirements

### Findings (Required)

What the agent discovered. This is the core content.

**Guidelines:**
- Include all relevant data found
- Structure clearly (use sub-headers if complex)
- Don't omit data just because it seems obvious
- Include raw data, let orchestrator synthesize

**Example:**
```markdown
### Findings
**Crate:** tokio
**Version:** 1.49.0
**Description:** An event-driven, non-blocking I/O platform

**Key Features:**
- `full`: Enables all features
- `rt-multi-thread`: Multi-threaded runtime
- `sync`: Synchronization primitives

**Recent Changes:**
- 1.49.0: Added cooperative scheduling improvements
```

### Confidence (Required)

Self-assessment of finding reliability.

| Level | Meaning | Criteria |
|-------|---------|----------|
| HIGH | Reliable, complete | Primary source, core data complete |
| MEDIUM | Partial, usable | Some source, core data found |
| LOW | Limited, gaps | Minimal sources, incomplete |
| UNCERTAIN | Unreliable | No sources, errors, conflicts |

**Example:**
```markdown
### Confidence
- **Level**: MEDIUM
- **Reason**: Found crate info on lib.rs, but changelog not accessible
```

### Gaps Identified (Required)

What couldn't be found or verified.

**Guidelines:**
- Be specific about what's missing
- Use checkboxes to allow marking as resolved
- Prioritize by impact on answer quality
- Don't list irrelevant gaps

**Example:**
```markdown
### Gaps Identified
- [ ] Performance benchmarks not found
- [ ] Breaking changes from 1.48 unknown
- [x] Feature list - resolved from docs.rs
```

### Context Needed (Conditional)

Questions for the orchestrator to enable better answers.

**When to include:**
- Query is ambiguous
- Multiple valid interpretations
- Need domain-specific context
- Scope unclear

**When to skip:**
- Query is unambiguous
- All necessary context provided
- Direct lookup with clear answer

**Example:**
```markdown
### Context Needed
- Q1: Is this for a web server or CLI application?
- Q2: Do you need WebSocket support?
```

**Not needed for:**
```markdown
Query: "What is tokio's latest version?"
→ No context needed, direct lookup
```

### Metadata (Required)

Source attribution and coverage assessment.

**Example:**
```markdown
### Metadata
- **Source**: lib.rs/crates/tokio, docs.rs/tokio/1.49.0
- **Coverage**: 85% - missing performance benchmarks
```

---

## Coverage Assessment Guide

| Coverage | Meaning |
|----------|---------|
| 100% | All requested data found |
| 80-99% | Minor gaps, core complete |
| 50-79% | Significant gaps, partial answer |
| <50% | Major gaps, may need alternative |

---

## Response Examples

### High Confidence Example

```markdown
## Negotiation Response

### Findings
**Crate:** serde
**Version:** 1.0.219
**Description:** A generic serialization/deserialization framework

**Features:**
- `derive`: Enables #[derive(Serialize, Deserialize)]
- `std`: Use standard library (default)
- `alloc`: For no_std with alloc

**Documentation:** Comprehensive with examples
**Ecosystem:** De facto standard for Rust serialization

### Confidence
- **Level**: HIGH
- **Reason**: Official docs complete, widely documented crate

### Gaps Identified
- [x] Core info - complete
- [x] Features - complete
- [ ] Migration from 0.x - not requested, not fetched

### Context Needed
(None - query was specific)

### Metadata
- **Source**: lib.rs, docs.rs/serde/1.0.219
- **Coverage**: 95% - comprehensive for typical use
```

### Low Confidence Example

```markdown
## Negotiation Response

### Findings
**Crate:** obscure-crate
**Version:** 0.1.2 (last updated 2023)
**Description:** [minimal description on crates.io]

**Features:** Unknown (not documented)

### Confidence
- **Level**: LOW
- **Reason**: Minimal documentation, abandoned crate, no recent activity

### Gaps Identified
- [ ] Feature documentation missing
- [ ] No usage examples found
- [ ] Maintenance status unclear
- [ ] No API documentation on docs.rs

### Context Needed
- Q1: Is there an alternative crate you'd consider?
- Q2: What specific functionality do you need from this crate?

### Metadata
- **Source**: crates.io (lib.rs had no additional info)
- **Coverage**: 30% - minimal data available
```

### Comparative Query Example

```markdown
## Negotiation Response

### Findings
**Comparison:** tokio vs async-std (runtime focus)

**tokio:**
- Multi-threaded by default
- Larger ecosystem (axum, tonic, etc.)
- More configuration options

**async-std:**
- Single-threaded default, multi-thread available
- Closer to std API design
- Simpler getting started

**Common:**
- Both support async/await
- Both production-ready

### Confidence
- **Level**: MEDIUM
- **Reason**: General characteristics known, but no benchmark data for specific use case

### Gaps Identified
- [ ] Performance benchmarks for web servers
- [ ] Memory usage comparison
- [ ] Ecosystem compatibility matrix

### Context Needed
- Q1: Which web framework will you use? (axum requires tokio)
- Q2: Is multi-threaded runtime required?
- Q3: What's the expected request volume?

### Metadata
- **Source**: lib.rs for both, official docs
- **Coverage**: 60% - characteristics known, specifics missing
```

---

## Anti-Patterns

### Don't: Inflate Confidence

```markdown
# BAD
Confidence: HIGH
Reason: Found some info
# GOOD
Confidence: MEDIUM
Reason: Found basic info, but detailed docs not accessible
```

### Don't: Vague Gaps

```markdown
# BAD
Gaps: Some things missing
# GOOD
Gaps:
- [ ] Feature `x` documentation not found
- [ ] Version 2.0 migration guide unavailable
```

### Don't: Irrelevant Context Questions

```markdown
# BAD (for query "what is tokio version")
Context Needed: What's your favorite color?
# GOOD
Context Needed: (None - query is specific)
```

### Don't: Skip Metadata

```markdown
# BAD
(no metadata section)
# GOOD
Metadata:
- Source: lib.rs
- Coverage: 90%
```

---

## Related Documents

- `_meta/negotiation-protocol.md` - Full protocol specification
- `_meta/negotiation-templates.md` - Agent-specific templates
- `confidence-rubric.md` - Detailed confidence criteria
