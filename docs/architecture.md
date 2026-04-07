# WaitAgent Architecture

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

## 1. Purpose

This document translates the product requirements in [wait-agent-prd.md](wait-agent-prd.md) into a build-oriented system architecture.

It answers:

- What runtime components exist
- How local mode and network mode share one model
- Where PTY ownership lives
- How focus, scheduling, synchronization, and rendering work
- Which state is local, shared, or derived

## 2. Architecture Principles

The architecture must preserve the following product constraints:

- Raw terminal semantics must remain intact
- The system must not parse or rewrite agent intent
- The same interaction model must apply in local and network mode
- Network mode must extend local mode rather than replace it
- Multiple consoles may attach to the same session, but each console keeps one local focus

## 3. Architectural View

WaitAgent has one core architecture with two deployment shapes:

- `Local mode`
- `Network mode`

The internal model should stay the same in both:

- A `session` is always a PTY-backed agent process
- A `console` is always an attached interaction surface
- A `scheduler` always operates per console
- A `renderer` always renders one focused session per console

Network mode only adds:

- A transport layer
- Remote client nodes
- Session registration and broadcast

## 4. Runtime Topology

### 4.1 Local Mode

```text
User Terminal
   ↓
CLI Frontend
   ↓
Console Runtime
   ↓
Session Registry
   ↓
Scheduler
   ↓
PTY Runtime
   ↓
Agent Processes
```

### 4.2 Network Mode

```text
Server Terminal
   ↓
Server CLI Frontend
   ↓
Server Console Runtime
   ↓
Server Session Registry
   ↓
Server Scheduler
   ↓
Transport Hub
   ↙                ↘
Client A            Client B
   ↓                  ↓
Local Console       Local Console
   ↓                  ↓
Local PTY Runtime   Local PTY Runtime
   ↓                  ↓
Agent Processes     Agent Processes
```

## 5. Core Architectural Components

### 5.1 CLI Frontend

Responsibilities:

- Parse commands and flags
- Bootstrap local or network mode
- Attach the current terminal as a console
- Apply terminal raw mode and resize wiring

### 5.2 Console Runtime

Responsibilities:

- Own one attached console lifecycle
- Track the console-local focus
- Track in-progress input state
- Invoke scheduler decisions
- Invoke renderer updates

Notes:

- Every attached console gets its own Console Runtime
- The same session may be visible from more than one Console Runtime

### 5.3 Session Registry

Responsibilities:

- Store session metadata
- Store session state snapshots
- Maintain session-to-node ownership
- Track attach relationships between consoles and sessions

The registry should distinguish:

- `authoritative state`
  Owned by the PTY host
- `replicated state`
  Cached elsewhere for rendering and scheduling
- `derived state`
  Waiting heuristics, activity windows, focus eligibility

Implementation note for local-first delivery:

- In `local mode`, the registry is authoritative because the PTY lives in-process
- In `network mode`, the PTY-owning client keeps the authoritative registry for local sessions
- The server maintains an aggregate replicated view, not PTY authority

This distinction must stay explicit in code to avoid over-designing the local MVP around distributed ownership.

### 5.4 PTY Runtime

Responsibilities:

- Spawn or adopt PTYs
- Read stdout/stderr byte streams
- Write stdin byte streams
- Handle resize
- Preserve byte-level ordering

This is the only place that directly owns PTY file descriptors.

### 5.5 Scheduler

Responsibilities:

- Maintain waiting queue per console
- Enforce switch lock per console
- Observe interaction rounds
- Decide when to stay, switch, or keep waiting

The scheduler must be console-scoped, not global.

### 5.6 Renderer

Responsibilities:

- Render one focused session to one console
- Restore a session’s visible screen state on switch
- Render minimal metadata chrome
- Support read-only Peek rendering

### 5.7 Transport Hub

Responsibilities in network mode:

- Maintain client connections
- Route events to the correct client or server console
- Broadcast PTY output to attached consoles
- Support reconnect and replay

