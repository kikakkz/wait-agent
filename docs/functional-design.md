# WaitAgent Functional Design

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

## 1. Purpose

This document defines the user-visible functions of WaitAgent and the exact expected behavior for each function.

It complements:

- [wait-agent-prd.md](wait-agent-prd.md)
- [architecture.md](architecture.md)

## 2. Functional Scope

WaitAgent provides three layers of functionality:

- Session hosting
- Attention scheduling
- Optional cross-machine aggregation

## 3. Functional Areas

### 3.1 Session Lifecycle

#### 3.1.1 Start Session

The system must allow a user to start a new session through WaitAgent.

Expected behavior:

- Allocate or adopt a PTY
- Launch the agent process
- Register the session
- Make the session eligible for focus and scheduling

#### 3.1.2 Adopt Existing Session

The system may support adopting an externally started session later, but this is not required for the MVP unless the PTY adoption path is technically straightforward.

#### 3.1.3 Exit Session

When a session exits:

- Mark it `exited`
- Remove it from eligible scheduling sets
- Preserve enough metadata for short-term UI continuity
- If focused, move focus to the next valid session

### 3.2 Console Attach

#### 3.2.1 Local Attach

A local CLI console may attach to the local WaitAgent runtime.

Behavior:

- One focused session is chosen
- Terminal resize and raw mode are enabled
- Input is routed to the focused session

#### 3.2.2 Server Attach

A server console may attach to the aggregate session plane.

Behavior:

- Sessions across nodes are visible
- Server-side scheduling uses aggregate waiting data
- Input to a remote session is forwarded to the owning client

#### 3.2.3 Multi-Console Attach

The same session may be attached from more than one console.

Behavior:

- All consoles receive synchronized output
- Any console may send input
- Concurrent input is serialized by arrival order

### 3.3 Focus Management

#### 3.3.1 Initial Focus Selection

When a console attaches:

- If there is exactly one runnable session, focus it
- Else if there is a most recent interactive session, reuse it
- Else choose the earliest active session

#### 3.3.2 Manual Focus Switch

The user may manually switch to:

- Next session
- Previous session
- Specific indexed session
- Specific session address

Effects:

- Focus changes immediately
- Scheduler lock clears
- Renderer restores the target screen

#### 3.3.3 Focus Loss

If the focused session exits or becomes unreachable:

- Release focus
- Choose the next eligible session
- Render a minimal transition notice

### 3.4 Input Handling

#### 3.4.1 Typing State

While the user has in-progress unsubmitted input:

- Automatic switching is forbidden
- Manual switching is forbidden

This rule prevents misrouting partially typed commands.

#### 3.4.2 Input Submission

When the user presses `Enter`:

- Send the input to the focused session
- Arm one automatic scheduling opportunity
- Start the continuation observation window

#### 3.4.3 Mirrored Input

In network mode:

- Input from local CLI appears in the server-side attached view
- Input from the server-side attached view appears in the local CLI
- Resulting PTY output is synchronized to both

### 3.5 Automatic Scheduling

#### 3.5.1 Entry Condition

Automatic scheduling may only be considered after input submission.

#### 3.5.2 Continuation Observation

After input submission:

- If the current session continues producing output as part of the same interaction round, stay on it
- Only when that round stabilizes may the scheduler consume the switch opportunity

#### 3.5.3 Waiting Queue Selection

If a switch opportunity is available and the current interaction has stabilized:

- Choose the earliest waiting session in FIFO order

#### 3.5.4 Locking

After an automatic switch:

- Lock further automatic switching
- Unlock on next input submission or manual switch

### 3.6 Waiting Detection

#### 3.6.1 Required Signals

The system must infer waiting without agent protocol support.

MVP signals:

- Recent output occurred
- Output became inactive for threshold `X`
- No input was sent recently
- Process is still alive

#### 3.6.2 Waiting Queue Update

When a session transitions into waiting:

- Add it to the queue if not already present
- Preserve FIFO order by first waiting timestamp

When a session becomes active again:

- Remove it from the waiting queue

### 3.7 Peek

Peek is a read-only operation.

Behavior:

- Show the current screen state of another session
- Do not change the active focus
- Do not change input ownership
- Exit back to the original focused session

### 3.8 Network Access Point

#### 3.8.1 Configure Access Point

The user may configure one server access point for a local WaitAgent instance.

Behavior:

- The client connects to the server
- Local sessions register automatically
- Server-side consoles can view and interact with those sessions
- Local behavior remains unchanged

#### 3.8.2 Disconnect

If the client loses connection:

- Mark the node offline on the server
- Keep local sessions running
- Prevent server-side writes to unreachable sessions
- Restore session linkage after reconnect when possible

### 3.9 Resize Synchronization

When the console terminal size changes:

- Update the focused console runtime
- Propagate size change to the owning PTY
- In network mode, forward resize through the server/client path

### 3.10 Minimal UI

The visible UI must remain terminal-native.

Required visible elements:

- Active session identifier
- Waiting count

Optional minimal elements:

- Node identifier
- Short switch notices
- Attach awareness notices

Disallowed:

- Dashboards
- Card layouts
- Summary-first panes
- Multi-panel workspace layouts

## 4. Command Surface

This section defines a suggested MVP command model.

### 4.1 Session Commands

- `waitagent run <agent-command...>`
- `waitagent list`
- `waitagent attach`

### 4.2 Network Commands

- `waitagent server`
- `waitagent client --connect <access-point>`
- `waitagent attach --server <access-point>`

### 4.3 Focus Commands

- `next-session`
- `prev-session`
- `focus-session <session>`
- `peek-session <session>`

These may be implemented as keyboard shortcuts rather than shell subcommands.

## 5. Functional Invariants

The following must hold in every mode:

- One console has one focused session
- Input never goes to a non-focused session within the same console
- Input-in-progress blocks switching
- One `Enter` creates at most one automatic switch opportunity
- Peek is read-only
- Session output remains raw terminal output

## 6. Acceptance Matrix

### 6.1 Local Mode

- Start three sessions
- Focus one
- Type partial input
- Verify switch is blocked
- Submit input
- Verify only one automatic switch may happen
- Verify Peek does not change focus

### 6.2 Network Mode

- Connect two client nodes
- Verify sessions appear on the server
- Interact locally with a remote-registered session
- Verify the server sees synchronized output
- Interact from the server
- Verify local CLI sees synchronized output
- Disconnect one client
- Verify other nodes remain usable

## 7. Out of Scope for This Document

This document does not define:

- Wire protocol schema
- Internal module boundaries
- Language-specific implementation details

Those belong in:

- `protocol.md`
- [module-design.md](module-design.md)

