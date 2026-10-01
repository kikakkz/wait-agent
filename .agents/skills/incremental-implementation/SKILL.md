---
name: incremental-implementation
description: Delivers changes incrementally in thin, verifiable slices. Use when implementing any feature or change that touches more than one file, or when picking up the next task from a plan. Use when rolling a change out behind a feature flag, when you're about to write a large amount of code at once, or when a task feels too big to land in one step.
---

<!--
  Vendored from addyosmani/agent-skills (MIT) at revision
  bcab6a1b8503100e8618c3b4e32cc78de43de769, upstream path
  skills/incremental-implementation/SKILL.md.
  Do not edit the content; update by re-vendoring a newer revision.
-->

# Incremental Implementation

## Overview

Build in thin vertical slices — implement one piece, test it, verify it, then expand. Avoid implementing an entire feature in one pass. Each increment should leave the system in a working, testable state. This is the execution discipline that makes large features manageable.

## When to Use

- Implementing any multi-file change
- Building a new feature from a task breakdown
- Refactoring existing code
- Any time you're tempted to write more than ~100 lines before testing

**When NOT to use:** Single-file, single-function changes where the scope is already minimal.

## The Increment Cycle

```
┌──────────────────────────────────────┐
│                                      │
│   Implement ──→ Test ──→ Verify ──┐  │
│       ▲                           │  │
│       └───── Commit ◄─────────────┘  │
│              │                       │
│              ▼                       │
│          Next slice                  │
│                                      │
└──────────────────────────────────────┘
```

For each slice:

1. **Implement** the smallest complete piece of functionality
2. **Test** — run the test suite (or write a test if none exists)
3. **Verify** — confirm the slice works as expected (tests pass, build succeeds, manual check)
4. **Commit** -- save your progress with a descriptive message
5. **Move to the next slice** — carry forward, don't restart

## Slicing Strategies

### Vertical Slices (Preferred)

Build one complete path through the stack. Each slice delivers working end-to-end functionality.

### Contract-First Slicing

Define the contract (types, interfaces) first, then implement both sides against it, then integrate.

### Risk-First Slicing

Tackle the riskiest or most uncertain piece first — if Slice 1 fails, you discover it before investing in the rest.

## Implementation Rules

- **Rule 0: Simplicity first.** The simplest thing that could work; naive and obviously-correct before optimized. Three similar lines beat a premature abstraction.
- **Rule 0.5: Scope discipline.** Touch only what the task requires; note improvements, don't make them.
- **Rule 1: One thing at a time.** Each increment changes one logical thing.
- **Rule 2: Keep it compilable.** Every increment builds and existing tests pass.
- **Rule 3: Feature flags for incomplete features** (where merging hidden work early is needed).
- **Rule 4: Safe defaults.** New behavior defaults to conservative.
- **Rule 5: Rollback-friendly.** Each increment independently revertable.

## Common Rationalizations

| Rationalization | Reality |
|---|---|
| "I'll test it all at the end" | Bugs compound. A bug in Slice 1 makes Slices 2-5 wrong. Test each slice. |
| "It's faster to do it all at once" | It *feels* faster until something breaks and you can't find which of 500 changed lines caused it. |
| "These changes are too small to commit separately" | Small commits are free. Large commits hide bugs and make rollbacks painful. |
| "This refactor is small enough to include" | Refactors mixed with features make both harder to review and debug. Separate them. |

## Red Flags

- More than 100 lines of code written without running tests
- Multiple unrelated changes in a single increment
- "Let me just quickly add this too" scope expansion
- Build or tests broken between increments
- Large uncommitted changes accumulating
- Building abstractions before the third use case demands it
- Touching files outside the task scope "while I'm here"

## Verification

After completing all increments for a task:

- [ ] Each increment was individually tested and committed
- [ ] The full test suite passes (this repository: `cargo test`)
- [ ] The build is clean (`cargo build`)
- [ ] No uncommitted changes remain
