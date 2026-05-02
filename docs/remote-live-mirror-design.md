# WaitAgent Remote Live Mirror Design

Version: `v1.0`
Status: `Accepted for task.t5-08c4d3a -> task.t5-08c4d3d`
Date: `2026-05-02`

## 1. Purpose

This document closes the remaining design gap between:

- remote session discovery and activation
- remote control-plane routing by `session_id`
- the accepted product requirement that opening a remote session must feel the
  same as opening a local session

The gap is not target discovery.
The gap is the live mirror lifecycle for one opened remote session.

This document defines:

- how a server requests a client node to start or stop mirroring one session
- how the client binds that request to one real PTY-owning session
- how the first visible screen becomes available to the server-side surface
- how multiple consoles share one mirrored session without duplicating PTY
  ownership
- how disconnect, reopen, and teardown behave

It complements:

- [remote-session-foundation.md](remote-session-foundation.md)
- [remote-network-completion-plan.md](remote-network-completion-plan.md)
- [remote-node-connection-architecture.md](remote-node-connection-architecture.md)
- [protocol.md](protocol.md)

## 2. Problem Statement

The current codebase already supports:

- node-scoped `--connect`
- remote session rows in the shared catalog
- remote target activation into the main slot
- authority input and output transport seams

But the opened remote session still falls back to placeholder content in the
real cross-host path.

That happens because the current product path still lacks one complete
session-scoped live mirror contract:

- the server can open a remote session surface
- the client can publish remote session metadata
- but no accepted production message currently tells the PTY-owning node to
  start mirroring one concrete session into the opened surface

So the remaining gap is not cosmetic rendering.
It is the missing session-scoped remote mirror lifecycle.

## 3. Product Rule

Opening a remote session must produce the same visible terminal surface the
user would see on the client host for that same session.

This means:

- the server-side main slot must not stop at metadata or placeholder state once
  the authority node is available
- the opened remote session must stream real PTY output from the PTY owner
- server-side input must reach that same PTY
- the visible remote session must preserve terminal parity rather than degrade
  into a second interaction contract

The accepted user-visible sequence is:

1. the remote session row already exists in the sidebar because the node
   published its backend-owned local session set
2. the user opens that row in the workspace main slot or server console
3. the server requests a live mirror for that `session_id`
4. the PTY-owning node starts mirroring that session
5. the server surface renders the same current screen and then follows the live
   PTY byte stream

## 4. Non-Negotiable Rules

1. Session-scoped mirror, not node-scoped placeholder
   Live mirroring must be requested and owned per `session_id`.
2. PTY ownership stays remote
   The PTY host remains the only machine that reads and writes the live PTY.
3. No server-owned fake PTY bootstrap
   The server must not fabricate a local PTY copy for remote sessions.
4. One node connection, many mirrors
   Mirror requests are multiplexed over the existing node session.
5. One mirrored session, many observers
   Multiple server-side consoles may open the same remote session, but they
   share one mirror lifecycle on the PTY-owning node.
6. Attachment is observer scope only
   `attachment_id` remains a console-local observer handle, never the routing
   identity for the mirror itself.
7. Remote and local parity means visible parity
   The user should see the actual remote terminal state, not a session card or
   transport status screen, once the authority node is live.

## 5. Terms

- `mirror session`
  The live PTY-output stream for one publishable `session_id`.
- `mirror owner`
  The PTY-owning node runtime that starts or stops a live mirror for one local
  session.
- `mirror route`
  The server-side per-session runtime state that receives PTY output for that
  remote session and fans it out to local observers.
- `mirror request`
  An explicit protocol message that asks the PTY-owning node to start or stop
  mirroring one session.
- `bootstrap replay`
  The first bounded screen or transcript state sent after a mirror is opened so
  the observer does not stay on placeholder content until the next fresh PTY
  write.

## 6. Accepted Runtime Model

The accepted ownership split is:

### 6.1 Server Side

The server owns:

- the shared remote session catalog
- which consoles are attached to each remote `session_id`
- ordering of remote input
- the per-session mirror route state
- observer fanout and local render state

The server does not own:

- the live PTY
- PTY output generation
- remote screen truth

### 6.2 PTY-Owning Client Node

The PTY-owning node owns:

- local session discovery and publication
- the actual PTY for each publishable session
- starting or stopping PTY output mirroring for a requested `session_id`
- producing authoritative output bytes and bootstrap replay state

### 6.3 Node Session Transport

The node-scoped connection carries:

- session publication
- mirror open and close requests
- ordered input delivery
- PTY resize delivery
- PTY output and bootstrap replay frames

No second production transport connection is introduced for mirror lifecycle.

## 7. Protocol Additions

The accepted existing protocol is not enough because it leaves mirror lifecycle
implicit.

The protocol must gain explicit session-scoped mirror control messages.

The accepted additions are:

- `OpenMirrorRequest`
- `OpenMirrorAccepted`
- `OpenMirrorRejected`
- `CloseMirrorRequest`
- `MirrorBootstrapChunk`
- `MirrorBootstrapComplete`

These may reuse the existing `OpenTarget*` naming family if the proto owner
chooses to preserve that shape, but the product meaning is fixed:

