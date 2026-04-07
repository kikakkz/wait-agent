# WaitAgent Interaction Flows

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

## 1. Purpose

This document defines the main user interaction flows for WaitAgent.

It describes how the product should behave step by step in:

- Local mode
- Network mode
- Focus switching
- Auto-scheduling
- Peek
- Mirrored local/server interaction
- Disconnect and recovery paths

It complements:

- [wait-agent-prd.md](wait-agent-prd.md)
- [functional-design.md](functional-design.md)
- [ui-design.md](ui-design.md)

## 2. Flow Conventions

Each flow is described with:

- Trigger
- Preconditions
- Main path
- Result
- Edge notes

## 3. Primary Flows

### 3.1 Start Local Workspace Flow

Trigger:

- The user starts WaitAgent on a local machine

Preconditions:

- Local WaitAgent binary is available

Main path:

1. User runs `waitagent`.
2. WaitAgent boots one local workspace runtime.
3. WaitAgent creates a console runtime for the current terminal.
4. WaitAgent restores existing session state or initializes an empty workspace.
5. If a focusable session exists, WaitAgent focuses it.
6. The terminal begins rendering the focused session or empty workspace state.

Result:

- One local WaitAgent workspace is active and ready to manage sessions.

Edge notes:

- Starting WaitAgent should not require the user to decide upfront which agent command will run first.

### 3.2 Create Local Session Flow

Trigger:

- The user creates a new managed session inside WaitAgent

Preconditions:

- A local workspace is active

Main path:

1. User triggers `new-session` or the equivalent control action.
2. WaitAgent allocates a PTY for the new session.
3. WaitAgent starts the session with the configured shell or template.
4. WaitAgent registers the session in the local session registry.
5. If policy says the new session should be foregrounded, WaitAgent focuses it.
6. The terminal renders the new focused session or keeps the prior focus if creation was backgrounded.

Result:

- A new managed session exists and is eligible for scheduling.

### 3.3 Run Agent Command Inside Session Flow

Trigger:

- The user is inside a focused session and starts an agent workflow

Preconditions:

- A focused session exists

Main path:

1. User types a normal command such as `codex`, `claude`, `kilo`, `cd`, or a shell script.
2. WaitAgent forwards the raw bytes to the focused PTY.
3. The session command runs inside that PTY-backed session.
4. Output renders as raw terminal output in the focused viewport.

Result:

- WaitAgent remains a transport and scheduling layer, not a semantic command interpreter.

### 3.4 Typing and Submit Flow

Trigger:

- The user types input into the focused session

Preconditions:

- A focused session exists

Main path:

1. User types characters.
2. Console runtime marks input as in-progress.
3. Automatic and manual switching are blocked.
4. User presses `Enter`.
5. Input is sent to the focused session PTY.
6. WaitAgent arms one auto-switch opportunity.
7. WaitAgent enters continuation observation state.

Result:

- The session receives the input.
- One scheduling opportunity exists.

### 3.5 Continuous Interaction Protection Flow

Trigger:

- A scheduling opportunity exists after `Enter`

Preconditions:

- Current focused session is still active

Main path:

1. User submits input.
2. Current session continues producing output.
3. Scheduler classifies this as the same interaction round.
4. Scheduler refuses to switch away yet.
5. Scheduler waits for the round to stabilize.

Result:

- Focus stays on the current session.

Edge notes:

- This is the rule that protects `prompt1 -> input -> prompt2`.

### 3.6 Automatic Switch Flow

Trigger:

- A scheduling opportunity exists and another session is waiting

Preconditions:

- Current interaction round has stabilized
- Switch lock is clear
- Waiting queue is non-empty

Main path:

1. Scheduler evaluates the waiting queue.
2. Scheduler selects the earliest waiting session.
3. Console focus changes to that session.
4. Renderer restores the selected session screen.
5. Scheduler arms switch lock.

Result:

- One automatic switch occurs.
- No further auto-switch may happen until unlock.

### 3.7 Manual Switch Flow

Trigger:

- The user invokes next, previous, or direct focus selection

Preconditions:

- No unsubmitted input is in progress

Main path:

1. User triggers manual switch.
2. WaitAgent resolves the target session.
3. Console focus changes immediately.
4. Scheduler lock clears.
5. Renderer restores the target session.

Result:

- The user now interacts with the selected session.

### 3.8 Peek Flow

Trigger:

- The user invokes Peek on another session

Preconditions:

- Another session exists

Main path:

1. User triggers Peek.
2. WaitAgent records the original focused session.
3. WaitAgent renders the target session in read-only mode.
4. Input ownership stays with the original focus.
5. User exits Peek.
6. WaitAgent restores the original focused session viewport.

Result:

- The user inspected another session without changing focus ownership.

