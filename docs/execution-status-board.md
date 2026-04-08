# WaitAgent Execution Status Board

Version: `v1.0`  
Status: `Active`  
Date: `2026-04-07`

## 1. Purpose

This document is the working task board for WaitAgent.

It serves as:

- The baseline for implementation planning
- The source of truth for development status
- The dependency map between design, implementation, and validation work

It should be updated whenever:

- A task changes state
- Scope changes
- A milestone is completed
- A blocker appears or is removed

## 2. Status Legend

Use one of the following statuses for every task:

- `done`
- `in_progress`
- `ready`
- `blocked`
- `not_started`

Status meaning:

- `done`
  Completed and accepted for the current phase
- `in_progress`
  Actively being worked on
- `ready`
  Fully specified and unblocked, but not started
- `blocked`
  Cannot move until a dependency or decision is resolved
- `not_started`
  Not yet prepared for execution

## 3. Current Snapshot

Current project state:

- Product definition is documented
- Architecture is documented
- Functional design is documented
- Module design is documented
- UI design is documented
- Interaction flows are documented
- Protocol design is documented
- MVP execution plan is documented
- Rust implementation workspace exists
- Command and config skeleton exists
- Core session and console domain skeletons exist
- Local PTY-backed process spawn and ownership model exist
- Terminal raw-mode and size-capture foundation exist
- Console focus and Peek runtime state exist
- Waiting heuristic and FIFO waiting queue foundations exist
- Auto-switch and continuation-protection state machine foundations exist
- Explicit read-only Peek interaction path exists
- VT screen-state engine foundation exists
- Session screen snapshot storage exists
- Focused renderer and minimal status-line chrome exist
- Focus-restore render path exists
- Explicit Peek read-only render path exists
- TTY local run flow now enters the console event loop
- Scheduler rule coverage now includes deterministic local validation cases
- PTY IO round-trip and renderer frame snapshot coverage exist
- Local server hosting can now accept delegated local session spawns
- Manual three-session local workflow validation has passed
- Local host exposes focus switching, input protection, auto-switch, and Peek controls
- Internal event bus baseline exists
- Executable transport schema and protocol version baseline exist
- Server runtime skeleton exists and now owns listener acceptance
- Client runtime skeleton exists and now owns connect-side delegation, hello preparation, and heartbeat preparation
- Node registration and liveness baseline now exist across client and server runtimes
- Remote session publication baseline now exists across transport, client publication helpers, and server-side publication intake events
- The target product UX has been revised to a workspace-first model where one `waitagent` manages multiple local sessions
- The current public CLI still exposes a temporary bridge model centered on `run` and `server`
- Network foundations remain reusable, but they no longer define the next user-facing milestone by themselves

Current phase:

- `Phase 1: Local Workspace MVP`

## 4. Milestones

| Milestone | Goal | Status |
| --- | --- | --- |
| `M0` | Product and design baseline completed | `done` |
| `M1` | Local single-machine workspace UX usable end to end | `in_progress` |
| `M2` | Network aggregation MVP usable end to end | `not_started` |
| `M3` | Hardening, observability, and developer usability | `not_started` |

## 5. Board Structure

The board is split into the following execution tracks:

- `T0` Documentation and planning
- `T1` Local runtime foundation
- `T2` Console interaction and scheduler
- `T3` Terminal UI and rendering
- `T4` Local workspace UX and validation
- `T5` Network transport and registration
- `T6` Mirrored multi-console interaction
- `T7` Reliability, security, and diagnostics

## 6. Task Board

## 6.1 T0 Documentation and Planning

| ID | Task | Depends On | Status | Notes |
| --- | --- | --- | --- | --- |
| `T0-01` | Finalize PRD baseline | None | `done` | [wait-agent-prd.md](wait-agent-prd.md) |
| `T0-02` | Finalize architecture baseline | `T0-01` | `done` | [architecture.md](architecture.md) |
| `T0-03` | Finalize functional design baseline | `T0-01` | `done` | [functional-design.md](functional-design.md) |
| `T0-04` | Finalize module design baseline | `T0-02` | `done` | [module-design.md](module-design.md) |
| `T0-05` | Finalize UI design baseline | `T0-03` | `done` | [ui-design.md](ui-design.md) |
| `T0-06` | Finalize interaction flows baseline | `T0-03` | `done` | [interaction-flows.md](interaction-flows.md) |
| `T0-07` | Define wire protocol document | `T0-02`, `T0-04` | `done` | [protocol.md](protocol.md) |
| `T0-08` | Define MVP execution plan | `T0-02`, `T0-03`, `T0-04` | `done` | [mvp-plan.md](mvp-plan.md) |
| `T0-09` | Keep execution board current | `T0-01` | `in_progress` | This document |

