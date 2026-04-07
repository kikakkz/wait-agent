# WaitAgent PRD

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`  
Working name: `WaitAgent`

## 1. Product Definition

One-line definition:

> Let multiple AI agent sessions share one terminal instead of forcing the user to move across many terminals. These sessions may come from one machine or many machines.

WaitAgent is not an agent, not an IDE, and not an orchestrator. It is a:

> Terminal-native interaction scheduler

It sits between the user and multiple agent sessions and decides:

- Which session is currently visible
- Which session receives user input
- Which session is most likely worth the user’s attention
- How to switch safely without breaking raw TTY behavior

WaitAgent supports two deployment modes:

- `Local mode`: multiple sessions run on one machine and are aggregated locally
- `Network mode`: the user configures one access point; local sessions become visible on a server-side console, while the local CLI remains fully interactive and both sides stay synchronized

## 2. Background and Problem

Multi-agent workflows are already real. Developers increasingly run multiple AI agents in parallel for tasks such as:

- Bug fixing
- Test execution and automated repair
- Refactoring
- Diff review
- Deployment and environment checks

The real bottleneck is no longer whether multiple agents can run in parallel. The bottleneck is:

> When multiple agents run concurrently and stop at different times waiting for human input, how can one user safely and efficiently handle them through a single terminal workflow?

Typical pain points:

- Too many terminals, tabs, or panes to monitor
- Multiple agents may wait for user input at the same time
- Sessions across multiple machines are hard to manage from one place
- The user cannot easily tell which session should be handled next
- Even checking background progress requires an explicit context switch
- Input can easily be sent to the wrong session
- Human attention gets fragmented across concurrent waiting sessions

WaitAgent does not try to make agents smarter. It tries to:

> Compress concurrent multi-agent execution into a human-manageable serial confirmation flow.

## 3. Product Goals

### 3.1 Goals

- Support multiple independent agent sessions behind one terminal experience
- Support sessions from multiple machines inside one interaction plane
- Expose only one session for active interaction at a time within a given console
- Keep local mode and network mode behaviorally consistent
- Detect sessions that are likely waiting for user input
- Allow at most one automatic switch after the user submits input
- Preserve raw TTY semantics and avoid changing agent behavior or command habits
- Let a user configure a single access point and have sessions become naturally available on the server side
- Keep both the local CLI and the server-side console interactive in network mode, with automatic synchronization

### 3.2 Success Criteria

- The user no longer needs to maintain a many-terminal mental model
- The user can manage sessions from multiple client machines on one server console
- The user does not lose the local CLI interaction model when enabling network mode
- The user can always tell where input is going within the active console
- Waiting sessions are surfaced quickly enough to matter
- Automatic switching does not interrupt continuous interaction in the current session
- The system remains transparent to the agents themselves

## 4. Non-goals

The MVP does not include:

- Dashboard UI
- Multi-panel UI
- Visual diff UI
- Orchestration rules
- Prompt or semantic analysis
- AI summaries
- Automatic approvals
- Web inbox or browser console
- Team approval workflows
- MCP integration

## 5. Target Users

Core users:

- Developers using `Claude Code`, `Codex CLI`, `Kilo`, or similar CLI agents
- Terminal-heavy engineers
- Users running `2~10+` agent sessions in parallel
- Users who spread agents across multiple dev machines, remote hosts, or container hosts
- Users who frequently need manual confirmation for diffs, commands, fixes, or deployment steps

These users generally:

- Prefer CLI-native workflows
- Care strongly about TTY fidelity
- Do not want to move into a heavy IDE-style management surface
- Are highly sensitive to input being sent to the wrong place

## 6. Design Principles

### 6.1 P0: Must Not Break

- `100% TTY passthrough`
- `No semantic parsing`
- `No agent behavior modification`
- `No command habit changes`

### 6.2 P1: Experience Principles

- `Single focus`: only one session is visible per attached console at a time
- `Automatic but controllable`: the system helps schedule attention without hijacking control
- `Input safety`: no lost input, no misrouted input, no cross-session writes
- `Continuous interaction protection`: do not switch away while the current session is still in the same interaction flow
- `Minimal UI`: do not turn the terminal into an IDE
- `Deployment consistency`: local mode and network mode should feel like the same product
- `Mirrored interaction`: once network mode is enabled, local CLI and server-side views stay synchronized

## 7. Core Concepts

### 7.1 Session

A session is one agent process attached to one PTY.

In network mode, a session belongs to a client node but is visible through the server.

### 7.2 Focus

The session that is currently visible and receiving input within one attached console.

This is scoped per console, not globally.

Implications:

- A single WaitAgent UI instance has exactly one focus at a time
- In network mode, the server console and the local client console each maintain their own focus
- The same session may be attached to and interacted with from multiple consoles

### 7.3 Waiting

A heuristic state indicating that a session is likely waiting for user input.

This is not a protocol-level truth. It is a scheduling signal.

### 7.4 Waiting Queue

Sessions that enter the waiting state are ordered by FIFO for scheduling.

### 7.5 Switch Lock

Once an automatic switch happens, the system locks further automatic switching until the next user action that unlocks it.

### 7.6 Peek

Read-only inspection of a background session without changing the current focus or taking over input.

### 7.7 Node

A machine running WaitAgent Client.

A node may be:

- A local development machine
- A remote server
- A container host
- A CI or sandbox host

### 7.8 Server

WaitAgent Server is the aggregation and interaction surface that:

- Collects sessions from multiple nodes
- Maintains a global session registry and aggregate view
- Maintains its own focus and waiting queue for the server console
- Receives events from attached consoles and routes them to the correct node
- Broadcasts session output to all attached consoles

### 7.9 Client

WaitAgent Client runs on the machine that owns the PTY and:

- Starts or adopts local agent processes
- Maintains local PTYs and screen buffers
- Synchronizes session output and state to the server
- Writes input coming from the local CLI or the server-side console into the same PTY
- Preserves full local interaction even after network mode is enabled

### 7.10 Session Address

In network mode, every session must have a globally unique identity.

Suggested format:

`<node-id>/<session-id>`

Example:

`devbox-1/claude-3`

### 7.11 Access Point

In network mode, the user configures a single access point.

After that:

- Local sessions automatically register with the server
- The local CLI keeps its normal interaction behavior
- The server side can see and interact with those sessions
- Output, input results, and state changes synchronize automatically

### 7.12 Attached Console

An attached console is any interactive terminal view connected to WaitAgent.

Examples:

- The local client CLI
- A server-side CLI
- Another attached view on the same machine

Each attached console follows the same single-focus rule, but different consoles may attach to the same session at the same time.

## 8. Core Interaction Model

### 8.1 Single-Focus Model

At any moment, within one attached console, only one session may:

- Render to the terminal
- Receive user input from that console
- Act as the current interaction context

Other sessions:

- Continue running in the background
- Keep accumulating output
- Do not render directly into that console

In network mode:

- Each attached console still has exactly one focused session at a time
- The server console and local client console use the same focus model
- Network mode must not remove local CLI interactivity

### 8.2 Automatic Scheduling Rule

Automatic scheduling may only happen after:

> The user submits input with `Enter`

Each `Enter` creates one scheduling opportunity:

- It may be consumed at most once
- If there is a waiting session, switch to the earliest one in the FIFO queue
- If there is no waiting session, keep the current focus

In network mode:

- The server console maintains an aggregate waiting queue across nodes
- Sessions from different nodes enter the same FIFO order
- Scheduling rules do not change just because the target session lives on another machine

At the same time, a local client console may keep its own local waiting queue so that local interaction still feels like single-machine mode.

### 8.3 Continuous Interaction Protection

This pattern should be treated as one continuous interaction and should not trigger a switch away:

`prompt1 -> user input -> current session continues output -> prompt2`

Therefore, the system should not switch immediately on `Enter`. It should:

- Arm one scheduling opportunity
- Observe the current session during the current interaction round
- Prefer staying on the same session if it keeps producing output
- Only consider spending the scheduling opportunity after the current output round has stabilized

This rule is required to satisfy both:

- Automatic scheduling only happens after user submission
- Continuous interaction in the current session must not be interrupted

### 8.4 Switch Lock

After one automatic switch:

- Lock further automatic switching
- Keep the lock until one of the following happens

Unlock conditions:

- The user submits input again
- The user manually switches sessions

Goal:

- Guarantee at most one automatic switch per submitted input
- Prevent multiple waiting sessions from repeatedly stealing attention

### 8.5 Manual Operations

Base operations:

- `Enter`: submit input
- `Ctrl + Tab`: next session
- `Ctrl + Shift + Tab`: previous session
- `Ctrl + Number`: jump to a specific session

Useful additional operations:

- Filter by node
- Jump directly to `<node-id>/<session-id>`

### 8.6 Peek

Definition:

> Temporarily inspect the latest screen state of a non-focused session in read-only mode, without taking over input, without changing focus, and without triggering automatic scheduling.

Constraints:

- stdin still belongs to the current focus within that console
- Peek must not write input to the target session
- Peek does not change the scheduler lock state
- Exiting Peek restores the original focused screen

Typical uses:

- Check whether an agent is stuck
- Check whether a session is already waiting for input
- Inspect progress without interrupting the current flow

### 8.7 Mirrored Multi-Console Interaction

In network mode, the same session may be attached to both the local CLI and the server console at the same time.

Synchronization rules:

- stdout and screen state are synchronized to all attached consoles
- stdin from any attached console is written into the same underlying PTY
- PTY output is broadcast back to all attached consoles

This means:

- Input sent from the local CLI becomes visible on the server side
- Input sent from the server side becomes visible on the local CLI
- WaitAgent does not attempt semantic conflict resolution across concurrent inputs
- Input is written in arrival order at the PTY boundary

## 9. Session State Model

### 9.1 States

| State | Meaning |
| --- | --- |
| `running` | The session has had recent output activity |
| `waiting_input` | The session is likely waiting for user input |
| `idle` | No clear activity is happening |
| `exited` | The process has exited |

### 9.2 Detection Rules

Requirements:

- Non-intrusive
- No dependency on agent-specific protocols
- No dependency on semantic understanding

MVP heuristics:

- There was recent output
- Output then stops for more than `X ms`
- No new stdin arrived during that period
- The process is still alive

Optional strengthening signals:

- CPU idle
- TTY mode change
- Stable cursor behavior
- Alt-screen state

Note:

`waiting_input` is a high-probability heuristic state in the MVP, not an authoritative truth.

## 10. System Architecture

Local mode:

```text
Shell alias
   ↓
