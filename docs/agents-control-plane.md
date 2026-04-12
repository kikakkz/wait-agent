# WaitAgent Assistant Control Plane

Version: `v1.0`  
Status: `Active`  
Date: `2026-04-12`

## 1. Purpose

This document explains the machine-readable assistant control plane stored in `.agents/`.

It exists to make coding assistants behave more like disciplined project operators:

- Read the current task before exploring widely
- Route work through reusable primitives and runbooks
- Keep machine state aligned with the human execution board
- Avoid drifting into premature network work while the local acceptance gate is still open

This file is human-facing.
The machine-facing control plane lives under `.agents/`.

## 2. Design Principles

The WaitAgent control plane follows these rules:

- Human-readable rationale stays in `docs/`
- Machine-routable task state stays in `.agents/`
- `.agents/index.yaml` is the single assistant entrypoint
- `docs/execution-status-board.md` remains the human-facing status summary
- `docs/local-acceptance-checklist.md` remains the human-facing acceptance checklist
- Local workspace acceptance remains the phase gate before resumed network execution
- Exact execution state, ordering, blockers, and verification now live in `.agents/`

## 3. Directory Layout

The control plane is organized as:

```text
.agents/
  index.yaml
  context/
  primitives/
  runbooks/
  tasks/
  state/
  schemas/
  generated/
```

Responsibilities:

- `index.yaml`
  Assistant bootstrap, read order, default task, and recommended runbooks
- `context/`
  Stable project facts, constraints, repo map, and generated-artifact boundaries
- `primitives/`
  Small reusable operating instructions such as reading the current task, syncing acceptance evidence, recording blockers, and refreshing verification
- `runbooks/`
  Multi-step flows such as resuming default work, closing local acceptance, selecting the next task, and maintaining the control plane
- `tasks/`
  Current task, backlog ordering, and reusable task templates
- `state/`
  Current focus, verification trail, blocker list, and task history
- `schemas/`
  Lightweight field contracts for the machine-readable files
- `generated/`
  Disposable generated artifacts that should not become source of truth

## 4. Current Default Route

The current default task is `task.t4-10`:

> Finish local acceptance sign-off for the single-entry workspace UX.

That task intentionally routes assistants through:

- `primitive.acceptance-check-sync`
- `primitive.local-workspace-validation`
- `primitive.verification-refresh`
- `primitive.blocker-record`
- `primitive.task-board-sync`

This keeps assistants focused on:

- real terminal validation
- explicit acceptance evidence
- blocker capture
- consistency between `.agents` and `docs/`

## 5. Maintenance Rules

Update `.agents/` when any of the following changes:

- the current task changes
- a blocker appears or is resolved
- meaningful validation was run
- the execution board changes phase or milestone emphasis
- a new reusable assistant workflow appears
- a human doc was slimmed down and the machine state references need to follow it

Do not use `.agents/` for:

- long-form product rationale
- architecture prose already captured in `docs/`
- generated logs or temporary scratch notes

## 6. Expected Assistant Behavior

A coding assistant working in this repository should:

1. Read `.agents/index.yaml`
2. Load the current task and blockers
3. Use linked primitives and runbooks to choose the next action
4. Update verification, blockers, and task history when execution changes
5. Sync human docs when project-visible status changes

If `.agents/` and `docs/` disagree, assistants should treat that as maintenance work rather than silently choosing one and continuing.
