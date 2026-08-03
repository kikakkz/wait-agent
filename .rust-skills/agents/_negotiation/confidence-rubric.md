# Confidence Assessment Rubric

> Standardized criteria for determining confidence levels in negotiation responses.

## Confidence Levels Overview

| Level | Symbol | Meaning | Typical Action |
|-------|--------|---------|----------------|
| HIGH | ✓✓✓ | Reliable, complete | Accept |
| MEDIUM | ✓✓ | Partial, usable | Evaluate gaps |
| LOW | ✓ | Limited, significant gaps | Refine |
| UNCERTAIN | ? | Unreliable or failed | Alternative |

---

## HIGH Confidence

### Definition

Agent found comprehensive, verified information from authoritative sources. Core data is complete with no significant gaps.

### Criteria (must meet ALL)

- [ ] Primary/official source available
- [ ] Core requested data complete
- [ ] No conflicting information
- [ ] Data is current (not outdated)

### Source Quality for HIGH

| Source Type | Qualifies? | Notes |
|-------------|------------|-------|
| Official documentation | Yes | doc.rust-lang.org, docs.rs |
| Official release notes | Yes | releases.rs, GitHub releases |
| Crate registry (lib.rs, crates.io) | Yes | For version/metadata |
| Official blog posts | Yes | blog.rust-lang.org |
| Third-party tutorials | No | May be outdated |
| Stack Overflow | No | Varies in quality |

### Examples

```
Query: "What is serde's latest version?"
Finding: Version 1.0.219 from lib.rs
Confidence: HIGH
Reason: Official registry data, single authoritative answer
```

```
Query: "What does Send trait do?"
Finding: Definition from doc.rust-lang.org
Confidence: HIGH
Reason: Official Rust documentation, stable definition
```

---

## MEDIUM Confidence

### Definition

Agent found partial information. Core data exists but with gaps that don't block understanding.

### Criteria (must meet MOST)

- [ ] Some authoritative source available
- [ ] Core data found (may be incomplete)
- [ ] Minor gaps identified
- [ ] No major conflicts

### Common MEDIUM Scenarios

| Scenario | Why MEDIUM |
|----------|------------|
| Found version but not changelog | Core info present, detail missing |
| Found API but not examples | Usable, but not complete |
| Found one side of comparison | Partial answer |
| Found info but slightly outdated | Usable with caveat |

### Examples

```
Query: "What features does tokio have?"
Finding: Main features from lib.rs, but feature flags not fully documented
Confidence: MEDIUM
Reason: Core features known, but complete feature matrix not found
```

```
Query: "Compare tokio and async-std"
Finding: General characteristics of both, no benchmarks
Confidence: MEDIUM
Reason: Qualitative comparison possible, quantitative data missing
```

---

## LOW Confidence

### Definition

Agent found limited information with significant gaps. Answer may be incomplete or unreliable.

### Criteria (any of these)

- [ ] Minimal authoritative sources
- [ ] Core data incomplete
- [ ] Significant gaps that affect usefulness
- [ ] Information may be outdated
- [ ] Single non-authoritative source

### Common LOW Scenarios

| Scenario | Why LOW |
|----------|---------|
| Only found crate name, no docs | Missing core info |
| Found outdated information | Currency concern |
| Third-party source only | Authority concern |
| Conflicting information found | Reliability concern |

### Examples

```
Query: "Best practices for async error handling"
Finding: A few blog posts with different recommendations
Confidence: LOW
Reason: No authoritative source, opinions vary
```

```
Query: "What's new in obscure-crate 2.0?"
Finding: Only found GitHub issues mentioning 2.0
Confidence: LOW
Reason: No official changelog, incomplete information
```

---

## UNCERTAIN Confidence

### Definition

Agent couldn't find reliable information or encountered errors that prevent a trustworthy answer.

### Criteria (any of these)