### 5.8 Aggregate Session View

Responsibilities:

- Represent remote sessions without taking PTY ownership
- Merge replicated session metadata from many nodes
- Support server-side focus, scheduling, and rendering

This component exists only as a network-layer projection.

It must not:

- Become the source of truth for PTY lifecycle
- Own the canonical session screen state for client-owned PTYs
- Require the local MVP to implement replication logic before local interaction works

## 6. State Ownership

### 6.1 Authoritative State

Authoritative session state lives on the machine that owns the PTY.

Authoritative fields include:

- PTY handle
- Process lifecycle
- Raw byte streams
- Current screen buffer snapshot
- Last resize applied

### 6.2 Replicated State

Replicated state is synchronized outward for scheduling and rendering elsewhere.

Replicated fields include:

- Session identity
- Node identity
- Process status
- Screen snapshot version
- Activity timestamps
- Attach list

### 6.3 Derived State

Derived state is computed locally by the observing runtime.

Derived fields include:

- `waiting_input`
- `idle`
- `switch_lock`
- `interaction_round_active`
- `focus_candidate_rank`

## 7. Data Model

### 7.1 Node

Suggested fields:

- `node_id`
- `display_name`
- `mode`
- `connection_status`
- `last_heartbeat_at`
- `capabilities`

### 7.2 Session

Suggested fields:

- `session_id`
- `node_id`
- `address`
- `title`
- `process_id`
- `status`
- `created_at`
- `last_output_at`
- `last_input_at`
- `screen_version`
- `screen_snapshot_ref`

### 7.3 Console

Suggested fields:

- `console_id`
- `location`
  Values: `local`, `server`, `remote-attach`
- `focused_session`
- `peek_session`
- `input_buffer_state`
- `switch_lock`
- `scheduler_state`

### 7.4 Events

WaitAgent should use an event-driven core.

Minimum event types:

- `session_started`
- `session_exited`
- `stdout_chunk`
- `stdin_chunk`
- `resize_applied`
- `session_metadata_updated`
- `node_connected`
- `node_disconnected`
- `console_attached`
- `console_detached`
- `focus_changed`
- `peek_started`
- `peek_ended`

## 8. Session Screen Model

To preserve TTY fidelity while supporting switching, WaitAgent should maintain a terminal screen model per session.

Recommended design:

- Ingest PTY output bytes
- Feed them into a terminal emulator state engine
- Store the resulting screen snapshot
- Restore that snapshot into a console when the session becomes focused

The screen model must support:

- Normal screen
- Alternate screen
- Cursor position
- Scrollback
- Resize reflow policy

Important:

- WaitAgent may interpret ANSI for terminal state reconstruction
- WaitAgent must not interpret agent semantics

## 9. Scheduling Architecture

### 9.1 Why Scheduler State Is Per Console

The PRD requires local CLI and server console to remain interactive at the same time.

Therefore:

- Focus cannot be globally unique
- Waiting queues cannot be globally singular
- Switch locks cannot be shared across all consoles

The correct model is:

- One `session registry`
- Many `consoles`
- One scheduler instance per console

### 9.2 Scheduler Inputs

The scheduler observes:

- Session activity timestamps
- Waiting heuristic transitions
- Current focus
- User input submission
- Switch lock state
- Manual switch events

### 9.3 Scheduler Outputs

The scheduler may emit:

- `stay_on_current_session`
- `switch_to_session`
- `arm_switch_lock`
- `clear_switch_lock`
- `update_waiting_queue`

### 9.4 Scheduler State Machine

Suggested states:

- `idle`
- `typing`
- `armed_after_enter`
- `observing_continuation`
- `locked_after_auto_switch`

Transition outline:

1. `typing`
   User is editing input. No switching allowed.
2. `armed_after_enter`
   One scheduling opportunity is available.
3. `observing_continuation`
   Current focused session may still be in the same interaction round.
