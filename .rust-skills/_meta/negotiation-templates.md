# Negotiation Response Templates

> Standard templates for agent negotiation responses.

## Agent Response Template

When `negotiation: true`, agents MUST use this format:

```markdown
## Negotiation Response

### Findings
[Query results - what was discovered]

### Confidence
- **Level**: HIGH | MEDIUM | LOW | UNCERTAIN
- **Reason**: [Why this confidence level]

### Gaps Identified
- [ ] [Gap 1: What couldn't be found or verified]
- [ ] [Gap 2: Incomplete information area]
- [x] [Gap 3: Initially missing but resolved] (if applicable)

### Context Needed
- Q1: [Clarifying question for orchestrator]
- Q2: [Scope/constraint question]

### Metadata
- **Source**: [Where data came from]
- **Coverage**: [Query coverage, e.g., "80% - missing X"]
```

---

## Template by Agent Type

### crate-researcher

```markdown
## Negotiation Response

### Findings
**Crate:** <name>
**Version:** <version>
**Description:** <description>

**Features:**
- `feature1`: description

**Dependencies:** [if relevant]

### Confidence
- **Level**: [HIGH|MEDIUM|LOW|UNCERTAIN]
- **Reason**: [e.g., "Found on lib.rs with complete metadata"]

### Gaps Identified
- [ ] Feature documentation incomplete
- [ ] Version history not available
- [ ] Dependency tree not fetched
- [ ] Breaking changes unknown
- [ ] Comparison data not available (for comparative queries)

### Context Needed
- Q1: Is this for async or sync usage?
- Q2: Which features do you plan to enable?
- Q3: What's the minimum supported Rust version?

### Metadata
- **Source**: lib.rs | crates.io | docs.rs
- **Coverage**: [e.g., "90% - missing changelog"]
```

### docs-researcher

```markdown
## Negotiation Response

### Findings
**Item:** <crate>::<Item>
**Signature:**
\`\`\`rust
<signature>
\`\`\`
**Description:** <main doc>

**Examples found:** [yes/no, count]

### Confidence
- **Level**: [HIGH|MEDIUM|LOW|UNCERTAIN]
- **Reason**: [e.g., "Official docs.rs with examples"]

### Gaps Identified
- [ ] No usage examples
- [ ] Missing error documentation
- [ ] Related types not fetched
- [ ] Version-specific behavior unclear

### Context Needed
- Q1: Which version are you using?
- Q2: What's the specific use case?
- Q3: Do you need error handling patterns?

### Metadata
- **Source**: docs.rs/<crate>/<version>
- **Coverage**: [e.g., "70% - no examples"]
```

### std-docs-researcher

```markdown
## Negotiation Response

### Findings
**Item:** std::<path>::<Item>
**Signature:**
\`\`\`rust
<signature>
\`\`\`
**Key Points:**
- Point 1
- Point 2

**Related items:** [if relevant]

### Confidence
- **Level**: [HIGH|MEDIUM|LOW|UNCERTAIN]
- **Reason**: [e.g., "Official Rust documentation"]

### Gaps Identified
- [ ] Implementation details not covered
- [ ] Platform-specific behavior unclear
- [ ] Related traits not fetched
- [ ] Performance characteristics unknown

### Context Needed
- Q1: Which platform/target?
- Q2: Is this for no_std environment?
- Q3: Do you need thread-safety guarantees?

### Metadata
- **Source**: doc.rust-lang.org/std
- **Coverage**: [e.g., "95% - standard docs complete"]
```

### clippy-researcher

```markdown
## Negotiation Response

### Findings
**Lint:** clippy::<lint_name>
**Level:** warn | deny | allow
**Category:** correctness | style | complexity | perf | pedantic

**What it checks:** <description>
**Why it matters:** <rationale>

**Bad example:**
\`\`\`rust
<triggering code>
\`\`\`

**Good example:**
\`\`\`rust
<fixed code>
\`\`\`

### Confidence
- **Level**: [HIGH|MEDIUM|LOW|UNCERTAIN]
- **Reason**: [e.g., "Official clippy documentation"]

### Gaps Identified
- [ ] Edge cases not documented
- [ ] Configuration options unclear
- [ ] Related lints not listed
- [ ] False positive scenarios unknown

### Context Needed
- Q1: What's triggering this lint?
- Q2: Is suppression acceptable for your use case?

### Metadata
- **Source**: rust-lang.github.io/rust-clippy
- **Coverage**: [e.g., "100% - lint fully documented"]
```