## 6.2 T1 Local Runtime Foundation

| ID | Task | Depends On | Status | Notes |
| --- | --- | --- | --- | --- |
| `T1-01` | Initialize implementation workspace and crate structure | `T0-04` | `done` | Rust crate and base source layout exist |
| `T1-02` | Implement base config loading and app bootstrap | `T1-01` | `done` | Local-first bootstrap and command dispatch exist |
| `T1-03` | Implement session registry core types | `T1-01` | `done` | Addressing, status, registry, local session creation, and lifecycle updates exist |
| `T1-04` | Implement PTY manager spawn and ownership model | `T1-01` | `done` | Local PTY spawn, ownership mapping, resize API, and exit tracking exist |
| `T1-05` | Implement internal event bus | `T1-01` | `done` | `src/event.rs` now provides grouped runtime event envelopes, fan-out subscriptions, and ordered in-process publication for local/network unification |
| `T1-06` | Implement terminal raw mode and resize capture | `T1-01` | `done` | Terminal module, raw-mode guard, size snapshot, and resize tracker exist |

## 6.3 T2 Console Interaction and Scheduler

| ID | Task | Depends On | Status | Notes |
| --- | --- | --- | --- | --- |
| `T2-01` | Implement console runtime state | `T1-02`, `T1-03` | `done` | Focus, Peek, attach selection, manual switching, and focus loss handling exist |
| `T2-02` | Implement manual focus switching | `T2-01`, `T1-03` | `done` | Next, previous, index, and direct-target switching exist in console state |
| `T2-03` | Implement typing-state protection | `T2-01` | `done` | Manual focus switching is blocked during partial input |
| `T2-04` | Implement waiting heuristic engine | `T1-03`, `T1-04` | `done` | Non-semantic waiting classification exists for running, waiting, idle, and exited sessions |
| `T2-05` | Implement waiting queue management | `T2-04` | `done` | FIFO queue sync preserves first-wait order and removes resumed sessions |
| `T2-06` | Implement auto-switch state machine | `T2-03`, `T2-05` | `done` | One-enter one-switch decision flow and switch lock behavior exist in scheduler state |
| `T2-07` | Implement continuation protection | `T2-06` | `done` | Current-session continuation keeps focus until the round stabilizes |
| `T2-08` | Implement Peek mode | `T2-01` | `done` | Explicit read-only Peek entry, exit, rendered-session, and input-owner behavior exist |

## 6.4 T3 Terminal UI and Rendering

| ID | Task | Depends On | Status | Notes |
| --- | --- | --- | --- | --- |
| `T3-01` | Integrate VT screen state engine | `T1-04` | `done` | Terminal engine reconstructs screen state, cursor, alternate screen, and scrollback from PTY bytes |
| `T3-02` | Implement session screen snapshot storage | `T3-01`, `T1-03` | `done` | Sessions store canonical screen state and snapshot versions for focus restore |
| `T3-03` | Implement focused session renderer | `T2-01`, `T3-02` | `done` | Renderer composes the focused or peeked session snapshot into a main viewport frame |
| `T3-04` | Implement minimal top and bottom status lines | `T3-03` | `done` | Renderer outputs minimal top and bottom status lines for normal and peek states |
| `T3-05` | Implement focus restore on switch | `T3-02`, `T2-02` | `done` | Renderer restores the target snapshot immediately after focus changes with a short restore notice |
| `T3-06` | Implement Peek rendering path | `T2-08`, `T3-02` | `done` | Dedicated read-only render mode now distinguishes viewport ownership from input ownership |
| `T3-07` | Implement narrow terminal compaction rules | `T3-04` | `not_started` | Optional in local MVP, required before hardening |

