# WaitAgent Architecture

Version: `v1.1`  
Status: `Draft`  
Date: `2026-04-23`

## 1. Purpose

Current note:

- the accepted replacement for the old custom local fullscreen and live-surface path is documented in [tmux-first-workspace-plan.md](tmux-first-workspace-plan.md)
- the accepted code-level runtime reorganization for that migration is documented in [tmux-first-runtime-architecture.md](tmux-first-runtime-architecture.md)
- until this architecture document is fully revised, treat the tmux-first plan as the authoritative local workspace display direction

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
- One `waitagent` instance should be the default local entrypoint per machine
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
- A server-side interaction surface for remote sessions routed through the server control plane

## 4. Runtime Topology

### 4.1 Local Mode

```text
User Terminal
   ↓
WaitAgent Workspace Entry
   ↓
Console Runtime
   ↓
Session Registry
   ↓
Scheduler
   ↓
PTY Runtime
   ↓
Managed Shell / Agent Workflows
```

### 4.2 Network Mode

```text
Server Terminal
   ↓
Server Workspace Entry
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
Local Workspace     Local Workspace
   ↓                  ↓
Local PTY Runtime   Local PTY Runtime
   ↓                  ↓
Managed Sessions    Managed Sessions
```

### 4.3 Remote Session Connection Model

For future remote sessions, the accepted interaction path is:

- the remote waitagent node maintains a long-lived connection to the server
- the remote node remains the PTY owner for its sessions
- the server maintains aggregate session state and routes control messages
- server-side user interaction runs through a waitagent `interact` surface, not through a server-owned PTY pretending to be local

Anti-goal:

- do not model remote sessions as if the server directly owns their PTY
- do not reduce the server-side experience to a point-to-point mirror CLI that bypasses the server control plane

## 5. Core Architectural Components

### 5.1 Workspace Entry

Responsibilities:

- Parse commands and flags
- Bootstrap local workspace mode
- Attach the current terminal as a console
- Apply terminal raw mode and resize wiring

Preferred public model:

- `waitagent`
- `waitagent attach <session>`
- `waitagent ls`
- `waitagent detach [session]`

Future remote connection and management entrypoints are deferred until the remote model is redesigned on top of the local workspace architecture.

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
- In future network mode, a server-side console interacting with a remote session does so through server-routed control and stream messages rather than direct PTY ownership

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

Future compatibility note:

- sidebar, footer, menu, and session selection should depend on transport-agnostic session records
- local tmux discovery is one producer of those records, not the universal source of truth

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
- Support fullscreen and normal workspace rendering modes

### 5.7 Transport Hub

Responsibilities in network mode:

- Maintain client connections
- Route events to the correct client or server console
- Broadcast PTY output to attached consoles
- Support reconnect and replay
- Carry server-side `interact` traffic for remote sessions over the same control plane used for registry and lifecycle events

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

### 5.9 Server Interact Surface

Responsibilities:

- provide the server-side interactive view for a selected remote session
- consume terminal stream data routed by the server
- send raw input and resize events back through the server control plane
- remain separate from PTY ownership

This is not a PTY host and not a direct remote-mirror shortcut around the server.
It is the server-visible interaction surface for a host-owned session.

## 6. State Ownership

### 6.1 Authoritative State

Authoritative session state lives on the machine that owns the PTY.

Authoritative fields include:

- PTY handle
- Process lifecycle
- Raw byte streams
- Current screen buffer snapshot
- Last resize applied

In future network mode:

- the remote node remains authoritative for remote PTY-backed sessions
- the server is authoritative for routing, attach state, and interaction leases, but not for the remote PTY itself

### 6.2 Replicated State

Replicated state is synchronized outward for scheduling and rendering elsewhere.

Replicated fields include:

- Session identity
- Node identity
- Process status
- Screen snapshot version
- Activity timestamps
- Attach list
- Host-published command label, path, task-state, and screen or stream version metadata

### 6.3 Derived State

Derived state is computed locally by the observing runtime.

Derived fields include:

- `waiting_input`
- `idle`
- `switch_lock`
- `interaction_round_active`
- `focus_candidate_rank`

## 6.4 Interaction Lease

Remote interactive sessions require an explicit interaction lease.

The lease determines:

- which attached console currently has write authority
- which console's resize is authoritative for the PTY-backed session
- which observers are read-only mirrors at that moment

This lease is required because one PTY can only have one effective terminal size at a time.
The server should coordinate the lease, while the PTY host remains authoritative for applying the resulting input and resize events.

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
- `input_buffer_state`
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

- `update_waiting_queue`
- `mark_session_waiting`
- `mark_session_active`

### 9.4 Scheduler State Machine

The current local product does not perform automatic switching.

Any future automation should be:

- policy-driven rather than heuristic magic
- explicit in user-visible state
- designed after the unified local and remote session model is settled

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
  Workspace-first CLI binary

Recommended command progression:

- Current:
  - `waitagent`
  - `waitagent attach`
  - `waitagent ls`
  - `waitagent detach`
- Future remote connection and management commands:
  - deferred until the remote model is redesigned on top of the accepted local architecture

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
- Whether new local sessions should always start from a shell template or support an optional initial command in the MVP

## 19. Deliverables After This Document

Next implementation documents should refine:

- [functional-design.md](functional-design.md)
- [module-design.md](module-design.md)
- `protocol.md`