Proxy / PTY Manager
   ↓
Multiple PTYs (one per agent)
   ↓
Session Manager
   ↓
Focus Scheduler
   ↓
Renderer + Input Controller
   ↓
Single Terminal Output
```

Network mode:

```text
User Terminal
   ↓
WaitAgent Server
   ↓
Global Session Registry + Aggregate Scheduler
   ↓
Persistent Connections
   ↓
WaitAgent Clients (multiple machines)
   ↓
Local PTY Managers
   ↓
Agent Processes on each machine
```

Experience invariants:

- Local mode and network mode use the same CLI and interaction model
- Network mode adds synchronization and aggregation; it does not replace the local experience

### 10.1 PTY Proxy Layer

Responsibilities:

- Start or adopt agent processes
- Create one PTY per agent
- Pass through stdin, stdout, and stderr
- Handle ANSI, cursor control, raw mode, and resize

### 10.2 Session Manager

Responsibilities:

- Manage session lifecycle
- Manage session metadata and state
- Maintain screen buffers
- Handle exits, cleanup, and crash isolation

### 10.3 Focus Scheduler

Responsibilities:

- Track current focus
- Track waiting queues
- Implement FIFO scheduling
- Implement enter-triggered scheduling
- Implement switch lock

### 10.4 Input Controller

Responsibilities:

- Route stdin only to the focused session of the current console
- Prevent switching while the user has unsubmitted input
- Prevent input loss or cross-session writes

### 10.5 Renderer

Responsibilities:

- Render only the focused session for the current console
- Restore the full visible context when switching
- Avoid semantic re-rendering
- Avoid summary-first views

### 10.6 WaitAgent Server

Responsibilities:

- Accept long-lived connections from multiple clients
- Maintain the global session registry
- Maintain the server console’s own waiting queue and focus scheduler
- Route user input to the correct client
- Broadcast PTY output to all attached consoles
- Provide a cross-node aggregate interaction surface

### 10.7 WaitAgent Client

Responsibilities:

- Start or adopt local agent processes
- Maintain local PTYs and screen buffers
- Report session output, state changes, and lifecycle events
- Accept input, resize, and attach requests from the server
- Write both local CLI input and remote server input into the same local PTY
- Keep local sessions alive even when the network is disrupted

### 10.8 Network Transport

Requirements:

- Maintain persistent connections between server and clients
- Carry stdout chunks, stdin events, resize events, lifecycle events, and state changes
- Avoid semantic interpretation of agent traffic
- Support reconnect and session recovery
- Support event broadcast for sessions attached by multiple consoles

### 10.9 Global Session Namespace

In network mode, the server must maintain a global namespace in order to:

- Uniquely identify remote sessions
- Render machine origin clearly
- Route input and attribute logs correctly

## 11. UI Rules

WaitAgent UI must stay minimal:

```text
──────────────
[devbox-1/agent-2] active

