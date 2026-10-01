---
name: test-driven-development
description: Drives development with tests using the red-green-refactor loop. Use when implementing any logic, fixing any bug, or changing any behavior. Use when you need to prove that code works, when a bug report arrives, or when you're about to modify existing functionality.
---

<!--
  Vendored from addyosmani/agent-skills (MIT) at revision
  bcab6a1b8503100e8618c3b4e32cc78de43de769, upstream path
  skills/test-driven-development/SKILL.md.
  Do not edit the content; update by re-vendoring a newer revision.
  This repository's Rust commands apply throughout: focused tests via
  `cargo test <name>`, full suite via `cargo test`, gating per AGENTS.md.
-->

# Test-Driven Development

## Overview

Write a failing test before writing the code that makes it pass. For bug fixes, reproduce the bug with a test before attempting a fix. Tests are proof — "seems right" is not done.

## When to Use

- Implementing any new logic or behavior
- Fixing any bug (the Prove-It Pattern)
- Modifying existing functionality
- Adding edge case handling

**When NOT to use:** Pure configuration changes, documentation updates, or static content changes that have no behavioral impact.

## Discover the Stack First

The TDD cycle is universal; the commands are not. Before writing the first test, discover how *this* repository tests, and use its commands for every RED, GREEN, and verification step:

- **Build system** — `Cargo.toml`, the `Makefile`
- **Checked-in wrappers** — prefer repo scripts over globally installed tools
- **Test framework and conventions** — where tests live, how neighboring tests are structured
- **Documented commands** — AGENTS.md and CI workflows show the commands that actually gate merges

Run the repository's focused-test command during the loop and its full-suite command before completion.

## The TDD Cycle

```
    RED                GREEN              REFACTOR
 Write a test    Write minimal code    Clean up the
 that fails  ──→  to make it pass  ──→  implementation  ──→  (repeat)
      │                  │                    │
      ▼                  ▼                    ▼
   Test FAILS        Test PASSES         Tests still PASS
```

### Step 1: RED — Write a Failing Test

Write the test first. It must fail. A test that passes immediately proves nothing.

### Step 2: GREEN — Make It Pass

Write the minimum code to make the test pass. Don't over-engineer.

### Step 3: REFACTOR — Clean Up

With tests green, improve the code without changing behavior. Run tests after every refactor step.

## The Prove-It Pattern (Bug Fixes)

When a bug is reported, **do not start by trying to fix it.** Start by writing a test that reproduces it:

```
Bug report → test that demonstrates the bug (FAILS = bug confirmed)
→ implement the fix → test PASSES → full suite green (no regressions)
```

## The Test Pyramid

Most tests small and fast (unit, ~80%), fewer integration (~15%), fewest E2E (~5%, critical paths only).

**The Beyonce Rule:** If you liked it, you should have put a test on it. If a change breaks your code and you didn't have a test for it, that's on you.

**Test sizes:** Small (single process, no I/O, milliseconds) should dominate; Medium (localhost, seconds); Large (E2E, minutes) limited to critical paths.

## Writing Good Tests

- **Test state, not interactions.** Assert outcomes, not internal call sequences — interaction tests break under refactoring.
- **DAMP over DRY in tests.** Each test reads like a specification; controlled duplication is fine.
- **Prefer real implementations over mocks.** Real > fake > stub > mock; mock only at slow, non-deterministic, or side-effecting boundaries.
- **Arrange-Act-Assert.** One concept per test; descriptive names that read like a specification.

## Test Anti-Patterns to Avoid

| Anti-Pattern | Problem | Fix |
|---|---|---|
| Testing implementation details | Breaks under refactoring | Test inputs and outputs |
| Flaky tests | Erode trust | Deterministic assertions, isolated state |
| Snapshot abuse | Unreviewed breakage | Use sparingly, review every change |
| No test isolation | Pass alone, fail together | Each test sets up its own state |
| Mocking everything | Passes while production breaks | Prefer real implementations |

## When to Use Subagents for Testing

For complex bug fixes, a subagent can write the reproduction test without knowledge of the fix, making it more robust.

## Common Rationalizations

| Rationalization | Reality |
|---|---|
| "I'll write tests after the code works" | You won't. And tests written after the fact test implementation, not behavior. |
| "This is too simple to test" | Simple code gets complicated. The test documents the expected behavior. |
| "Tests slow me down" | They slow you down now and speed up every later change. |
| "I tested it manually" | Manual testing doesn't persist. Tomorrow's change might break it silently. |

## Red Flags

- Writing code without any corresponding tests
- Tests that pass on the first run (they may not be testing what you think)
- Bug fixes without reproduction tests
- Test names that don't describe the expected behavior
- Skipping tests to make the suite pass
- Running the same test command twice in a row without any intervening code change

## Verification

After completing any implementation:

- [ ] Every new behavior has a corresponding test
- [ ] The full suite passes with the repository's own test command
- [ ] Bug fixes include a reproduction test that failed before the fix
- [ ] No tests were skipped or disabled
