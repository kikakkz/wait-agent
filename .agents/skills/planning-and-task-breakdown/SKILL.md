---
name: planning-and-task-breakdown
description: Breaks work into ordered tasks. Use when you have a spec or clear requirements and need to break work into implementable tasks. Use when a task feels too large to start, when you need to estimate scope, or when parallel work is possible.
---

<!--
  Vendored from addyosmani/agent-skills (MIT) at revision
  bcab6a1b8503100e8618c3b4e32cc78de43de769, upstream path
  skills/planning-and-task-breakdown/SKILL.md.
  Do not edit the content; update by re-vendoring a newer revision.
  Adaptation (per the skill's own "Task List Target" section): this
  repository designates GitHub issues as the task list target (AD-0001),
  so the tasks/plan.md + tasks/todo.md convention below does not apply —
  create one tracker issue per task instead.
-->

# Planning and Task Breakdown

## Overview

Decompose work into small, verifiable tasks with explicit acceptance criteria. Good task breakdown is the difference between an agent that completes work reliably and one that produces a tangled mess. Every task should be small enough to implement, test, and verify in a single focused session.

## When to Use

- You have a spec and need to break it into implementable units
- A task feels too large or vague to start
- Work needs to be parallelized across multiple agents or sessions
- You need to communicate scope to a human
- The implementation order isn't obvious

**When NOT to use:** Single-file changes with obvious scope, or when the spec already contains well-defined tasks.

## The Planning Process

### Step 1: Plan Before Writing Code

Operate in read-only mode: read the spec and relevant code, identify existing patterns, map dependencies, note risks and unknowns. The output is a plan posted to the tracking issue, not implementation. (This repository: post the plan as an issue comment before implementation, per AGENTS.md collaboration protocol.)

### Step 2: Identify the Dependency Graph

Map what depends on what; implementation order follows the graph bottom-up — foundations first.

### Step 3: Slice Vertically

Build one complete feature path at a time, not layer by layer. Each vertical slice delivers working, testable functionality.

### Step 4: Write Tasks

Each task carries: a short title, a one-paragraph description, acceptance criteria (specific, testable), verification steps (the repository's own commands), dependencies, and files likely touched.

**In this repository, each task is a GitHub issue** using the task template (context, design ref, depends on, acceptance criteria, verification). Umbrella issues list slices; each slice gets its own issue before any code exists.

### Step 5: Order and Checkpoint

Dependencies satisfied first; each task leaves the system working; verification checkpoints after every 2-3 tasks; high-risk tasks early (fail fast).

## Task Sizing Guidelines

| Size | Files | Scope | Example |
|------|-------|-------|---------|
| **XS** | 1 | Single function or config change | Add a validation rule |
| **S** | 1-2 | One component or endpoint | Add a new API endpoint |
| **M** | 3-5 | One feature slice | User registration flow |
| **L** | 5-8 | Multi-component feature | Search with filtering and pagination |
| **XL** | 8+ | **Too large — break it down further** | — |

If a task is L or larger, break it down further. An agent performs best on S and M tasks.

**When to break a task down further:**
- It would take more than one focused session (roughly 2+ hours of agent work)
- You cannot describe the acceptance criteria in 3 or fewer bullet points
- It touches two or more independent subsystems
- You find yourself writing "and" in the task title (a sign it is two tasks)

## Parallelization Opportunities

- **Safe to parallelize:** Independent feature slices, tests for already-implemented features, documentation
- **Must be sequential:** Shared state changes, dependency chains
- **Needs coordination:** Features that share a contract (define the contract first, then parallelize)

## Common Rationalizations

| Rationalization | Reality |
|---|---|
| "I'll figure it out as I go" | That's how you end up with a tangled mess and rework. 10 minutes of planning saves hours. |
| "The tasks are obvious" | Write them down anyway. Explicit tasks surface hidden dependencies and forgotten edge cases. |
| "Planning is overhead" | Planning is the task. Implementation without a plan is just typing. |
| "I can hold it all in my head" | Context windows are finite. Written plans survive session boundaries and compaction. |

## Red Flags

- Starting implementation without a written task list
- Tasks that say "implement the feature" without acceptance criteria
- No verification steps in the plan
- All tasks are XL-sized
- No checkpoints between tasks
- Dependency order isn't considered

## Verification

Before starting implementation, confirm:

- [ ] Every task has acceptance criteria
- [ ] Every task has a verification step
- [ ] Task dependencies are identified and ordered correctly
- [ ] Tasks are recorded as GitHub issues (AD-0001)
- [ ] No task touches more than ~5 files
- [ ] Checkpoints exist between major phases
- [ ] The human has reviewed and approved the plan