- they open or close one live mirror for one `session_id`
- they do not represent a local tmux attach
- they do not create a server-owned PTY

### 7.1 Required Routing Fields

For mirror lifecycle messages:

- `authority_node_id` identifies the PTY-owning node
- `session_id` is the primary routing identity
- `target_id` remains the shared catalog identity
- `attachment_id` may be present for diagnostics, but it must not decide which
  mirror route exists
- `console_id` identifies the initiating console when needed for telemetry or
  resize-authority rules

### 7.2 Mirror Open Semantics

When the server opens a remote session in any console:

- the server ensures there is a server-side mirror route for that `session_id`
- if no live mirror exists yet, the server sends one `OpenMirrorRequest`
- repeated opens from additional consoles do not create duplicate mirror opens
- the PTY-owning node either accepts the mirror or rejects it explicitly

### 7.3 Bootstrap Semantics

After a successful mirror open:

- the PTY-owning node must send bounded bootstrap replay before or alongside
  normal live output
- bootstrap replay is session-scoped and ordered
- bootstrap replay may be encoded as transcript chunks, screen-state chunks, or
  another app-agnostic terminal form already accepted by the replay design
- the server must render bootstrap replay into the same observer state used for
  live output

The visible surface must therefore leave placeholder state as soon as bootstrap
replay begins, not only after the next fresh PTY write.

### 7.4 Mirror Close Semantics

When the last server-side observer for a remote `session_id` disappears:

- the server sends `CloseMirrorRequest`
- the PTY-owning node tears down the mirror if no other route still needs it
- the local session itself does not exit just because the mirror closed

## 8. Server Lifecycle

### 8.1 Open

On remote-session activation:

1. resolve the catalog row to `authority_node_id + session_id + target_id`
2. attach the local console as an observer of that `session_id`
3. ensure one live mirror route exists for that `session_id`
4. request mirror open if the route was not already live
5. render placeholder only until bootstrap or live output arrives

### 8.2 Shared Observers

If another server-side console opens the same remote session:

- attach it to the existing server-side mirror route
- do not send a second mirror-open message
- replay current observer state locally to the new console

### 8.3 Close

If one console leaves but others remain:

- remove only that observer
- keep the session mirror alive

If the last observer leaves:

- close the mirror route
- send `CloseMirrorRequest`

## 9. PTY-Owner Lifecycle

### 9.1 Mirror Start

On `OpenMirrorRequest` for one local publishable session:

- verify the requested `session_id` is local and publishable
- resolve its PTY-owning target host
- attach or reuse one local mirror runtime for that session
- emit bootstrap replay
- continue streaming live PTY output

### 9.2 Mirror Reuse

If the PTY-owning node receives another mirror-open for the same session while
that session is already mirrored:

- increment observer interest in node-local mirror state if needed
- do not open a second PTY output pipe
- do not duplicate the local authority runtime per console

### 9.3 Mirror Stop

On `CloseMirrorRequest`:

- remove the requesting server-side interest
- stop the local mirror only when no live route still needs it

## 10. Bootstrap And Replay Rule

The accepted rule is:

- a newly opened observer must receive the current visible terminal state
  without waiting for the next user keystroke or PTY write

So the implementation must not rely on:

- placeholder lines until the next natural PTY output
- fake local snapshots synthesized on the server
- console-specific side channels outside the accepted node session

The implementation may reuse the existing bounded replay design from
`task.t5-08a3`, but the replay must now be wired into the real session-open
path.

## 11. Failure And Reconnect Semantics

### 11.1 Open Failure

If the PTY-owning node cannot mirror the requested session:

- send `OpenMirrorRejected`
- keep the server-side row present if the session is still published
- render a clear temporary transport or session-open failure state in the
  opened surface

### 11.2 Node Disconnect

If the node connection drops:

- mark the server-side mirror route disconnected
- keep the remote session row present according to the accepted offline
  projection model
- do not fabricate stale live output

### 11.3 Reconnect

After reconnect:

- remote session publication resynchronizes first
- any still-open remote observer causes the server to request mirror open again
- bootstrap replay runs again

Reconnect recovery is therefore reopen-based, not hidden socket reuse.

## 12. Implementation Split

The accepted remaining split is:

1. `task.t5-08c4d3a`
   Freeze the session-scoped live-mirror design, protocol additions, and task
   split.
2. `task.t5-08c4d3b`
   Implement explicit mirror open or close protocol messages and server-side
   session-route ownership.
3. `task.t5-08c4d3c`
   Implement PTY-owner mirror lifecycle on the client node, including mirror
   reuse and teardown per `session_id`.
4. `task.t5-08c4d3d`
   Bind bootstrap replay and live output into the visible main-slot path and
   close end-to-end cross-host validation.

## 13. Anti-Goals

- do not reintroduce auto-switch or queue-driven focus changes
- do not key live remote routing by pane id
- do not keep placeholder-only remote surfaces as an accepted steady state
- do not solve this by inventing a second remote-only UI contract
- do not scatter direct socket access through runtime code outside the accepted
  transport facade and runtime boundaries