4. `locked_after_auto_switch`
   One automatic switch already happened. No more auto-switches until unlock.

## 10. Input Routing Architecture

All user input should pass through an explicit Input Router.

Responsibilities:

- Accept input from one console
- Resolve the target focused session for that console
- Write bytes to the authoritative PTY host
- Emit mirrored input events for other attached consoles

Important distinction:

- WaitAgent routes input by console focus
- WaitAgent does not attempt to arbitrate semantic intent

If two consoles type at once:

- Both input streams are accepted
- Arrival order defines PTY write order
- Optional awareness signals may be emitted

## 11. Local and Network Unification

WaitAgent should not maintain two independent implementations.

Recommended implementation rule:

- Local mode is network mode with the transport loop collapsed into the same process

That means:

- The same event bus model should be used in both modes
- The same session registry shape should be used in both modes
- The same console runtime should work with local or remote sessions

This reduces behavioral drift between local and network mode.

Important local-first clarification:

- The local MVP should ship with no real remote transport dependency
- The only requirement is that transport-facing boundaries already exist
- Network mode should later plug into those boundaries using a remote transport adapter
- If useful, a loopback transport adapter may be used in tests or in-process simulations, but it must not become a prerequisite for delivering the first local MVP

## 12. Process Architecture

Recommended runtime split:

- `waitagent`
  CLI binary
- `waitagent-server`
  Optional dedicated server entrypoint

Alternative:

- One binary with subcommands:
  - `waitagent run`
  - `waitagent server`

The single-binary model is preferable for consistency and distribution simplicity.

Recommended command progression:

- Phase 1:
  - `waitagent run`
- Phase 2:
  - `waitagent server`
  - `waitagent run --connect ...`

## 13. Persistence Strategy

MVP persistence should stay minimal.

Persist only what is necessary:

- Node identity
- Access point configuration
- Credentials or tokens
- Session metadata cache for reconnect

Do not persist:

- Full PTY byte logs by default
- Semantic summaries
- Full command history outside the PTY itself

## 14. Security Architecture

Minimum security requirements:

- Explicit node enrollment
- Authenticated server/client transport
- Revocable credentials
- No unauthenticated console attach

Recommended additional controls:

- Per-node authorization
- Per-session attach authorization
- Audit event stream for attach and input actions

## 15. Failure Model

The system must tolerate:

- One session crash
- One client disconnect
- Server reconnect windows
- Partial event replay after reconnect

Failure rules:

- A session crash must remain isolated
- A client disconnect must not kill local PTYs
- A server disconnect must not remove local console usability
- A reconnected node should attempt to reclaim existing session identity

## 16. Observability

WaitAgent needs low-level observability because most failures will be interaction-level, not business-logic-level.

Recommended instrumentation:

- Event counters
- Session lifecycle logs
- Scheduler transition logs
- Transport reconnect logs
- PTY write/read metrics
- Console attach/detach logs

Recommended debug views:

- Session table
- Console table
- Scheduler state per console
- Last event stream per session

## 17. Recommended Implementation Stack

Reference recommendation:

- Language: `Rust`
- PTY handling: platform-native PTY crate
- Terminal state engine: VT-compatible emulator library
- Async runtime: `tokio`
- Transport: `WebSocket` or framed TCP with structured messages
- Serialization: `serde` + versioned message schema

Reasoning:

- PTY and terminal fidelity are systems problems
- Concurrency and byte-stream ordering matter
- Network mode requires stable async and reconnect behavior

## 18. Open Design Questions

These questions should be resolved before implementation locks down:

- Should scrollback be bounded per session in the MVP
- Should server-side consoles default to attach-all visibility or on-demand attach
- Should local and server schedulers share waiting metadata or derive independently
- How should alternate screen restoration behave across console attaches
- Whether attach notifications should be silent or explicit by default

## 19. Deliverables After This Document

Next implementation documents should refine:

- [functional-design.md](functional-design.md)
- [module-design.md](module-design.md)
- `protocol.md`
