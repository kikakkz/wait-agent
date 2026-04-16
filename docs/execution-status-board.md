# WaitAgent Execution Status Board

Version: `v1.1`  
Status: `Active`  
Date: `2026-04-12`

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

- `T4-10` final local acceptance sign-off for the single-entry workspace UX

Why this is still the gate:

- the local workspace is now the intended default product entrypoint
- tmux-style persistence work and network foundations both exist as follow-on priorities, but neither should pull execution forward until local daily-use trust is established
- any serious local shell, terminal, or auto-switch issue would multiply debugging cost if mirrored-network work resumes too early

## 3. Current Snapshot

Project status at a glance:

- Product, architecture, functional, module, UI, interaction-flow, protocol, and MVP planning docs exist
- The Rust implementation workspace and core local runtime are in place
- Local PTY ownership, console focus, scheduling, Peek, renderer, and VT screen-state handling exist
- Local multi-session workspace behavior has passed the main implementation and live validation loops
- Network foundations up through client/server registration and remote session publication baselines also exist
- The remaining product gate is human sign-off that the local workspace is trustworthy in a real daily-use terminal environment

## 4. Milestone Summary

| Milestone | Goal | Status |
| --- | --- | --- |
| `M0` | Product and design baseline completed | `done` |
| `M1` | Local single-machine workspace UX usable end to end | `in_progress` |
| `M2` | Network aggregation MVP usable end to end | `not_started` |
| `M3` | Hardening, observability, and developer usability | `not_started` |

## 5. Track Summary

Execution tracks at human-summary level:

- `T0` Documentation and planning: complete
- `T1` Local runtime foundation: complete
- `T2` Console interaction and scheduler: complete
- `T3` Terminal UI and rendering: functionally complete for the local gate, with `T3-07` still optional until acceptance evidence says otherwise
- `T4` Local workspace UX and validation: complete through `T4-09`; `T4-10` remains open for final sign-off
- `T5` Network transport and registration: complete through the current foundations; resumed network work now sits behind the queued lifecycle and UI follow-on tasks after local acceptance closes
- `T6` Mirrored multi-console interaction: not started
- `T7` Reliability, security, and diagnostics: not started

## 6. Current Focus And Next Queue

Current focus:

- `T4-10` Validate one-process multi-session workflow through one `waitagent` entrypoint

What remains for `T4-10`:

- rerun the local acceptance checklist in the user's real terminal environment
- confirm the preferred real agent mix still behaves naturally
- decide whether any remaining issue is serious enough to block resumed network work

Next queue once `T4-10` closes:

1. `lifecycle-1` Wrap the current workspace runtime in a daemon-owned PTY envelope
2. `lifecycle-2` Add workspace-local daemon discovery and single-client attach
3. `lifecycle-3` Implement detach, reattach, and resize forwarding for the daemon envelope
4. `lifecycle-4` Validate tmux-style persistence without changing interaction or rendering behavior
5. `sidebar-1` Add a right-side session sidebar menu for future interaction
6. `T5-06` Implement aggregate server session registry
7. `T5-07` Implement remote resize and input routing
8. `T6-01` Implement server-side workspace console
9. `T3-07` Implement narrow-terminal compaction rules if acceptance evidence makes it necessary

The exact machine ordering for that queue now lives in `.agents/tasks/backlog.yaml`.

Deferred refactor queue for later reconsideration:

- `display-1` Separate focused PTY passthrough from WaitAgent chrome rendering
- `display-2` Keep the focused fullscreen TUI on the real terminal size
- `display-3` Collapse screen recovery to one primary restore path
- `runtime-1` Extract a shared console runtime loop for workspace and server surfaces
- `terminal-1` Decide and document the terminal-engine coverage strategy for reliable TUI switching

## 7. Human Sign-Off Notes

Local acceptance should only close when:

- shell-backed sessions still feel like real reusable shell contexts
- Codex-like TUI behavior remains trustworthy inside WaitAgent
- UTF-8 and Chinese input remain readable in practical use
- auto-switch behavior is predictable enough for daily use
- no remaining UX issue is likely to distort subsequent network debugging

## 8. Maintenance Rule

Update this board when:

- the project phase changes
- the current human gate changes
- milestone-level progress changes
- the next queue changes in a way humans need to understand

Do not re-expand this file into a machine task database.
That role now belongs to `.agents/`.
Any task that becomes real work must be represented in `.agents/tasks/`; do not keep orphan tasks only in docs or chat.
