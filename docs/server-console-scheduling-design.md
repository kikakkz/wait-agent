# WaitAgent Server Console Scheduling Design

Version: `v1.0`
Status: `Accepted for task.t6-01`
Date: `2026-04-29`

## 1. Purpose

This document is the authoritative top-down design for the active
`task.t6-01` server-console work.

It exists to remove ambiguity in four places:

- what the server console is in the product model
- which runtime owns server-console focus and scheduling state
- which data model is required for honest waiting-queue and auto-switch work
- which implementation slices are allowed next without falling into patch-on-patch changes

It complements:

- [remote-session-foundation.md](remote-session-foundation.md)
- [architecture.md](architecture.md)
- [interaction-flows.md](interaction-flows.md)
- [protocol.md](protocol.md)

## 2. Source Of Truth Rule

For active server-console work, this document is the source of truth for:

- server-console runtime ownership
- server-console focus state
- server-console scheduling state
- the implementation order for remaining `task.t6-01` slices

If older product or flow documents still describe remote work as fully deferred,
that wording must not be used to justify ad hoc implementation decisions inside
`task.t6-01`.

All further `task.t6-01` implementation must satisfy this rule:

> no server-console behavior change lands without an explicit top-down design
> home in this document or in a design document that supersedes it

Patch-style behavior additions without a declared model, state machine, and
runtime ownership boundary are not acceptable.

## 3. Product Model

The server console is not:

- a second workspace product
- a local tmux client masquerading as remote interaction
- a server-owned PTY for remote sessions

The server console is:

- one attached console
- backed by the shared transport-agnostic target catalog
- scoped to its own focus and scheduling state
- able to open either local or remote targets through one activation model

User-visible rule:

- local target and remote target both appear as activation targets
- choosing either target opens that target in the same server-console product surface
- transport differences are backend differences, not a second UX contract

## 4. Non-Negotiable Rules

1. One console model
   The server console must not grow a transport-specific interaction model.
   Local and remote targets may use different adapters, but they must report
   into one console-runtime state machine.
2. One scheduler per console
   Focus, waiting queue, scheduling opportunity, and switch lock are all
   console-scoped, never global.
3. No fake FIFO
   Waiting-queue order must come from explicit observed wait-entry events, not
   from incidental catalog enumeration order.
4. No fake auto-switch
   Automatic switching must not be claimed until the runtime has real submit,
   output-round, stabilization, and lock signals.
5. No server PTY ownership
   The server console may route input and render output for remote targets, but
   remote PTYs remain owned by remote nodes.
6. Mirrored interaction remains intact
   Local client consoles keep their own focus and scheduling state even when the
   same target is also open in the server console.

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
- it is not sufficient by itself to define FIFO ordering or auto-switch timing

### 5.4 Waiting Set And Waiting Queue

These are not the same thing.

- `waiting_set`
  The current set of targets with active waiting signals.
- `waiting_queue`
  An ordered queue of waiting targets based on first observed wait-entry order.

Required queue entry shape:

```text
WaitingQueueEntry {
  target_id
  first_wait_seq
  latest_wait_state
}
```

Rules:

- queue membership is added on a transition `not waiting -> waiting`
- queue membership is removed on a transition `waiting -> not waiting`
- queue order is by `first_wait_seq`, not by catalog order
- the focused target may still be present in the waiting set
- decision logic derives a switch-candidate queue that excludes the current
  focused target when evaluating whether to leave the current interaction round

### 5.5 Scheduling Opportunity

- `SchedulingOpportunity`
  One chance to auto-switch after a user submit.

State model:

- `disarmed`
- `armed { submit_seq }`
- `spent { submit_seq }`

Rules:

- only a real submit event may arm it
- it may be spent at most once
- a later submit creates a new opportunity

### 5.6 Interaction Round

- `InteractionRound`
  The period after a submit during which the currently focused target may keep
  producing output and therefore retain focus.

Required state:

- `round_submit_seq`
- `current_target_output_seen`
- `stabilized`

This is the mechanism that enforces:

`prompt1 -> submit -> current target continues output -> prompt2`

without premature switching.

### 5.7 Switch Lock

- `SwitchLock`
  A per-console lock that prevents repeated automatic switches from one submit.

State model:

- `unlocked`
- `locked { cause_submit_seq }`

Unlock conditions:

- the user submits input again
- the user manually switches target

