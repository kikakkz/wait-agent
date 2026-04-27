# WaitAgent Execution Status Board

Version: `v1.8`  
Status: `Active`  
Date: `2026-04-27`

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
- reusable assistant procedures

## 2. Current Phase

Current phase:

- `Phase 1: Local Workspace MVP`

Current gate:

- `local-cleanup` close local legacy cleanup and reset the remote design baseline

Why this is the current gate:

- the local tmux-native workspace path is now usable enough to end the current acceptance phase
- the most important remaining risk is no longer local interaction breakage, but stale legacy assumptions in code and docs
- remote session work should restart from the cleaned local architecture rather than from deleted network-era surfaces

## 3. Current Snapshot

Project status at a glance:

- product, architecture, functional, module, UI, interaction-flow, protocol, and MVP planning docs exist
- the Rust implementation workspace and core local runtime are in place
- the tmux-first local path already owns the visible workspace chrome
- the accepted new direction is now stricter than the earlier tmux-window switching model: sidebar and footer stay fixed while only the main view changes
- `task.event-r2` is complete: chrome updates, session-catalog refresh, pane refresh, and shell-exit cleanup now use explicit events rather than pane-local polling loops on the accepted path
- `task.event-r2a` is now accepted for the local product goal: same-socket switching uses tmux-native pane rebinding, target hosts are modeled separately from the visible workspace chrome session, active-target projection comes from workspace state instead of the visible chrome session id, workspace lifecycle hooks refresh only the affected workspace chrome, startup materializes the initial target identity before attach, and real-terminal sidebar or footer switching keeps the fixed chrome mounted
- local acceptance is no longer blocked on deleted legacy interaction features, because they are not part of the accepted current product scope
- `task.event-r3` is now the active local gate: move remaining attach, resize, and scheduler control off accepted-path tick or timeout coordination and onto explicit runtime events
- future remote work remains deferred until the fixed local chrome and main-slot activation model is stable and the remaining attach-side event cleanup lands

## 4. Milestone Summary

| Milestone | Goal | Status |
| --- | --- | --- |
| `M0` | Product and design baseline completed | `done` |
| `M1` | Local single-machine workspace UX usable end to end | `done` |
| `M2` | Network aggregation MVP usable end to end | `not_started` |
| `M3` | Hardening, observability, and developer usability | `not_started` |

## 5. Track Summary

Execution tracks at human-summary level:

- `T0` Documentation and planning: active and aligned with the refined fixed-chrome architecture
- `T1` Local runtime foundation: complete enough for the current architecture correction
- `T2` Event-driven control path: complete enough for the accepted local scope
- `T3` Terminal UI and rendering: the old custom fullscreen and shared-surface path remains retired
- `T4` Local workspace UX and validation: complete enough for the current local scope
- `T5` Network transport and registration: foundations exist, but resumed network work remains intentionally deferred
- `T6` Mirrored multi-console interaction: not started
- `T7` Reliability, security, and diagnostics: not started

## 6. Current Focus And Next Queue

Current focus:

- execute `task.event-r3` on the accepted local path, then close `task.event-r4` before remote session design resumes

Accepted local architecture direction:

- one persistent workspace chrome with fixed sidebar, fixed main slot, and fixed footer or menu
- selecting a sidebar or footer item rebinds only the main slot target
- in-workspace switching must not detach the current client, reveal the shell, or rebuild the workspace chrome
- local targets live inside one tmux backend and switch through tmux-native rebinding primitives rather than by launching a fresh attach command
- future remote targets must fit the same transport-agnostic target catalog and render into the same main slot through a bridge runtime
- `waitagent` or `workspace` may bootstrap a backend; `waitagent attach` joins an existing backend only

Accepted event-driven delivery queue:

1. `event-r1` Establish the new event-driven local runtime architecture and event contract
2. `event-r2` Implement event-driven tmux chrome, session catalog, and pane update flows
3. `event-r2a` Replace cross-session attach switching with fixed-chrome main-slot target activation
4. `event-r3` Move remaining attach, resize, and lifecycle control onto explicit runtime events
5. `event-r4` Route the default local path through the new event-driven stack and isolate polling history only if future remote design still needs that split

Priority rule:

- no deleted legacy surface should be revived during remote planning
- remote and local session management should be redesigned on top of the cleaned tmux-native workspace baseline

Deferred queue after local stabilization:

1. `T5-06` Implement the aggregate transport-agnostic target registry
2. `T5-07` Implement remote target input and resize routing through the server control plane
3. `T6-01` Implement the server-side workspace console as a target-activation surface
4. `T3-07` Implement narrow-terminal compaction rules for the fixed-chrome workspace layout if acceptance evidence makes it necessary

The exact machine ordering for that queue lives in `.agents/tasks/backlog.yaml`.

## 7. Human Sign-Off Notes

The local product contract that must survive the migration is:

- shell-backed sessions still feel like real reusable shell contexts
- Codex-like TUI behavior remains trustworthy inside WaitAgent
- sidebar and menu remain first-class workspace controls
- sidebar and menu stay mounted while switching targets in normal mode
- fullscreen still exists and behaves like a real terminal view
- UTF-8 and Chinese input remain readable in practical use
- the local display architecture should stop generating chrome-switch artifacts that would distort later network debugging

## 8. Maintenance Rule

Update this board when:

- the project phase changes
- the current human gate changes
- milestone-level progress changes
- the next queue changes in a way humans need to understand

Do not re-expand this file into a machine task database.
That role belongs to `.agents/`.
Any task that becomes real work must be represented in `.agents/tasks/`; do not keep orphan tasks only in docs or chat.