## 4. Network Flows

### 4.1 Configure Access Point Flow

Trigger:

- The user configures a WaitAgent server access point for a local workspace

Preconditions:

- Local workspace runtime is installed
- Network credentials are available

Main path:

1. User starts `waitagent --connect <access-point>` or sets equivalent configuration.
2. The local workspace establishes a connection to the server.
3. The local workspace authenticates.
4. The local workspace publishes node metadata.
5. The local workspace registers all local sessions.
6. Server adds the node and sessions to its aggregate registry.

Result:

- Local sessions become visible on the server side.
- Local CLI behavior remains unchanged.

### 4.2 Server Console Flow

Trigger:

- The user starts a server-side WaitAgent runtime

Preconditions:

- At least one client node is connected or one server-local session exists

Main path:

1. User runs `waitagent server`.
2. Server creates a console runtime.
3. Server builds the aggregate session view.
4. Server selects an initial focused session.
5. Renderer displays that focused session.

Result:

- The server terminal becomes a first-class interaction surface for the same workspace model.

### 4.3 Mirrored Interaction Flow

Trigger:

- The same session is attached from local CLI and server console

Preconditions:

- Network mode is active
- One session exists on the client node
- The same session is attached by both consoles

Main path:

1. User types in the local CLI.
2. Input is written to the client PTY.
3. PTY produces output.
4. Output is broadcast to the local console and server console.
5. Later, user types on the server console.
6. Input is routed to the client PTY.
7. PTY produces output again.
8. Output is broadcast back to both consoles.

Result:

- Both consoles remain synchronized around the same PTY-backed session.

### 4.4 Multi-Console Input Conflict Flow

Trigger:

- Two attached consoles type into the same session at nearly the same time

Preconditions:

- Same session is attached by multiple consoles

Main path:

1. Console A sends input.
2. Console B sends input.
3. Input router serializes writes in arrival order.
4. PTY processes the resulting byte sequence.
5. Output is broadcast to all attached consoles.

Result:

- The session remains consistent at the PTY level.

Edge notes:

- WaitAgent does not merge or reinterpret concurrent user intent.

### 4.5 Server-Side Auto-Scheduling Flow

Trigger:

- The server console has one scheduling opportunity after input submission

Preconditions:

- Multiple sessions from multiple nodes may be waiting

Main path:

1. Server-side scheduler observes the aggregate waiting queue.
2. Scheduler prefers current-session continuation first.
3. Once stable, scheduler selects the earliest waiting session.
4. Focus changes only in the server console.
5. Local client consoles keep their own focus unchanged.

Result:

- Server-side interaction stays single-focus without stealing local client focus.

## 5. Failure and Recovery Flows

### 5.1 Focused Session Exit Flow

Trigger:

- The focused session exits

Preconditions:

- Session process terminates

Main path:

1. Session registry marks the session exited.
2. Console runtime detects focused session loss.
3. Scheduler selects the next eligible session.
4. Renderer shows the new focus or empty state.

Result:

- The console remains usable.

### 5.2 Client Disconnect Flow

Trigger:

- A client node disconnects from the server

Preconditions:

- Network mode is active

Main path:

1. Server stops receiving heartbeat or transport events.
2. Server marks node `offline`.
3. Remote sessions become unreachable on the server.
4. If one unreachable session was focused on the server console, focus is released.
5. Scheduler selects the next reachable session if available.

Result:

- Server-side interaction remains usable for other nodes.

### 5.3 Local-Only Continuation After Disconnect

Trigger:

- The client loses server connectivity

Preconditions:

- The client still owns the PTYs locally

Main path:

1. Client transport fails.
2. Client keeps local PTYs running.
3. Local CLI remains attached and interactive.
4. Client attempts reconnect in the background.

Result:

- Network failure does not destroy local usability.

### 5.4 Reconnect Flow

Trigger:

- A previously disconnected client reconnects

Preconditions:

- Credentials remain valid

Main path:

1. Client reconnects to server.
2. Client re-authenticates.
3. Client republishes node and session state.
4. Server merges sessions back into the aggregate registry.
5. Existing session identity is restored when possible.

Result:

- Server-side visibility returns without creating duplicate session identities.

## 6. Flow Invariants

Every interaction flow must preserve:

- One focused session per console
- No switching during partial input
- At most one automatic switch opportunity per `Enter`
- Raw terminal output as the primary viewport
- Local CLI usability even when network mode is enabled

## 7. Suggested Sequence Priorities

Recommended implementation order for flows:

1. Local workspace start
2. Local session creation
3. Run agent command inside a session
4. Typing and submit
5. Manual switch
6. Automatic scheduling
7. Peek
8. Access point and registration
9. Server workspace interaction
10. Mirrored interaction
11. Disconnect and reconnect
