# WaitAgent Interaction Flows

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

> Note
> Remote/network flows in this document are deferred future design material.
> The current implemented product surface is the local tmux-native workspace path.

## 1. Purpose

This document defines the main user interaction flows for WaitAgent.

It describes how the product should behave step by step in:

- Local mode
- Deferred future remote mode
- Focus switching
- Waiting-state visibility
- Deferred future mirrored interaction
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
6. Focus remains on the current session.

Result:

- The session receives the input.
- The current session remains the active interaction target.

### 3.5 Post-Submit Focus Stability Flow

Trigger:

- User has just submitted input to the focused session

Preconditions:

- Current focused session is still active

Main path:

1. User submits input.
2. Current session continues producing output.
3. WaitAgent keeps focus on the same session.
4. The user may continue interacting with that session without interruption.

Result:

- Focus stays on the current session.

Edge notes:

- This is the rule that protects `prompt1 -> input -> prompt2`.

### 3.6 Waiting Signal Update Flow

Trigger:

- A background session appears to be waiting for user input

Preconditions:

- Another session produces output that looks like a prompt, approval request, or other waiting state

Main path:

1. WaitAgent refreshes its session metadata.
2. The session is marked as likely waiting.
3. Chrome updates counts or badges to surface that state.

Result:

- The user can notice that another session may need attention.

### 3.7 Manual Switch Flow

Trigger:

- The user invokes next, previous, or direct focus selection

Preconditions:

- No unsubmitted input is in progress

Main path:

1. User triggers manual switch.
2. WaitAgent resolves the target session.
3. Console focus changes immediately.
4. Renderer restores the target session.

Result:

- The user now interacts with the selected session.

### 3.8 Fullscreen Flow

Trigger:

- The user enters fullscreen on the active session

Preconditions:

- A focused session exists

Main path:

1. User triggers fullscreen.
2. WaitAgent zooms the active main interaction surface.
3. The active session continues receiving normal input and output.
4. User exits fullscreen.
5. WaitAgent restores the fixed workspace chrome.

Result:

- The user temporarily gets a larger view without changing the active session.

## 4. Network Flows

### 4.1 Future Remote Configure Flow (Deferred)

Trigger:

- This is a deferred future flow for remote session connection

Preconditions:

- Remote session architecture has been designed
- Required credentials and transport configuration exist

Main path:

1. User starts the future remote-connect entrypoint for a local workspace.
2. The local workspace establishes a connection to the server.
3. The local workspace authenticates.
4. The local workspace publishes node metadata.
5. The local workspace registers all local sessions.
6. Server adds the node and sessions to its aggregate registry.

Result:

- Local sessions become visible on the server side.
- Local CLI behavior remains unchanged.

### 4.2 Future Remote Console Flow (Deferred)

Trigger:

- This is a deferred future flow for remote session aggregation

Preconditions:

- At least one remote workspace is connected

Main path:

1. User starts the future remote aggregation runtime.
2. The runtime builds the aggregate session view.
3. The runtime selects an initial focused session.
4. Renderer displays that focused session.

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

### 4.5 Server-Side Attention Visibility Flow

Trigger:

- The server console observes that one or more targets are waiting

Preconditions:

- Multiple sessions from multiple nodes may be waiting

Main path:

1. Server-side scheduler observes the aggregate waiting queue.
2. Chrome updates waiting counts, badges, or “next” labels.
3. Focus remains on the current target until the user chooses to switch.
4. Local client consoles keep their own focus unchanged.

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
- Waiting state may raise attention cues but must not move focus automatically
- Raw terminal output as the primary viewport
- Local CLI usability even when network mode is enabled

## 7. Suggested Sequence Priorities

Recommended implementation order for flows:

1. Local workspace start
2. Local session creation
3. Run agent command inside a session
4. Typing and submit
5. Manual switch
6. Waiting-state visibility
7. Fullscreen
8. Access point and registration
9. Server workspace interaction
10. Mirrored interaction
11. Disconnect and reconnect
