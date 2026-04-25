# WaitAgent Execution Status Board

Version: `v1.7`  
Status: `Active`  
Date: `2026-04-25`

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

- `event-r2a` replace cross-session attach switching with fixed-chrome main-slot target activation

Why this is the current gate:

- the pane-side chrome path is already event-driven enough to expose the real remaining architecture problem
- the same-socket switching path has now been rewritten around tmux-native main-slot rebinding, but the product contract still needs real-terminal acceptance on that new path
- closing this gate is still required before the remaining attach or scheduler cleanup can be called done against the accepted architecture
- attach or scheduler event cleanup is still needed, but it is not the current root blocker
- this work is now the first slice in a locked batch: `event-r2a -> event-r3 -> event-r4`

## 3. Current Snapshot

Project status at a glance:

- product, architecture, functional, module, UI, interaction-flow, protocol, and MVP planning docs exist
- the Rust implementation workspace and core local runtime are in place
- the tmux-first local path already owns the visible workspace chrome
- the accepted new direction is now stricter than the earlier tmux-window switching model: sidebar and footer stay fixed while only the main view changes
- `task.event-r2` is complete: chrome updates, session-catalog refresh, pane refresh, and shell-exit cleanup now use explicit events rather than pane-local polling loops on the accepted path
- `task.event-r2a` is materially advanced: same-socket switching now uses tmux-native pane rebinding, target hosts are modeled separately from the visible workspace chrome session, active-target projection now comes from workspace state instead of the visible chrome session id, workspace lifecycle hooks now refresh only the affected workspace chrome, and startup now materializes the initial target identity before attach
- the remaining `task.event-r2a` work is umbrella acceptance rerun rather than architectural rework: the batch root causes have been addressed and the next step is to clear the remaining fixed-chrome switching evidence in a real terminal before moving to `event-r3`
- future remote work remains deferred until the fixed local chrome and main-slot activation model is stable

## 4. Milestone Summary

| Milestone | Goal | Status |
| --- | --- | --- |
| `M0` | Product and design baseline completed | `done` |
| `M1` | Local single-machine workspace UX usable end to end | `in_progress` |
| `M2` | Network aggregation MVP usable end to end | `not_started` |
| `M3` | Hardening, observability, and developer usability | `not_started` |

## 5. Track Summary

Execution tracks at human-summary level:

- `T0` Documentation and planning: active and aligned with the refined fixed-chrome architecture
- `T1` Local runtime foundation: complete enough for the current architecture correction
- `T2` Event-driven control path: in progress
- `T3` Terminal UI and rendering: the old custom fullscreen and shared-surface path remains retired
- `T4` Local workspace UX and validation: in progress on fixed chrome and main-slot target activation
- `T5` Network transport and registration: foundations exist, but resumed network work remains intentionally deferred
- `T6` Mirrored multi-console interaction: not started
- `T7` Reliability, security, and diagnostics: not started

## 6. Current Focus And Next Queue

Current focus:

- `event-r2a` Rerun umbrella acceptance on the fixed-chrome target-activation model now that ctrl-n hot paths, workspace-local hook refresh, and startup-time initial target materialization are all landed

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
4. `event-r3` Move attach, resize, and scheduler control onto explicit runtime events
5. `event-r4` Route the default local path through the new event-driven stack and isolate polling history

Priority rule:

- `event-r2a -> event-r3 -> event-r4` is the locked top-priority local batch
- no network, remote, or optional local polish task should overtake this batch without an explicit replanning decision

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
