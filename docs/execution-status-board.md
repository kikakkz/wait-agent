# WaitAgent Execution Status Board

Version: `v1.5`  
Status: `Active`  
Date: `2026-04-22`

## 1. Purpose

This document is the human-facing project status snapshot for WaitAgent.

It is intentionally no longer the place for exhaustive machine execution state.
Detailed task routing, blockers, verification history, and reusable assistant procedures now live in `.agents/`.

Use this document for:

- the current phase and why it matters
- the current human decision point
- milestone and track-level progress
- the next queue after the current gate closes

Use `.agents/` for:

- exact current task state
- task backlog ordering
- the complete machine task inventory
- blocker records
- verification records
- reusable assistant primitives and runbooks

## 2. Current Phase

Current phase:

- `Phase 1: Local Workspace MVP`

Current gate:

- `tmux-r2` implement the real vendored tmux control adapter behind the new runtime interfaces

Why this is the current gate:

- repeated real-terminal validation showed that the old custom fullscreen path is structurally unstable for Codex-like live TUI workloads
- the accepted replacement is now the tmux-first workspace architecture documented in `docs/tmux-first-workspace-plan.md`
- the current code structure is also not a good base for this migration, especially because `src/app.rs` concentrates too many responsibilities
- the new code-level target is documented in `docs/tmux-first-runtime-architecture.md`

## 3. Current Snapshot

Project status at a glance:

- Product, architecture, functional, module, UI, interaction-flow, protocol, and MVP planning docs exist
- The Rust implementation workspace and core local runtime are in place
- Local PTY ownership, console focus, scheduling, Peek, renderer, and VT screen-state handling exist
- Local multi-session workspace behavior has passed the original custom-runtime implementation and live validation loops, but that display baseline is no longer the accepted architecture target
- The tmux-style daemon lifecycle queue through `lifecycle-5` now supports detach and reattach persistence, multi-client attach, shared PTY input, and host-wide `waitagent ls` listing
- The right-sidebar and bottom menu remain accepted product requirements, but their implementation home is now tmux-native panes rather than shared-surface composition
- The old custom `native fullscreen` and `live surface` direction is formally retired as the target baseline
- The current accepted local direction is `tmux-first`, with one session per tmux window, fullscreen implemented as pane zoom, and tmux vendored as a pinned backend rather than required as a system dependency
- Network foundations up through client/server registration and remote session publication baselines also exist
- The current machine focus is `tmux-r2`, while resumed network expansion is explicitly deferred until the tmux-first local base is stable

## 4. Milestone Summary

| Milestone | Goal | Status |
| --- | --- | --- |
| `M0` | Product and design baseline completed | `done` |
| `M1` | Local single-machine workspace UX usable end to end | `done` |
| `M2` | Network aggregation MVP usable end to end | `not_started` |
| `M3` | Hardening, observability, and developer usability | `not_started` |

## 5. Track Summary

Execution tracks at human-summary level:

- `T0` Documentation and planning: complete
- `T1` Local runtime foundation: complete
- `T2` Console interaction and scheduler: complete
- `T3` Terminal UI and rendering: partially complete, but the old custom fullscreen/render path is no longer the accepted steady-state architecture
- `T4` Local workspace UX and validation: reopened on a bounded architecture pivot to tmux-first workspace delivery
- `T5` Network transport and registration: complete through the current foundations; resumed network work is intentionally deferred behind tmux-first local stabilization
- `T6` Mirrored multi-console interaction: not started
- `T7` Reliability, security, and diagnostics: not started

## 6. Current Focus And Next Queue

Current focus:

- `tmux-r2` Implement the real vendored tmux control adapter behind the new runtime interfaces

Accepted local architecture direction:

- main interactive sessions run in real tmux panes
- sidebar and footer become dedicated tmux panes
- fullscreen becomes tmux pane zoom
- fullscreen scrollback uses tmux-native history instead of WaitAgent-local replay
- one session maps to one tmux window
- multiple waitagent instances are allowed when separate tmux-backed workspaces are the simpler model

Accepted tmux-first delivery queue:

1. `tmux-0` Establish the new modular runtime architecture, unified entry model, and migration skeleton
2. `tmux-r1` Re-baseline the migration around a clean rewrite boundary and freeze the invalid bridge path
3. `tmux-r2` Implement the real vendored tmux control adapter behind the new runtime interfaces
4. `tmux-r3` Build a new workspace lifecycle stack outside the legacy lifecycle module
5. `tmux-r4` Route workspace-facing commands through the new tmux-first lifecycle stack
6. `tmux-r5` Implement persistent sidebar and footer panes as tmux-owned UI surfaces
7. `tmux-r6` Implement tmux-native fullscreen zoom and fullscreen-only scrollback
8. `tmux-r7` Move session switching and sidebar focus semantics onto tmux-native control
9. `tmux-r8` Remove the obsolete custom local display path and complete shell plus Codex acceptance

Deferred queue after local tmux-first stabilization:

1. `T5-06` Implement aggregate server session registry
2. `T5-07` Implement remote resize and input routing
3. `T6-01` Implement server-side workspace console
4. `T3-07` Implement narrow-terminal compaction rules if acceptance evidence makes it necessary

The exact machine ordering for that queue now lives in `.agents/tasks/backlog.yaml`.

Retired or absorbed queue:

- `display-1`, `display-2`, `display-2a`, `display-2b`, and `display-2c` are now historical slices from the retired custom fullscreen direction
- `display-3`, `runtime-1`, and `terminal-1` are absorbed by the tmux-first migration and are no longer the preferred cleanup path
- `scroll-1` remains retired
- the original `tmux-1` through `tmux-6` execution split is retained as historical planning only and is superseded by the rewrite queue above

## 7. Human Sign-Off Notes

The prior local acceptance sign-off is preserved as historical evidence for the old custom runtime, but it is not treated as final sign-off for the tmux-first architecture.

The current local product contract that must survive the migration is:

- shell-backed sessions still feel like real reusable shell contexts
- Codex-like TUI behavior remains trustworthy inside WaitAgent
- sidebar and menu remain first-class workspace controls
- fullscreen still exists and behaves like a real terminal view
- UTF-8 and Chinese input remain readable in practical use
- auto-switch behavior is predictable enough for daily use
- the local display architecture should stop generating bugs that would distort later network debugging

## 8. Maintenance Rule

Update this board when:

- the project phase changes
- the current human gate changes
- milestone-level progress changes
- the next queue changes in a way humans need to understand

Do not re-expand this file into a machine task database.
That role now belongs to `.agents/`.
Any task that becomes real work must be represented in `.agents/tasks/`; do not keep orphan tasks only in docs or chat.
