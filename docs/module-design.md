# WaitAgent Module Design

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

## 1. Purpose

This document defines the internal module boundaries for the WaitAgent implementation.

It is meant to support:

- Parallel implementation
- Clear ownership boundaries
- Testability
- Minimal coupling between terminal, scheduler, and network concerns

## 2. Recommended Repository Structure

Suggested structure:

```text
src/
  main.rs
  cli/
  app/
  console/
  session/
  pty/
  scheduler/
  renderer/
  terminal/
  transport/
  server/
  client/
  config/
  auth/
  storage/
  diagnostics/
```

This layout may later be split into a workspace if the codebase grows, but a single crate is acceptable for the MVP.

## 3. Module Overview

### 3.1 `cli`

Responsibilities:

- Parse subcommands and flags
- Build runtime configuration
- Start the correct app mode

Should not:

- Own PTY state
- Implement scheduler logic
- Implement transport semantics directly

### 3.2 `app`

Responsibilities:

- Application bootstrap
- Dependency wiring
- Runtime startup and shutdown
- Mode selection for the local workspace runtime

This should be the composition root.

### 3.3 `console`

Responsibilities:

- Console lifecycle
- Focus state for one attached console
- Input buffer state
- Keybinding dispatch

Key types:

- `ConsoleId`
- `ConsoleState`
- `ConsoleManager`

### 3.4 `session`

Responsibilities:

- Session registry
- Session metadata
- Session lifecycle transitions
- Attach tracking

Key types:

- `SessionId`
- `SessionAddress`
- `SessionRecord`
- `SessionRegistry`

Recommended internal split:

- `AuthoritativeSessionStore`
  Lives with the PTY owner
- `AggregateSessionView`
  Replicated projection used by the server and other non-owning consoles

### 3.5 `pty`

Responsibilities:

- Spawn or adopt PTY-backed processes
- Byte-level IO
- Resize application
- PTY ownership

Key types:

- `PtyId`
- `PtyHandle`
- `PtyManager`
- `PtyEvent`

Hard rule:

- Only this module touches PTY descriptors directly

### 3.6 `scheduler`

Responsibilities:

- Waiting detection
- Waiting queue maintenance
- Session attention signaling
- Policy hook points for future automation

Key types:

- `SchedulerState`
- `WaitingQueue`
- `SwitchLock`
- `SchedulingDecision`

The scheduler must be console-scoped.

### 3.7 `renderer`

Responsibilities:

- Render one focused session into one console
- Draw minimal chrome
- Restore session screen snapshots on switch

Key types:

- `RenderFrame`
- `RenderTarget`
- `Renderer`

### 3.8 `terminal`

Responsibilities:

- Terminal raw mode handling
- Resize detection
- VT state model
- Screen snapshot reconstruction

Key types:

- `ScreenState`
- `ScreenSnapshot`
- `TerminalSize`
- `TerminalEngine`

This module is terminal-state aware but must remain agent-semantics unaware.

### 3.9 `transport`

Responsibilities:

- Server/client connection handling
- Message framing
- Broadcast and routing
- Reconnect logic

Key types:

- `TransportMessage`
- `TransportClient`
- `TransportServer`
- `ConnectionId`

Local-first rule:

- The implementation must compile and run without enabling remote transport
- Transport should be an optional integration boundary, not a prerequisite for local mode
- A `LoopbackTransport` test adapter is acceptable
- A real remote transport implementation is phase-two work

### 3.10 `server`

Responsibilities:

- Server-side aggregate runtime
- Node registry
- Session registration from clients
- Server-side attach handling

Key types:

- `ServerRuntime`
- `NodeRegistry`
- `ServerSessionView`

### 3.11 `client`

Responsibilities:

- Client-side connection to server
- Session publication
- Forward remote input into local PTYs
- Broadcast local PTY output outward

Key types:

- `ClientRuntime`
- `ClientPublisher`
- `ReconnectState`

### 3.12 `config`

Responsibilities:

- Access point configuration
- Node identity
- CLI defaults
- Persisted local settings

Key types:

- `AppConfig`
- `AccessPointConfig`
- `NodeConfig`

### 3.13 `auth`

Responsibilities:

- Credential loading
- Enrollment flow
- Connection authorization
- Token rotation support

### 3.14 `storage`

