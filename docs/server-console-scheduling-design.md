# WaitAgent Server Console Attention Design

Version: `v1.1`
Status: `Accepted for task.t6-01`
Date: `2026-04-29`

## 1. Purpose

This document is the authoritative top-down design for active
`task.t6-01` server-console work.

It exists to remove ambiguity in four places:

- what the server console is in the product model
- which runtime owns server-console focus and waiting state
- which data model is required for honest waiting-queue visibility
- which implementation slices are allowed next without reintroducing
  automatic focus changes

It complements:

- [remote-session-foundation.md](remote-session-foundation.md)
- [architecture.md](architecture.md)
- [interaction-flows.md](interaction-flows.md)
- [protocol.md](protocol.md)

## 2. Source Of Truth Rule

For active server-console work, this document is the source of truth for:

- server-console runtime ownership
- server-console focus state
- server-console waiting-state visibility
- the implementation order for remaining `task.t6-01` slices

All further `task.t6-01` implementation must satisfy this rule:

> no server-console behavior change lands without an explicit top-down design
> home in this document or in a design document that supersedes it

Patch-style behavior additions without a declared model, state shape, and
runtime ownership boundary are not acceptable.

## 3. Product Model

The server console is not:

- a second workspace product
- a local tmux client masquerading as remote interaction
- a server-owned PTY for remote sessions
- an automation surface that moves focus on the user’s behalf

The server console is:

- one attached console
- backed by the shared transport-agnostic target catalog
- scoped to its own focus and waiting-state visibility
- able to open either local or remote targets through one activation model

User-visible rule:

- local target and remote target both appear as activation targets
- choosing either target opens that target in the same server-console product
  surface
- transport differences are backend differences, not a second UX contract
- waiting state may raise attention through chrome, but focus changes remain
  manual

## 4. Non-Negotiable Rules

1. One console model
   The server console must not grow a transport-specific interaction model.
   Local and remote targets may use different adapters, but they must report
   into one console-runtime state machine.
2. One attention model per console
   Focus and waiting visibility are console-scoped, never global.
3. No fabricated ordering
   Waiting state is advisory only; the accepted product direction does not need
   queue ordering semantics in chrome.
4. Manual-only focus
   Waiting state may inform the user, but it must not automatically move focus.
5. No server PTY ownership
   The server console may route input and render output for remote targets, but
   remote PTYs remain owned by remote nodes.
6. Mirrored interaction remains intact
   Local client consoles keep their own focus and waiting visibility even when
   the same target is also open in the server console.

## 5. Domain Model

### 5.1 Console Identity

- `ServerConsoleId`
  Stable identity for one server-console runtime instance.

### 5.2 Focus State

- `focused_target`
  The target currently open in the server console.
- `selected_target`
  The target currently highlighted in the picker when the picker is visible.

Rules:

- `focused_target` and `selected_target` are related but distinct
- losing the focused target releases focus in this console only
- picker restoration should prefer the last focused target when still available

### 5.3 Waiting Signal

- `WaitingSignal`
  A console-consumable signal that a target is likely waiting for user input.

Current accepted signal source:

- shared-catalog task state projected as `INPUT` or `CONFIRM`

Important limitation:

- this is only membership evidence for “likely waiting”
- it must not be used to justify automatic focus movement

### 5.4 Waiting Visibility

- `waiting_set`
  The current set of targets with active waiting signals.

Rules:

- waiting membership is added on a transition `not waiting -> waiting`
- waiting membership is removed on a transition `waiting -> not waiting`
- the focused target may still be present in the waiting set

### 5.5 Attention Cue Policy

Waiting state exists to attract user attention, not to seize focus.

Accepted attention cues:

- focused target label
- selected target label
- waiting count
- explicit policy label such as `manual-only`

Not accepted:

- automatic target activation
- delayed automatic jumps after submit
- hidden scheduler state that implies future focus movement

## 6. Runtime Ownership

### 6.1 Current Ownership

Today the active ownership boundary is:

`CommandDispatcher -> RemoteServerConsoleRuntime`

This remains correct.

### 6.2 Required Runtime Shape

`RemoteServerConsoleRuntime` must own:

- picker lifecycle
- focused and selected target state
- waiting-set and waiting-queue state
- the policy decision to remain manual-only

What it must not own directly:

- transport-specific PTY control details
- tmux PTY ownership
- remote authority transport internals

### 6.3 Required Interaction Boundary

The runtime seam remains:

`ServerConsoleInteractionSurface`

It must normalize both:

- local target interaction
- remote target interaction

into one event stream consumed by `RemoteServerConsoleRuntime`.

Required emitted events:

- `TargetOpened`
- `ConsoleInputStarted`
- `ConsoleSubmit`
- `FocusedTargetOutput`
- `FocusedTargetStabilized`
- `ManualReturnToPicker`
- `FocusedTargetExited`
- `FocusedTargetUnavailable`

These events exist so focus ownership, waiting visibility, and future chrome
behavior stay transport-agnostic. They must not be used to reintroduce
automatic focus changes.

## 7. Data And Ordering Requirements

The current catalog record is sufficient for:

- target identity
- transport
- coarse availability
- current task-state snapshot

It is not sufficient for:

- deciding focus automatically

Not acceptable:

- inferring focus movement from passive waiting snapshots

## 8. Presentation Rules

The picker or sidebar may render a waiting snapshot.

Accepted UI:

- waiting-state badges
- optional simple waiting counts
- manual-only policy labeling

UI must not imply behavior that does not exist.

Therefore:

- `manual-only` is required
- wording that suggests the system will switch automatically is not acceptable

## 9. Implementation Plan

The accepted `task.t6-01` order is now:

1. Design lock
   Land this document and point status/task state at it.
2. Extract interaction surface seam
   Normalize local and remote target interaction into one server-console event
   model.
3. Add real submit and manual-switch events
   The runtime must observe console-local interaction, not infer it from
   snapshots.
4. Add waiting transition tracking
   Build `waiting_set` from transitions, not catalog order.
5. Lock policy on manual-only attention cues
   Retire auto-switch planning and keep future work limited to visibility polish.

## 10. Current Status Mapping

Already landed:

- dedicated hidden server-console entrypoint
- shared-catalog activation picker
- transport-agnostic target resolution
- local target route through existing local attach path
- remote target route through shared remote interact surface
- long-lived `picker -> target -> picker` lifecycle
- explicit focused versus selected target state
- real submit and manual-switch signal capture through the shared interaction
  seam
- waiting-state visibility rendered through per-session task state
- manual-only waiting snapshot rendered without queue ordering

Not planned:

- scheduling opportunity
- interaction-round-based auto-switch
- switch lock for automatic focus movement
- real server-side auto-switch

Possible future work:

- stronger manual-only attention cues in sidebar, picker, badge, or count
  presentation if acceptance evidence shows the current cues are too weak

## 11. Rejection Rule

Reject any future change that does one of the following:

- adds another server-console-only scheduler object outside
  `RemoteServerConsoleRuntime`
- adds transport-conditional waiting behavior without a shared event seam
- treats waiting state as an ordering contract rather than simple visibility
- introduces any automatic focus jump without an explicit product reversal
- documents focus-changing behavior in status notes only without updating this
  design