## 6.5 T4 Local Workspace UX and Validation

| ID | Task | Depends On | Status | Notes |
| --- | --- | --- | --- | --- |
| `T4-01` | Build end-to-end local interactive run flow | `T1-06`, `T2-01`, `T3-03` | `done` | `run` now enters raw mode on TTY, forwards stdin to the focused PTY, renders snapshots, handles resize, and ticks the scheduler locally |
| `T4-02` | Add scheduler unit tests | `T2-06`, `T2-07` | `done` | Deterministic tests now cover waiting classification, FIFO queue behavior, one-enter one-switch, continuation protection, manual reset, and focused-only waiting edge cases |
| `T4-03` | Add PTY integration tests | `T1-04` | `done` | PTY tests now cover spawn, resize, exit, and stdin-to-stdout round-trip behavior |
| `T4-04` | Add renderer snapshot tests | `T3-03`, `T3-05`, `T3-06` | `done` | Renderer tests now assert full focused and Peek frame snapshots in addition to field-level behavior |
| `T4-05` | Validate three-session local workflow manually | `T4-01`, `T4-02`, `T4-03`, `T4-04` | `done` | Manual validation passed for three delegated local sessions, focus switching, typing protection, auto-switch, and host Peek behavior |
| `T4-06` | Fix local MVP defects and stabilize | `T4-05` | `done` | Fixed the continuation-observation race that caused premature auto-switch before current-session follow-up output rendered |
| `T4-07` | Implement single-entry workspace shell bootstrap | `T1-02`, `T2-01`, `T3-03` | `done` | The workspace-first local entrypoint is live and `waitagent` is now the primary local interaction surface |
| `T4-08` | Implement in-workspace session creation and background lifecycle | `T4-07`, `T1-03`, `T1-04` | `done` | Sessions can be created, focused, backgrounded, listed, and closed from inside the workspace shell |
| `T4-09` | Implement shell-backed session defaults and working-directory handling | `T4-08` | `done` | Managed sessions default to reusable shell contexts and surface working-directory updates in the UI |
| `T4-10` | Validate one-process multi-session workflow through one `waitagent` entrypoint | `T4-08`, `T4-09` | `in_progress` | Real-agent acceptance is underway; use [local-acceptance-checklist.md](local-acceptance-checklist.md) as the gate, with auto-switch still requiring more validation in real workflows |

## 6.6 T5 Network Transport and Registration

| ID | Task | Depends On | Status | Notes |
| --- | --- | --- | --- | --- |
| `T5-01` | Define protocol schema and versioning | `T0-07` | `done` | `src/transport.rs` now defines the executable protocol envelope, MVP payload schema, and version validation baseline |
| `T5-02` | Implement server runtime skeleton | `T1-05`, `T5-01` | `done` | `src/server.rs` now owns nonblocking listener bind and connection acceptance, and the local host runs through that server runtime boundary |
| `T5-03` | Implement client runtime skeleton | `T1-05`, `T5-01` | `done` | `src/client.rs` now owns endpoint normalization, connect-side runtime identity, hello and heartbeat envelope preparation, event publication, and the temporary bridge that will later be refit to workspace-owned sessions |
| `T5-04` | Implement node registration and liveness | `T5-02`, `T5-03` | `done` | Client registration now exchanges `ClientHello` and `Heartbeat` over the transport envelope codec, while `src/server.rs` tracks node online/offline state through a node registry and heartbeat timeout |
| `T5-05` | Implement remote session publication | `T5-03`, `T1-03` | `done` | `src/client.rs` now prepares and can publish `SessionStarted`, `SessionUpdated`, and `SessionExited` envelopes, while `src/server.rs` accepts those messages as runtime publication events |
| `T5-06` | Implement aggregate server session registry | `T5-02`, `T5-05` | `ready` | Cross-node visibility remains valid, but it is no longer the next user-facing priority before `T4-07` through `T4-10` land |
| `T5-07` | Implement remote resize and input routing | `T5-02`, `T5-03`, `T1-04` | `not_started` | PTY host remains authoritative |

## 6.7 T6 Mirrored Multi-Console Interaction