Responsibilities:

- Persist small local state
- Cache reconnect metadata
- Store configuration state

Should not:

- Store full PTY recordings by default

### 3.15 `diagnostics`

Responsibilities:

- Structured logging
- Metrics
- Debug dumps
- Tracing hooks

## 4. Dependency Rules

Recommended dependency direction:

```text
cli -> app
app -> console, session, scheduler, renderer, pty, terminal, transport, config, auth, diagnostics
console -> scheduler, renderer, session, terminal
session -> none
scheduler -> session
renderer -> terminal, session, console
pty -> session
transport -> session message types
server -> transport, session, console
client -> transport, pty, session
```

Important constraints:

- `scheduler` must not depend on `renderer`
- `renderer` must not depend on `pty`
- `pty` must not depend on `scheduler`
- `transport` must not know terminal rendering details
- `console`, `scheduler`, and `renderer` must not depend on real network transport for local mode

## 5. Core Interfaces

Suggested interface boundaries:

### 5.1 Session Registry Interface

Should support:

- `create_session`
- `update_session_status`
- `get_session`
- `list_sessions`
- `attach_console`
- `detach_console`

### 5.2 Scheduler Interface

Should support:

- `on_input_started`
- `on_input_submitted`
- `on_session_activity`
- `on_session_waiting`
- `on_manual_switch`
- `decide_next_action`

### 5.3 PTY Interface

Should support:

- `spawn`
- `adopt`
- `write`
- `resize`
- `subscribe_output`
- `terminate`

### 5.4 Renderer Interface

Should support:

- `render_focus`
- `render_status_line`
- `render_waiting_indicator`

### 5.5 Transport Interface

Should support:

- `connect`
- `accept`
- `send`
- `broadcast`
- `replay`
- `close`

## 6. Internal Event Bus

The app should use an internal event bus rather than direct cross-module calls for all runtime transitions.

Benefits:

- Cleaner concurrency
- Easier diagnostics
- Better replay for tests
- Easier local and network unification

Suggested event groups:

- Console events
- Session events
- PTY events
- Scheduler events
- Transport events

## 7. Concurrency Model

Recommended model:

- One async task group for transport
- One async task group for PTY readers
- One event-processing loop per console
- Shared registry with controlled mutation

Mutation rules:

- Session registry updates should be serialized
- PTY writes should be ordered per session
- Renderer should consume immutable snapshots

## 8. Testing Strategy by Module

### 8.1 `scheduler`

Must have deterministic unit tests for:

- One-enter one-switch rule
- Lock and unlock behavior
- Continuation protection
- Per-session waiting-attention state transitions

### 8.2 `pty`

Must have integration tests for:

- PTY spawn
- Byte ordering
- Resize handling
- Exit detection

### 8.3 `renderer` and `terminal`

Must have snapshot-style tests for:

- Focus switch restoration
- Alternate screen behavior
- Minimal chrome rendering

### 8.4 `transport`

Must have reconnect and replay tests for:

- Client disconnect
- Session re-registration
- Output broadcast to multiple consoles
- Mirrored input visibility

### 8.5 End-to-End

Must have system tests for:

- Three local sessions
- Two remote nodes
- Server and client attached to the same session
- Concurrent input from two consoles

## 9. MVP Implementation Order

Recommended order:

1. `session`
2. `pty`
3. `terminal`
4. `renderer`
5. `scheduler`
6. `console`
7. `cli`
8. `transport`
9. `server`
10. `client`

Reasoning:

- Local mode should become real before network mode
- Local interaction correctness should be proven before remote synchronization increases complexity

Local-first checkpoint:

- Do not start `transport`, `server`, or `client` implementation until one local console can:
  - run multiple sessions
  - enforce typing protection
  - switch focus
  - preserve fullscreen and alternate-screen behavior

## 10. Open Module Questions

Questions to resolve:

- Should `terminal` wrap an external VT engine or own a custom adapter
- Should `console` own keybindings directly or delegate to a separate input module
- Should `transport` messages use versioned enums or explicit schema objects
- Should `storage` exist in MVP or remain embedded in `config`

## 11. Related Documents

- [wait-agent-prd.md](wait-agent-prd.md)
- [architecture.md](architecture.md)
- [functional-design.md](functional-design.md)
