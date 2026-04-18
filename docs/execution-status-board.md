# WaitAgent Execution Status Board

Version: `v1.3`  
Status: `Active`  
Date: `2026-04-17`

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

- `sidebar-4` validate and polish the first sidebar prototype after the default-visible collapsible detail-area rollout

Why this is the current gate:

- local acceptance has been signed off, so the next bounded local UX slice can proceed without reopening the old acceptance gate
- the first sidebar prototype now already covers unified focus navigation, one-line rows, wait-state badges, `Enter`-to-switch, a dedicated bottom detail area, and a default-visible collapsible affordance, so the remaining bounded slice is to stabilize and validate that layout without reopening the stabilized rendering path
- mirrored-network work still remains behind the current post-acceptance local UX follow-on queue

## 3. Current Snapshot

Project status at a glance:

- Product, architecture, functional, module, UI, interaction-flow, protocol, and MVP planning docs exist
- The Rust implementation workspace and core local runtime are in place
- Local PTY ownership, console focus, scheduling, Peek, renderer, and VT screen-state handling exist
- Local multi-session workspace behavior has passed the main implementation and live validation loops, and the final local acceptance sign-off has been manually confirmed
- The tmux-style daemon lifecycle queue through `lifecycle-5` now supports detach and reattach persistence, multi-client attach, shared PTY input, and host-wide `waitagent ls` listing
- The right-sidebar prototype now has unified focus navigation, one-line session rows with labels and wait-state badges, `Enter`-to-switch behavior, a default-visible collapsible rail, and a dedicated bottom detail row for the selected session path
- The remaining local UX follow-on in this rollout is validation and polish for long-path presentation and layout stability rather than a new sidebar interaction model
- Network foundations up through client/server registration and remote session publication baselines also exist
- The current focus is no longer acceptance sign-off; it is the stabilization slice for the first sidebar prototype

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
- `T4` Local workspace UX and validation: done, including the final local acceptance sign-off
- `T5` Network transport and registration: complete through the current foundations; resumed network work now sits behind the queued lifecycle and UI follow-on tasks after local acceptance closes
- `T6` Mirrored multi-console interaction: not started
- `T7` Reliability, security, and diagnostics: not started

## 6. Current Focus And Next Queue

Current focus:

- `sidebar-4` Validate and polish the default-visible collapsible sidebar while preserving the working prototype

What remains for the current sidebar rollout:

- keep the new unified `Right`/`Left`/`Up`/`Down` sidebar navigation intact
- keep the one-line session rows, detected labels, and `INPUT` or `UNKNOWN` badges intact
- keep the selected-session path/detail in a dedicated fixed sidebar detail area above the bottom info bar
- keep the sidebar default-visible when width allows, with a hide path that still leaves a visible collapsed affordance on the right edge
- keep the bottom info bar full-width and visually independent from sidebar rendering
- validate whether long-path presentation needs more polish after real use
- continue to keep future automation rules explicitly out of scope for this rollout

Current sidebar queue:

1. `sidebar-1` Finalize the v1 sidebar contract and rollout split
2. `sidebar-2` Implement unified sidebar focus navigation (`done`)
3. `sidebar-3` Render one-line session rows with labels and wait-state badges (`done`)
4. `sidebar-4` Polish the default-visible sidebar detail area and `Enter`-to-switch behavior (`in_progress`)

Next queue after the current sidebar rollout:

1. `T5-06` Implement aggregate server session registry
2. `T5-07` Implement remote resize and input routing
3. `T6-01` Implement server-side workspace console
4. `T3-07` Implement narrow-terminal compaction rules if acceptance evidence makes it necessary

The exact machine ordering for that queue now lives in `.agents/tasks/backlog.yaml`.

Deferred refactor queue for later reconsideration:

- `display-1` Separate focused PTY passthrough from WaitAgent chrome rendering
- `display-2` Keep the focused fullscreen TUI on the real terminal size
- `display-3` Collapse screen recovery to one primary restore path
- `runtime-1` Extract a shared console runtime loop for workspace and server surfaces
- `terminal-1` Decide and document the terminal-engine coverage strategy for reliable TUI switching

## 7. Human Sign-Off Notes

Local acceptance is now treated as closed after user-confirmed manual sign-off.

The current local UX follow-on should still preserve:

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