| ID | Task | Depends On | Status | Notes |
| --- | --- | --- | --- | --- |
| `T6-01` | Implement server-side workspace console | `T5-06`, `T3-03` | `not_started` | Aggregate interaction surface |
| `T6-02` | Implement mirrored output broadcast | `T5-06`, `T5-07` | `not_started` | Same session visible in many consoles |
| `T6-03` | Implement mirrored input propagation | `T5-07`, `T2-01` | `not_started` | Local and server input share PTY |
| `T6-04` | Implement server-side waiting queue | `T6-01`, `T2-04`, `T2-05` | `not_started` | Per-console scheduler on server |
| `T6-05` | Implement multi-console attach awareness UI | `T6-02`, `T3-04` | `not_started` | `attached: 2`, `remote typing` |
| `T6-06` | Validate mirrored local/server workflow end to end | `T6-01`, `T6-02`, `T6-03`, `T6-04` | `not_started` | M2 gate candidate |

## 6.8 T7 Reliability, Security, and Diagnostics

| ID | Task | Depends On | Status | Notes |
| --- | --- | --- | --- | --- |
| `T7-01` | Implement reconnect and session identity recovery | `T5-04`, `T5-05` | `not_started` | Preserve session identity on reconnect |
| `T7-02` | Implement offline node handling in UI and scheduler | `T6-01`, `T7-01` | `not_started` | Unreachable session handling |
| `T7-03` | Implement basic authentication and enrollment | `T5-02`, `T5-03` | `not_started` | Minimum viable security |
| `T7-04` | Implement structured logs and event tracing | `T1-05` | `not_started` | Needed for debugging |
| `T7-05` | Implement debug status views | `T7-04`, `T1-03`, `T2-06` | `not_started` | Session and scheduler inspection |
| `T7-06` | Add network reconnect and replay tests | `T7-01`, `T6-02`, `T6-03` | `not_started` | M3 gate candidate |

## 7. Phase Plan

### 7.1 Phase 0: Design Baseline

Scope:

- Product and system design documents
- Execution board

Exit criteria:

- PRD is stable enough to implement against
- Architecture, functional, module, UI, and flow docs exist
- Execution board exists and names the next implementation tasks

Status:

- `done`

### 7.2 Phase 1: Local Workspace MVP

Scope:

- Local PTY runtime
- Local console runtime
- Local scheduler
- Minimal terminal UI
- Focus switch, Peek, and waiting heuristics
- Single-entry workspace shell UX
- In-workspace session creation and reusable session contexts

Exit criteria:

- The user starts one `waitagent` and can manage multiple local sessions inside it
- One-enter one-switch behavior works
- Peek works
- No switching occurs during partial input

Status:

- `in_progress`

### 7.3 Phase 2: Network MVP

Scope:

- Server/client transport
- Node registration
- Remote session visibility
- Mirrored interaction between local and server consoles

Exit criteria:

- A client node can register sessions with the server
- The same session can be interacted with from local CLI and server console
- Output is mirrored across attached consoles
- Server-side scheduler can switch among remote sessions

Status:

- `not_started`

### 7.4 Phase 3: Hardening

Scope:

- Reconnect
- Offline handling
- Authentication
- Diagnostics
- Broader test coverage

Exit criteria:

- Network disruptions do not collapse local usability
- Session identity survives reconnect where possible
- Basic authenticated deployment works
- Debugging tools exist for session and scheduler state

Status:

- `not_started`

## 8. Current Blockers

Current blockers:

- No hard architecture blocker, but local workspace acceptance is still open
- Auto-switch behavior needs more real-workflow tuning before `M1` can be called done

## 9. Recommended Next Actions

Recommended immediate sequence:

1. Finish `T4-10` and use it as the local acceptance gate for the product UX
2. Refine auto-switch behavior and other remaining real-agent terminal fidelity gaps discovered during `T4-10`
3. Resume `T5-06` and `T6-01` only after the workspace-first local model is accepted as the supported default
4. Reuse the existing transport, client, and server foundations rather than replacing them

## 10. Update Rules

When updating this board:

- Change task status in place
- Add one short note when a task becomes `blocked`
- Move milestone status when its exit criteria are met
- Do not delete completed tasks; preserve execution history
