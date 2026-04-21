# WaitAgent Execution Status Board

Version: `v1.3`  
Status: `Active`  
Date: `2026-04-20`

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

- `display-2c` fullscreen-history acceptance is closed; the broader architecture cleanup remains explicitly deferred rather than silently becoming in-progress work

Why this is the current gate:

- the fullscreen-first display direction has now been accepted against both shell-style sessions and a real Codex-style resume flow, which closes the old dashboard-scroll direction as a product path
- the broader module cleanup debt is real, but it is intentionally parked as deferred work instead of being treated as an unbounded implicit gate
- with the fullscreen acceptance slice closed, the next ready delivery queue returns to the queued network work unless the user explicitly reprioritizes cleanup

## 3. Current Snapshot

Project status at a glance:

- Product, architecture, functional, module, UI, interaction-flow, protocol, and MVP planning docs exist
- The Rust implementation workspace and core local runtime are in place
- Local PTY ownership, console focus, scheduling, Peek, renderer, and VT screen-state handling exist
- Local multi-session workspace behavior has passed the main implementation and live validation loops, and the final local acceptance sign-off has been manually confirmed
- The tmux-style daemon lifecycle queue through `lifecycle-5` now supports detach and reattach persistence, multi-client attach, shared PTY input, and host-wide `waitagent ls` listing
- The right-sidebar prototype remains part of the accepted baseline, but it is no longer the active project gate
- Native fullscreen history now replays normal-screen output from transcript data at the real terminal width, preserves alternate-screen ownership for fullscreen TUIs, and returns cleanly to the dashboard
- The command-bar `/fullscreen` path no longer repaints dashboard chrome over the native fullscreen handoff
- Network foundations up through client/server registration and remote session publication baselines also exist
- The current machine focus is only the deferred architecture-cleanup note; the next ready delivery work is back on the network queue

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
- `T3` Terminal UI and rendering: functionally complete for the local gate, with `T3-07` still optional until acceptance evidence says otherwise
- `T4` Local workspace UX and validation: done, including the final local acceptance sign-off plus the fullscreen-history acceptance closure
- `T5` Network transport and registration: complete through the current foundations; resumed network work is the next ready delivery queue
- `T6` Mirrored multi-console interaction: not started
- `T7` Reliability, security, and diagnostics: not started

## 6. Current Focus And Next Queue

Current focus:

- `architecture-1` Keep the broader runtime and module-boundary cleanup visible as deferred work without implicitly starting it

What remains from the accepted fullscreen rollout:

- keep transcript replay as the authoritative normal-screen fullscreen seed instead of reviving snapshot-copy assumptions
- keep fullscreen handoff native so desktop terminals and remote clients retain their own scrollback, selection, and IME behavior
- keep the accepted dashboard return path stable after `/fullscreen` and `Ctrl-O`
- treat broader module cleanup as a later bounded refactor rather than as permission to reopen the fullscreen direction itself

Accepted display queue:

1. `display-1` Establish the explicit dashboard versus native-fullscreen boundary (`done`)
2. `display-2a` Establish replayable transcript-backed normal-screen history (`done`)
3. `display-2b` Switch fullscreen history seeding and resize redraw to replay (`done`)
4. `display-2c` Validate fullscreen history handoff on shell and Codex-style sessions (`done`)

Next queue after the accepted display rollout:

1. `T5-06` Implement aggregate server session registry
2. `T5-07` Implement remote resize and input routing
3. `T6-01` Implement server-side workspace console
4. `T3-07` Implement narrow-terminal compaction rules if acceptance evidence makes it necessary

The exact machine ordering for that queue now lives in `.agents/tasks/backlog.yaml`.

Deferred refactor queue for later reconsideration:

- `architecture-1` Refactor the runtime and module architecture for clearer boundaries and cleaner code
- `display-3` Collapse screen recovery to one primary restore path
- `runtime-1` Extract a shared console runtime loop for workspace and server surfaces
- `terminal-1` Decide and document the terminal-engine coverage strategy for reliable TUI switching

## 7. Human Sign-Off Notes

Local acceptance is now treated as closed after user-confirmed manual sign-off.

The current local baseline should still preserve:

- shell-backed sessions still feel like real reusable shell contexts
- Codex-like TUI behavior remains trustworthy inside WaitAgent
- native fullscreen old-history remains readable in both shell and Codex-style resume flows
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