## 6. Runtime Ownership

### 6.1 Current Ownership

Today the active ownership boundary is:

`CommandDispatcher -> RemoteServerConsoleRuntime`

This remains correct.

### 6.2 Required Runtime Shape

`RemoteServerConsoleRuntime` must own:

- picker lifecycle
- focused and selected target state
- server-console scheduling state
- the policy decision of whether to keep focus, return to picker, or switch

What it must not own directly:

- transport-specific PTY control details
- tmux PTY ownership
- remote authority transport internals

### 6.3 Required Interaction Boundary

The next runtime seam must be:

`ServerConsoleInteractionSurface`

This is a logical boundary, whether implemented as a trait, enum-backed adapter,
or runtime object.

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

Without this seam, scheduling logic will continue to split by transport and
degrade into patch-style branching.

## 7. Data And Ordering Requirements

The current catalog record is sufficient for:

- target identity
- transport
- coarse availability
- current task-state snapshot

It is not sufficient for:

- FIFO waiting order
- submit-scoped scheduling opportunity
- stabilization-based auto-switch

Therefore the next implementation must introduce explicit event ordering data.

Accepted first approach:

- a console-local monotonic sequence owned by `RemoteServerConsoleRuntime`
- queue insertion based on observed waiting transitions in that runtime
- submit sequencing based on observed console submit events in that runtime

Not acceptable:

- deriving queue order from `list_activation_targets()` enumeration order
- deriving submit opportunities from passive snapshot refresh alone

## 8. Decision Algorithm

The accepted future auto-switch decision flow is:

1. User submits input in the focused target.
2. Runtime arms one scheduling opportunity.
3. Runtime starts or continues the current interaction round.
4. If the focused target continues producing output, stay on it.
5. Once that round stabilizes, inspect the waiting queue excluding the focused target.
6. If no eligible waiting target exists, keep focus and leave the opportunity effectively unused.
7. If an eligible waiting target exists and switch lock is not active, auto-switch once.
8. Spend the scheduling opportunity and activate switch lock.
9. Keep lock until a later submit or a manual switch.

This means:

- waiting signals alone do not trigger switching
- submit alone does not trigger immediate switching
- queue order matters only after current-session continuation has had a chance

## 9. Presentation Rules

The picker may render a scheduling snapshot before full auto-switch exists.

Accepted pre-auto-switch UI:

- focused target label
- selected target label
- waiting count
- next queued waiting target
- queue position badges in the picker list
- explicit policy label such as `manual-only`

UI snapshot must not imply behavior that does not exist yet.

Therefore:

- `manual-only` is acceptable now
- labels implying active automatic scheduling are not acceptable until the
  state machine in this document is implemented

## 10. Implementation Plan

The remaining `task.t6-01` work should land in this order:

1. Design lock
   Land this document and point status/task state at it.
2. Extract interaction surface seam
   Normalize local and remote target interaction into one server-console event model.
3. Add real submit and manual-switch events
   The runtime must observe console-local interaction, not infer it from snapshots.
4. Add waiting transition tracking
   Build `waiting_set` and `waiting_queue` from transitions, not catalog order.
5. Add interaction-round stabilization
   Give the focused target a chance to continue before switching.
6. Add scheduling opportunity and switch lock
   Enforce one auto-switch per submit.
7. Only then enable server-side auto-switch policy
   Until then, keep explicit `manual-only` policy labeling.

## 11. Current Status Mapping

Already landed:

- public `waitagent server` entrypoint
- shared-catalog activation picker
- transport-agnostic target resolution
- local target route through existing local attach path
- remote target route through shared remote interact surface
- long-lived `picker -> target -> picker` lifecycle
- explicit focused versus selected target state
- manual-only scheduling snapshot based on current waiting membership

Not yet landed:

- unified interaction seam across local and remote targets
- FIFO waiting queue based on wait-entry transitions
- submit-scoped scheduling opportunity
- interaction-round stabilization
- switch lock
- real server-side auto-switch

## 12. Rejection Rule

Reject any future change that does one of the following:

- adds another server-console-only scheduler object outside
  `RemoteServerConsoleRuntime`
- adds transport-conditional scheduling behavior without a shared event seam
- treats catalog order as FIFO wait order
- introduces automatic switching without submit and stabilization signals
- documents behavior in status notes only without updating this design
