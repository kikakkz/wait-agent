# std-docs-researcher

Fetch Rust std library documentation from doc.rust-lang.org.

## URL Patterns

| Type | URL |
|------|-----|
| Trait | `doc.rust-lang.org/std/marker/trait.Send.html` |
| Struct | `doc.rust-lang.org/std/sync/struct.Arc.html` |
| Module | `doc.rust-lang.org/std/collections/index.html` |
| Function | `doc.rust-lang.org/std/mem/fn.replace.html` |

## Common Paths

| Item | Path |
|------|------|
| Send, Sync, Copy, Clone | `std/marker/trait.<Name>.html` |
| Arc, Mutex, RwLock | `std/sync/struct.<Name>.html` |
| RefCell, Cell | `std/cell/struct.<Name>.html` |
| Vec | `std/vec/struct.Vec.html` |
| Option, Result | `std/<name>/enum.<Name>.html` |

## Fetch

Use available tools to get doc.rust-lang.org content.

## Cache

Location: `~/.claude/cache/rust-docs/std/{module}/{item}.json`
TTL: 30 days (std is stable)

## Output (Standard Mode)

```markdown
## std::<Item>

**Signature:**
\`\`\`rust
<signature>
\`\`\`

**Description:** <main doc>

**Key Points:**
- point 1
- point 2
```

## Validation

1. Content is not empty
2. Not a 404 page
3. Contains signature or docblock
4. On failure: "Fetch failed: {reason}, see doc.rust-lang.org"

---

## Negotiation Mode

When `negotiation: true`, return structured response per `_negotiation/response-format.md`.

### Confidence Assessment

| Data Found | Confidence |
|------------|------------|
| Full documentation | HIGH |
| Basic documentation | MEDIUM |
| Minimal/stub docs | LOW |
| Not found | UNCERTAIN |

**Note:** std docs are generally HIGH confidence when found, as they are official and stable.

### Gap Categories

Standard gaps to check:

- [ ] Implementation details not covered
- [ ] Platform-specific behavior unclear
- [ ] Related traits not fetched
- [ ] Performance characteristics unknown
- [ ] Unsafe usage notes missing
- [ ] no_std compatibility unclear

### Context Questions

When std documentation request needs clarification:

| Situation | Question |
|-----------|----------|
| Platform-specific | "Which platform/target?" |
| no_std context | "Is this for no_std environment?" |
| Thread safety | "Do you need thread-safety guarantees?" |
| Unsafe usage | "Are you using this in unsafe context?" |

### Negotiation Response Template

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
- [ ] [Specific gap 1]
- [ ] [Specific gap 2]

### Context Needed
- Q1: [If ambiguous]

### Metadata
- **Source**: doc.rust-lang.org/std
- **Coverage**: [e.g., "95% - standard docs complete"]
```

### Related Documents

- `_negotiation/response-format.md` - Response structure
- `_negotiation/confidence-rubric.md` - Confidence criteria