...raw terminal output...

──────────────
2 sessions waiting
```

Must not:

- Use card-based UI
- Use multi-panel layouts
- Use split views
- Use dashboard-style management surfaces
- Change the underlying agent output style

Allowed minimal UI elements:

- Top status line with current session identity
- Node identity when needed, for example `devbox-1/claude-2`
- Bottom waiting count
- Small switch feedback messages

## 12. Critical Edge Cases

### 12.1 ANSI and Cursor Control

- Pass through everything
- Do not rewrite escape sequences
- Do not interpret output semantically

### 12.2 Input Protection

- Do not allow switching while the user has in-progress unsubmitted input
- Applies to both automatic and manual switching

### 12.3 Resize

- Keep PTY dimensions synchronized
- Preserve consistent behavior across sessions
- In network mode, the server forwards terminal size to the correct client, which applies it to the local PTY

### 12.4 Background Output

- Background sessions keep accumulating output
- Background output must not interrupt the current focused console
- Switching back restores the full session context

### 12.5 Crash Handling

- Remove exited sessions automatically
- Do not affect other sessions
- If the focused session exits, switch to the next available session

### 12.6 Network Disconnects

- If a client disconnects from the server, mark that node as `offline`
- Keep remote session metadata visible on the server, but mark sessions unreachable
- If a focused session on a console becomes unreachable, release focus and switch to the next available session
- Reconnection should restore prior session identity whenever possible
- Client-side local CLI interaction should continue for local sessions even when the server is disconnected

### 12.7 Multi-Console Input Conflicts

- The same session may be attached to by multiple consoles at once
- If multiple consoles type at the same time, bytes are written into the same PTY in arrival order
- WaitAgent does not perform semantic conflict merging
- The product should provide lightweight awareness signals such as `remote typing` or `another console attached`, but should not forcibly remove interactivity from either side

### 12.8 Security and Access Control

- Client-to-server connection must require explicit authentication
- Session input and output are sensitive data and must not allow anonymous plaintext access
- Minimum requirements include node identity, connection authorization, and revocable credentials

### 12.9 Continuous Prompt Flow

- `prompt1 -> input -> prompt2` is treated as one continuous interaction flow
- The waiting queue must not steal focus in the middle of that flow

## 13. MVP Scope

### 13.1 Phase 1: Local Single-Machine Version

- Alias injection
- PTY proxy
- Multi-session management
- Single-focus switching
- One automatic switch opportunity after input submission
- Manual switching
- Waiting heuristics
- Peek
- Resize synchronization
- Crash isolation

### 13.2 Phase 2: Network Version

- WaitAgent Server
- WaitAgent Client
- Multi-node session aggregation
- Global session namespace
- Cross-machine aggregate waiting queue
- Mirrored server/client interaction
- Multi-console attach and broadcast
- Access point configuration
- Reconnect and offline-node handling
- Basic authentication

### 13.3 Not in Scope Yet

- Dashboard
- Orchestration
- AI analysis
- Visual diff UI
- Web inbox
- Session recording
- Multi-user approvals
- Auto-approve rules
- MCP integration
- Agent profiling

## 14. Acceptance Criteria

Functional acceptance:

- Support at least `3` simultaneous sessions
- Foreground input must never enter background sessions within a console
- Waiting sessions must enter FIFO order correctly
- One `Enter` must trigger at most one automatic switch
- Continuous interaction must not cause false switching
- Peek must not change input ownership
- Resize must not break session behavior
- One session crash must not affect others
- Support at least `2` connected nodes
- The server must interact with sessions across multiple nodes
- Disconnecting one client must not break interaction with sessions on other nodes
- After configuring an access point, both local CLI and server console can interact with the same session
- Input on one side must result in synchronized terminal output on the other side

Experience acceptance:

- Users switch terminals significantly less often
- Users no longer need to poll background sessions constantly
- Users can clearly tell where input goes within the active console
- Users do not need to log into multiple machines just to take over sessions
- Users do not need to change local CLI habits to benefit from server aggregation

## 15. Why Current Tools Do Not Solve This

WaitAgent is not solving whether multiple agents can run. It is solving:

> Human interaction scheduling for multi-agent CLI workflows

That means:

- Which session should get attention now
- Where input should go
- How to avoid constant context switching
- How to avoid sending input to the wrong place
- How to preserve TTY fidelity while turning concurrent agent work into a manageable human confirmation flow

### 15.1 tmux / Zellij

These tools solve:

- Multi-session terminal hosting
- Panes, tabs, and workspace management

They do not solve:

- Which session is waiting for the user
- Waiting-aware FIFO scheduling
- One automatic switch opportunity after `Enter`
- Continuous interaction protection
- Read-only Peek semantics
- Cross-machine session scheduling with mirrored interaction

Conclusion:

> tmux and Zellij are terminal multiplexing infrastructure, not interaction schedulers.

### 15.2 Claude Code / Codex CLI / Kilo-style CLI Agents

These tools solve:

- Single-agent execution power
- Local terminal agent workflows
- Tool use, sub-tasks, and code modification inside one agent context

They do not solve:

- How multiple independent sessions share one terminal workflow
- Human input scheduling across many sessions
- A vendor-neutral single-focus interaction layer
- Safe cross-machine aggregation behind one interaction surface

Even where subagents exist, they are still internal delegation inside one agent workflow, not:

> Terminal-level interaction reuse across multiple independent sessions

### 15.3 Codex App / Cursor Background Agents / Warp / GitHub Copilot Cloud Agent

These products solve:

- Parallel agent execution
- Background agent management
- Sidebar or task-list driven UIs
- Cloud or async coding workflows

They do not solve:

- Raw TTY-preserving session reuse
- A terminal-first single-focus interaction model
- A vendor-neutral proxy layer that preserves normal CLI habits
- A server that aggregates sessions across machines while keeping the local CLI naturally interactive

They usually depend on:

- Their own app surface
- Their own sidebar or web workflow
- A panel-based management model

WaitAgent solves a different layer:

> I do not want a new agent platform. I want to keep using the terminal while making multi-agent human confirmation manageable.

### 15.4 Summary

Existing tools solve different layers:

- `tmux / Zellij`: terminal multiplexing
- `Claude Code / Codex CLI`: single-agent CLI execution
- `Codex App / Cursor / Warp`: vendor-owned multi-agent management
- `Copilot Cloud Agent`: asynchronous background PR workflows

WaitAgent targets the missing layer between them:

> A human-in-the-loop interaction scheduler for multi-agent CLI workflows

And, more specifically:

> A mirrored interaction layer between local CLI usage and a remote aggregate terminal surface

## 16. Market Opportunity

This product exists first to solve a real operator problem, not to satisfy a generic market thesis.

### 16.1 Why Now

- Multi-agent execution is already real
- Developers increasingly accept agents running concurrently in the background
- Human confirmation is still inherently serial
- There is still no standard terminal-native, cross-vendor, low-intrusion interaction scheduler
- There is also no clear standard for aggregating sessions across machines while preserving local CLI behavior

### 16.2 Opportunity Boundary

This is not a product every developer needs.

It is a dense problem for:

- CLI-heavy users
- Multi-agent users
- Operators who already manage sessions across multiple contexts

Once it works well, switching away becomes expensive.

### 16.3 Main Risks

- Being reduced to a `tmux plugin`
- Being absorbed by a major agent vendor
- Being too narrow as a market

This means the product must stay focused and avoid turning into a broad platform too early.

## 17. Product Boundary

WaitAgent is not a multi-agent platform. It is:

> A terminal-level interaction scheduler

More precisely:

- Not an agent tool
- Not an IDE
- Not an orchestrator
- A terminal-level interaction scheduler for human-in-the-loop workflows

It may run as a local single-machine tool, or as:

> A server that aggregates multiple client nodes into one terminal-native interaction layer

One-line summary:

> Let multiple AI agents share one terminal instead of making the user switch between many terminals.

## 18. Recommended Follow-up Documents

Based on this PRD, the next documents should be:

- `architecture.md`
  Defines PTY ownership, scheduler state machines, buffers, renderer behavior, and attach semantics
- `protocol.md`
  Defines server/client event transport, authentication, reconnection, and multi-console synchronization
- `mvp-plan.md`
  Defines phased implementation scope, milestones, and validation criteria

## 19. Reference Notes

The current product and market assessment was informed by public information about:

- OpenAI Codex CLI
- OpenAI Codex product positioning
- Anthropic Claude Code and subagents
- Cursor Background Agents
- Warp Agent Platform
- tmux
- Zellij
- GitHub Copilot coding agent

These references were used to validate:

- Multi-agent execution is already a real behavior
- Major vendors are already building background and parallel agent workflows
- A terminal-native, cross-vendor, single-focus interaction scheduler is still not standardized