- [ ] No sources found
- [ ] Fetch/access errors
- [ ] Contradictory information
- [ ] Source clearly unreliable
- [ ] Request outside agent capability

### Common UNCERTAIN Scenarios

| Scenario | Why UNCERTAIN |
|----------|---------------|
| 404 errors on docs | Cannot verify |
| Crate doesn't exist | No data |
| Conflicting official sources | Cannot determine truth |
| Request for future features | Speculation |

### Examples

```
Query: "What is nonexistent-crate?"
Finding: Crate not found on any registry
Confidence: UNCERTAIN
Reason: Crate does not exist or is private
```

```
Query: "What will Rust 2.0 include?"
Finding: No official roadmap
Confidence: UNCERTAIN
Reason: Future features are speculative
```

---

## Agent-Specific Rubrics

### crate-researcher

| Data Found | Confidence |
|------------|------------|
| Version + description + features + docs | HIGH |
| Version + description + features | HIGH |
| Version + description | MEDIUM |
| Version only | LOW |
| Not found or error | UNCERTAIN |

**Degrading factors:**
- Last update > 2 years: -1 level
- No README: -1 level
- Yanked versions: mention in gaps

### docs-researcher

| Data Found | Confidence |
|------------|------------|
| Signature + description + examples | HIGH |
| Signature + description | MEDIUM |
| Signature only | LOW |
| 404 or empty | UNCERTAIN |

**Degrading factors:**
- docs.rs build failed: -1 level
- No examples: note in gaps
- Deprecated item: mention in gaps

### std-docs-researcher

| Data Found | Confidence |
|------------|------------|
| Full documentation | HIGH |
| Basic documentation | MEDIUM |
| Minimal/stub docs | LOW |
| Not found | UNCERTAIN |

**Note:** std docs are generally HIGH confidence when found.

### clippy-researcher

| Data Found | Confidence |
|------------|------------|
| Full lint info with examples | HIGH |
| Lint info, no examples | MEDIUM |
| Lint exists, minimal info | LOW |
| Lint not found | UNCERTAIN |

### rust-changelog

| Data Found | Confidence |
|------------|------------|
| Full release notes | HIGH |
| Partial notes (some sections) | MEDIUM |
| Minimal info | LOW |
| Version not found | UNCERTAIN |

---

## Confidence Adjustments

### Upgrade Conditions

| Condition | Adjustment |
|-----------|------------|
| Multiple sources agree | +1 level (max HIGH) |
| Official source confirms | +1 level (max HIGH) |
| Recent verification | +1 level (max HIGH) |

### Downgrade Conditions

| Condition | Adjustment |
|-----------|------------|
| Information outdated (> 1 year) | -1 level |
| Single non-official source | -1 level |
| Partial data only | -1 level |
| Any fetch errors | -1 level |

---

## Decision Matrix

| Confidence | Orchestrator Action |
|------------|---------------------|
| HIGH | Accept, synthesize answer |
| MEDIUM | Evaluate gaps; accept if gaps don't block intent |
| LOW | Refine query with context; or try alternative |
| UNCERTAIN | Try alternative source/agent; or report limitation |

---

## Reporting Format

```markdown
### Confidence
- **Level**: [HIGH|MEDIUM|LOW|UNCERTAIN]
- **Reason**: [Specific reason following this rubric]
```

### Good Reasons

```
HIGH - Official docs complete with examples
MEDIUM - Found on lib.rs but changelog unavailable
LOW - Only found GitHub issues, no official docs
UNCERTAIN - Crate does not exist on any registry
```

### Bad Reasons

```
HIGH - Looks good (vague)
MEDIUM - Some info (doesn't explain what's missing)
LOW - Not great (doesn't explain why)
UNCERTAIN - Didn't work (doesn't explain what failed)
```

---

## Related Documents

- `response-format.md` - Full response structure
- `_meta/negotiation-protocol.md` - Protocol specification
- `_meta/error-protocol.md` - 3-Strike escalation
