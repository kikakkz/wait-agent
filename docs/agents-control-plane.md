# WaitAgent Assistant Control Plane

Version: `v1.8`  
Status: `Active`  
Date: `2026-04-30`

## 1. Purpose

This document explains the machine-readable assistant control plane stored in `.agents/`.

It exists to make coding assistants behave more like disciplined project operators:

- Read the current task before exploring widely
- Route work through reusable primitives and runbooks
- Keep machine state aligned with the human execution board
- Avoid drifting into stale local-only assumptions now that resumed network work is active again
- Avoid wasting prompt tokens on backlog, verification, and completed-task history that are not needed for the current slice

This file is human-facing.
The machine-facing control plane lives under `.agents/`.

## 2. Design Principles

The WaitAgent control plane follows these rules:

- Human-readable rationale stays in `docs/`
- Machine-routable task state stays in `.agents/`
- `.agents/tasks/` is the unified task source and must contain the complete machine-readable task inventory
- `.agents/index.yaml` is the single assistant entrypoint
- `docs/execution-status-board.md` remains the human-facing status summary
- `docs/local-acceptance-checklist.md` remains the human-facing acceptance checklist
- After the custom fullscreen baseline was retired, the tmux-first local architecture queue served as the execution gate before resumed network execution
- Now that that gate is closed, resumed network work must stay aligned with the accepted local fixed-chrome baseline and shared transport-agnostic target catalog
- Exact execution state, ordering, blockers, and verification now live in `.agents/`
- Do not create orphan tasks that exist only in chat, scratch notes, or human docs without a matching `.agents/tasks/` entry
- Implementation state and unified task-source state must move together; when code materially changes task completion, scope, or sequencing, the same work slice must update `.agents/tasks/` and any linked `.agents/state/` entries before the task can be considered synced

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
  Current focus, compact default prompt state, verification trail, blocker list, and task history
- `schemas/`
  Lightweight field contracts for the machine-readable files
- `generated/`
  Disposable generated artifacts that should not become source of truth

## 4. Current Default Route

The current default task is `task.t5-08c4d3b`:

> Implement explicit remote mirror open or close protocol messages and server-side session-route ownership.

The default prompt route is now intentionally minimal.

Assistants should load by default:

- `.agents/index.yaml`
- `.agents/tasks/current.yaml`
- `.agents/state/current-focus.yaml`
- `.agents/state/open-blockers.yaml`
- `.agents/state/prompt-context.yaml`

Assistants should not load by default:

- `.agents/tasks/backlog.yaml`
- `.agents/state/last-verified.yaml`
- `.agents/state/task-history.yaml`
- `.agents/state/task-history-archive.yaml`

Those heavier files are now on-demand inputs.
They should be loaded only when task selection, verification refresh, history
review, or a regression investigation actually needs them.

`task-history.yaml` is now a hot near-term history file, not a full project log.
It should keep only one latest-state snapshot per task for the active queue,
its immediate predecessors, and its immediate next tasks.

Older lifecycle transitions and long-tail history belong in
`.agents/state/task-history-archive.yaml`, which should never be part of the
default prompt.

This keeps assistants focused on:

- landing the session-scoped remote live-mirror control slice without paying
  prompt cost for unrelated completed-task history
- preserving the accepted local fixed-chrome activation model while real cross-host remote paths are introduced
- avoiding remote designs that assume server-owned remote PTYs, a second console UX contract, or resurrected auto-switch behavior
- keeping phase-2 work anchored on the current explicit completion queue
  `task.t5-08c4d3b -> task.t5-08c4d3c -> task.t5-08c4d3d`

## 5. Maintenance Rules

Update `.agents/` when any of the following changes:

- the current task changes
- a blocker appears or is resolved
- meaningful validation was run
- the execution board changes phase or milestone emphasis
- a new reusable assistant workflow appears
- a new task is introduced, split, deferred, or reordered
- a human doc was slimmed down and the machine state references need to follow it
- code implementation materially advances, completes, invalidates, or re-scopes a queued task
- hot history grows beyond the near-term queue and needs to be trimmed back into archive

Do not use `.agents/` for:

- long-form product rationale
- architecture prose already captured in `docs/`
- generated logs or temporary scratch notes

## 6. Expected Assistant Behavior

A coding assistant working in this repository should:

1. Read `.agents/index.yaml`
2. Load the current task, current focus, blockers, and compact prompt context
3. Pull backlog, verification, or task history only when the active action actually needs them
4. Use linked primitives and runbooks to choose the next action
5. Update verification, blockers, prompt-context, and task history when execution changes
6. Sync human docs when project-visible status changes

When updating task history:

- keep `task-history.yaml` as a compact near-term snapshot file
- move older repeated lifecycle transitions into `task-history-archive.yaml`
- update `prompt-context.yaml` only when a completed task still shapes the active queue

If `.agents/` and `docs/` disagree, assistants should treat that as maintenance work rather than silently choosing one and continuing.

Hard rule:

- At any moment, the unified task source must reflect the current implementation truth at task granularity.
- Assistants must not leave `.agents/tasks/` claiming `not_started`, `ready`, or `in_progress` for work that is already materially implemented.
- Assistants must not mark a task `done` until implementation, verification evidence, and linked machine state are all synchronized.