### rust-changelog

```markdown
## Negotiation Response

### Findings
**Version:** Rust <version>
**Release Date:** <date>

**Language Features:**
- Feature 1: description

**Stabilized APIs:**
- API 1: description

**Breaking Changes:**
- Change 1: description

### Confidence
- **Level**: [HIGH|MEDIUM|LOW|UNCERTAIN]
- **Reason**: [e.g., "Official release notes from releases.rs"]

### Gaps Identified
- [ ] Migration guide not available
- [ ] Edition changes not detailed
- [ ] Cargo changes incomplete
- [ ] MSRV impact unclear

### Context Needed
- Q1: Are you migrating from a specific version?
- Q2: Do you need edition-specific changes?

### Metadata
- **Source**: releases.rs/docs/<version>
- **Coverage**: [e.g., "85% - missing detailed migration"]
```

---

## Orchestrator Evaluation Template

When evaluating agent responses:

```markdown
## Evaluation: [Agent] Response

### Intent Mapping
- Original question aspect 1: [COVERED|PARTIAL|MISSING]
- Original question aspect 2: [COVERED|PARTIAL|MISSING]

### Confidence Assessment
- Agent reported: [LEVEL]
- Adjusted assessment: [LEVEL] (if different)
- Reason: [why adjustment]

### Gap Impact
| Gap | Impact on Intent | Action |
|-----|------------------|--------|
| [Gap 1] | [HIGH|MEDIUM|LOW] | [refine|accept|ignore] |
| [Gap 2] | [HIGH|MEDIUM|LOW] | [refine|accept|ignore] |

### Context Provision
[Answers to agent's context questions]
- A1: [answer to Q1]
- A2: [answer to Q2]

### Decision
- [ ] ACCEPT - Proceed to synthesis
- [ ] REFINE - Query again with context
- [ ] ALTERNATIVE - Try different agent
- [ ] ESCALATE - Need user input
```

---

## Refinement Request Template

When re-querying an agent:

```markdown
## Refined Query

### Original Query
[Original question]

### Previous Round Summary
- Findings: [key findings from last round]
- Gaps to address: [specific gaps]

### Additional Context
[Answers to agent's questions]
- [Context 1]
- [Context 2]

### Focus Areas
[Specific aspects to focus on this round]
1. [Focus 1]
2. [Focus 2]

### Round
[2|3] of 3
```

---

## Final Synthesis Template

```markdown
## Synthesized Answer

### Summary
[Direct answer to user's question]

### Details
[Comprehensive findings from all rounds]

### Confidence
- **Overall**: [HIGH|MEDIUM|LOW]
- **Basis**: [What this is based on]

### Disclosed Gaps
[Any remaining gaps that user should know about]
- Gap 1: [what's missing and why]

### Sources
- [Source 1]: [what it provided]
- [Source 2]: [what it provided]

### Negotiation Rounds
- Round 1: [agent] - [key finding]
- Round 2: [agent] - [key finding] (if applicable)
```

---

## Quick Reference

### Confidence Quick Assessment

| Data Quality | Sources | Confidence |
|--------------|---------|------------|
| Complete + verified | Official docs | HIGH |
| Partial + verified | Official docs | MEDIUM |
| Partial + unverified | Third-party | LOW |
| Missing + errors | None/failed | UNCERTAIN |

### Gap Priority

| Gap Type | Priority | Action |
|----------|----------|--------|
| Blocks core answer | HIGH | Must refine |
| Affects accuracy | MEDIUM | Refine if rounds available |
| Nice-to-have | LOW | Accept, disclose |
