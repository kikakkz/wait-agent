# WaitAgent Functional Design

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

> Note
> Remote and cross-machine sections in this document are future design notes.
> The current public command surface is local-only: `waitagent`, `waitagent attach`, `waitagent ls`, and `waitagent detach`.

## 1. Purpose

This document defines the user-visible functions of WaitAgent and the exact expected behavior for each function.

It complements:

- [wait-agent-prd.md](wait-agent-prd.md)
- [architecture.md](architecture.md)

## 2. Functional Scope

WaitAgent provides three layers of functionality:

- Workspace hosting
- Session hosting
- Session attention visibility
- Deferred future remote aggregation

## 3. Functional Areas

### 3.1 Workspace Lifecycle

#### 3.1.1 Start Workspace

The system must allow a user to start one local WaitAgent workspace through:

- `waitagent`

Expected behavior:

- Bootstrap one local workspace runtime
- Attach the current terminal as the active console
- Restore or initialize the local session registry
- Render the currently focused session or an empty workspace state

#### 3.1.2 Workspace-Network Extension

If an access point is configured:

- The same local workspace connects to the server
- Existing local sessions are published automatically
- Local interaction remains available
- Server-side interaction becomes an additional synchronized surface

The user must not need a separate public `client` entrypoint.

### 3.2 Session Lifecycle

#### 3.2.1 Start Session

The system must allow a user to start a new managed session from inside WaitAgent.

Expected behavior:

- Allocate or adopt a PTY
- Launch the session with a shell-capable environment or configured template
- Register the session
- Make the session eligible for focus and scheduling

Inside the session, the user may run `codex`, `claude`, `kilo`, `cd`, scripts, or normal shell commands without WaitAgent rewriting them.

#### 3.2.2 Adopt Existing Session

The system may support adopting an externally started session later, but this is not required for the MVP unless the PTY adoption path is technically straightforward.

#### 3.2.3 Exit Session

When a session exits:

- Mark it `exited`
- Remove it from eligible scheduling sets
- Preserve enough metadata for short-term UI continuity
- If focused, move focus to the next valid session

### 3.3 Console Runtime and Mirrored Visibility

The current public command surface is local-only.

Current model:

- `waitagent` starts a local workspace
- `waitagent attach [<target>]` attaches to an existing local tmux-managed workspace session
- `waitagent ls` and `waitagent detach [<target>]` manage the local tmux session set

Future note:

- Remote session connection and mirrored visibility need a fresh design on top of the tmux-native local architecture

#### 3.3.1 Multi-Console Attach

The same session may be attached from more than one console.

Behavior:

- All consoles receive synchronized output
- Any console may send input
- Concurrent input is serialized by arrival order

### 3.4 Focus Management

#### 3.4.1 Initial Focus Selection

When a console attaches:

- If there is exactly one runnable session, focus it
- Else if there is a most recent interactive session, reuse it
- Else choose the earliest active session

#### 3.4.2 Manual Focus Switch

The user may manually switch to:

- Next session
- Previous session
- Specific indexed session
- Specific session address

Effects:

- Focus changes immediately
- Scheduler lock clears
- Renderer restores the target screen

#### 3.4.3 Focus Loss

If the focused session exits or becomes unreachable:

- Release focus
- Choose the next eligible session
- Render a minimal transition notice

### 3.5 Input Handling

#### 3.5.1 Typing State

While the user has in-progress unsubmitted input:

- Automatic switching is forbidden
- Manual switching is forbidden

This rule prevents misrouting partially typed commands.

#### 3.5.2 Input Submission

When the user presses `Enter`:

- Send the input to the focused session
- Arm one automatic scheduling opportunity
- Start the continuation observation window

#### 3.5.3 Mirrored Input

In network mode:

- Input from local CLI appears in the server-side attached view
- Input from the server-side attached view appears in the local CLI
- Resulting PTY output is synchronized to both

### 3.6 Automatic Scheduling

#### 3.6.1 Entry Condition

Automatic scheduling may only be considered after input submission.

#### 3.6.2 Continuation Observation

After input submission:

- If the current session continues producing output as part of the same interaction round, stay on it
- Only when that round stabilizes may the scheduler consume the switch opportunity

#### 3.6.3 Waiting Queue Selection

If a switch opportunity is available and the current interaction has stabilized:

- Choose the earliest waiting session in FIFO order

#### 3.6.4 Locking

After an automatic switch:

- Lock further automatic switching
- Unlock on next input submission or manual switch

### 3.7 Waiting Detection

#### 3.7.1 Required Signals

The system must infer waiting without agent protocol support.

MVP signals:

- Recent output occurred
- Output became inactive for threshold `X`
- No input was sent recently
- Process is still alive

#### 3.7.2 Waiting Queue Update

When a session transitions into waiting:

- Add it to the queue if not already present
- Preserve FIFO order by first waiting timestamp

When a session becomes active again:

- Remove it from the waiting queue

### 3.8 Fullscreen

Fullscreen is a local viewport mode for the active session.

Behavior:

- Expand the active main interaction surface
- Preserve normal shell and TUI behavior
- Exit cleanly back to the fixed workspace chrome

### 3.9 Network Access Point

#### 3.9.1 Configure Access Point

The user may configure one server access point for a local WaitAgent workspace.

Behavior:

- The local workspace connects to the server
- Local sessions register automatically
- Server-side consoles can view and interact with those sessions
- Local behavior remains unchanged

#### 3.9.2 Mirrored Workspace Behavior

When a workspace is connected:

- Sessions still start locally
- Local ownership does not change
- Sessions become eligible for mirrored server-side visibility
- The user still does not need a separate public `client` entrypoint

#### 3.9.3 Disconnect

If the mirrored runtime loses connection:

- Mark the node offline on the server
- Keep local sessions running
- Prevent server-side writes to unreachable sessions
- Restore session linkage after reconnect when possible

### 3.10 Resize Synchronization

When the console terminal size changes:

- Update the focused console runtime
- Propagate size change to the owning PTY
- In network mode, forward resize through the server/client path

### 3.11 Minimal UI

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

### 4.1 Entry Commands

- `waitagent`
- `waitagent attach <session>`
- `waitagent ls`
- `waitagent detach [session]`

### 4.2 Future Remote Access

Future remote connection and management commands are deferred until the remote model is redesigned on top of the local tmux-native architecture.

### 4.3 Workspace Commands

- `new-session`
- `next-session`
- `prev-session`
- `focus-session <session>`

These may be implemented as keyboard shortcuts, prompt commands, or lightweight control actions inside the workspace rather than traditional shell subcommands.

## 5. Functional Invariants

The following must hold in every mode:

- One console has one focused session
- Input never goes to a non-focused session within the same console
- Input-in-progress blocks switching
- Session output remains raw terminal output

## 6. Acceptance Matrix

### 6.1 Local Mode

- Start one `waitagent`
- Create three sessions inside it
- Run an agent command such as `codex` or `claude` inside one session
- Focus one
- Type partial input
- Verify switch is blocked
- Submit input
- Verify focus remains stable through the interaction
- Verify fullscreen preserves normal interaction

### 6.2 Network Mode

- Connect two workspace nodes
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
