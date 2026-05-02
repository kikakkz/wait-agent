# WaitAgent Remote Runtime Owner Architecture

Version: `v1.0`
Status: `Accepted for task.t5-08c4a -> task.t5-08c4d`
Date: `2026-04-30`

## 1. Purpose

This document defines the accepted runtime ownership model for remote sidebar
state after discovering that the current file-backed merge path is not an
acceptable product architecture.

It exists to make five decisions explicit before more remote-session work
lands:

- why file-backed remote sidebar state is rejected
- which runtime must own remote node and session state
- how `detach -> reattach` works without stale sidebar rows
- how local UI consumers fetch remote state without reading ad hoc files
- how the remaining `task.t5-08c4` work must be split into bounded slices

It complements:

- [remote-session-foundation.md](remote-session-foundation.md)
- [remote-network-completion-plan.md](remote-network-completion-plan.md)
- [remote-node-connection-architecture.md](remote-node-connection-architecture.md)

## 2. Problem Statement

The current default target-catalog path is not clean enough.

Today the sidebar-visible catalog is assembled by merging:

- local tmux sessions
- file-backed published remote targets
- file-backed discovered remote sessions

That architecture is rejected for the remote session product because it causes
all of the following failures:

1. stale remote sessions can appear with no live connection
2. sidebar state survives process death for the wrong reason
3. `detach` and later `reattach` are forced to depend on cache files instead of
   a real runtime owner
4. multiple processes can race on the same file-backed state without a proper
   ownership boundary
5. remote session visibility is no longer tied to actual node or backend
   liveness

Remote sidebar rows are runtime state, not durable application data.

## 3. Non-Negotiable Rules

1. No file-backed remote sidebar source
   Remote rows shown in sidebar, picker, or related activation surfaces must
   come from a live runtime snapshot, not from `/tmp`, TSV, or similar cache
   files.
2. One backend-scoped owner
   Each local WaitAgent backend or tmux socket must have exactly one accepted
   remote runtime owner for node and remote-session state.
3. Owner outlives attached UI clients
   `detach` must remove only the current attached UI surface. It must not erase
   remote node or session state while the backend remains alive.
4. Cold start means empty remote catalog
   If no live owner or node connection exists, the remote part of the sidebar
   must be empty.
5. Remote routing stays session-centric
   The owner is keyed by node for transport connection ownership and by session
   for remote target routing. `attachment_id` remains observer scope only.
6. Transport code must stay behind runtime or transport boundaries
   UI, registry, and higher application code must not recover by opening raw
   sockets or rereading fallback files directly.

## 4. Accepted Runtime Model

### 4.1 One Backend-Scoped Runtime Owner

The accepted design is:

- one long-lived backend-scoped remote owner per local WaitAgent backend
- the owner is tied to backend lifetime, not to one foreground attached client
- the owner keeps remote node and session state only in memory

This owner is the only accepted source of truth for:

- connected remote nodes
- node connection state
- synchronized remote sessions contributed by each node
- remote-session availability and task-state projection
- the session-centric routing table used by remote interaction runtimes

### 4.2 Reuse The Existing Node Owner Boundary

WaitAgent already has a `RemoteNodeSessionOwnerRuntime` boundary for live node
ownership, reconnect, and disconnect handling.

The accepted direction is to promote that boundary, or a thin backend-scoped
wrapper around it, into the durable owner for both:

- live node connection ownership
- remote session catalog ownership

The project must not introduce one separate cache owner for sidebar rows and a
different runtime owner for live node connections.

There must be one live remote owner boundary per backend.

### 4.3 Owner Process Model

The accepted process model is:

- the remote owner runs as a backend-scoped local sidecar
- startup ensures that sidecar exists for the current backend
- attached workspace UI processes and helper panes are clients of that owner

This is required because a normal attached CLI process is allowed to exit while
the backend stays alive.

## 5. Lifecycle Semantics

### 5.1 Startup

On backend startup or first remote-capable entry:

1. ensure the backend-scoped remote owner sidecar is running
2. bind its local IPC endpoint for the backend
3. initialize with an empty in-memory remote catalog
4. accept future node connect, disconnect, and session-sync events

No historical remote sessions are loaded from disk.

### 5.2 Connect

`waitagent --connect <host:port>` must register the outbound node session with
the backend-scoped remote owner.

After the connect handshake succeeds:

- the owner records the live node
- the node's current default session is published into owner memory
- later session create, update, exit, and offline transitions mutate the same
  in-memory owner state

### 5.3 Detach

On `detach`:

- the current UI attachment disappears
- the backend-scoped remote owner remains alive
- live remote node sessions remain connected if their transport is still alive
- remote sidebar state is not recomputed from files because no files are the
  accepted source of truth

### 5.4 Reattach

On later `attach` or workspace re-entry:

1. the UI reconnects to the backend-scoped remote owner
2. it fetches a fresh runtime snapshot
3. sidebar and related activation surfaces render from that snapshot

Reattach must never depend on stale persisted remote rows.

### 5.5 Backend Shutdown

When the owning backend dies:

- the backend-scoped remote owner exits
- its in-memory remote catalog disappears
- all remote rows vanish on the next UI refresh

That is the accepted behavior because remote sidebar state is runtime-only.

## 6. Local IPC Boundary

Higher-level consumers must not read remote catalog files directly.

The accepted local IPC boundary is one backend-scoped control endpoint such as
a Unix-domain socket owned by the remote owner sidecar.

The minimum accepted API is:

- `snapshot`
  Return the current node and session catalog for the backend.
- `subscribe` or equivalent refresh signal
  Let UI or runtime consumers learn that remote state changed without polling
  random files.
- `register_node_session`
  Hand a newly established outbound or inbound node session to the owner.
- `node_disconnected`
  Mark a node offline or remove it according to accepted lifecycle semantics.

The exact wire format is an implementation detail.

The architectural rule is the important part:

- remote catalog consumers talk to the owner runtime
- remote catalog producers publish into the owner runtime
- nobody else owns or persists the sidebar-visible remote session snapshot

## 7. Catalog Integration

The accepted catalog composition becomes:

- local tmux sessions from the local tmux gateway
- remote sessions from the backend-scoped remote owner snapshot

Rejected composition:

- local tmux sessions
- plus file-backed published remote targets
- plus file-backed discovered remote sessions

`TargetRegistryService` may still merge multiple producers, but for remote
sidebar state the accepted producer is the live owner snapshot only.

Transitional rule:

- if legacy file-backed publication helpers still exist temporarily for
  migration reasons, they must stop feeding the user-visible remote session
  catalog
- any remaining file-backed publication path may survive only as a private
  compatibility bridge until the owning implementation slice deletes it

## 8. Detach, Reattach, And Multi-Client Behavior

The accepted ownership shape is:

- one backend
- one remote owner sidecar
- zero or more attached UI clients
- zero or more live remote node connections
- one node contributing zero or more remote sessions

This means:

- many local attaches to the same backend see the same remote session snapshot
- one `waitagent --connect` may expose many remote sessions under that node
- one connected node should appear as multiple rows such as
  `codex@10.1.29.165:pty1`
- server-to-client and client-to-server messages continue routing by
  `session_id`, not by one-off attachment handles

## 9. Failure Semantics

### 9.1 Owner Missing At Read Time

If a UI consumer cannot reach the backend-scoped remote owner:

- treat the remote catalog as unavailable or empty
- do not recover by reading stale files
- surface connection absence through runtime status cues only if needed

### 9.2 Owner Crash

If the remote owner crashes while the backend still lives:

- the next backend entrypoint must ensure it is restarted
- remote catalog starts empty until live node sessions re-register
- reconnect logic remains node-scoped and owned by the accepted node owner
  runtime

### 9.3 Node Disconnect

If a connected node disconnects:

- the owner updates that node and its sessions from live transport state
- offline projection or removal follows the accepted remote lifecycle policy
- stale rows must not survive only because an old cache file still exists

## 10. Migration Plan

The migration away from the current design is:

1. introduce the backend-scoped remote owner sidecar and local IPC
2. move live remote session ownership into that owner
3. switch sidebar and target-catalog remote reads to the owner snapshot
4. remove `DiscoveredRemoteSessionStore` from the sidebar-visible catalog path
5. remove any remaining file-backed remote sidebar source, including legacy
   published-target merge paths that are no longer product-correct
6. validate `detach -> reattach -> connect -> disconnect` against the accepted
   runtime-only model

## 11. Task Split

The remaining `task.t5-08c4` queue is split into:

1. `task.t5-08c4a`
   Lock this owner-runtime design and realign the execution queue.
2. `task.t5-08c4b`
   Introduce the backend-scoped remote owner sidecar and its local IPC
   snapshot or registration boundary.
3. `task.t5-08c4c`
   Switch shared-catalog and sidebar remote state to the live owner snapshot
   and retire file-backed remote sidebar sources.
4. `task.t5-08c4d`
   Close detach or reattach continuity, owner restart semantics, and explicit
   cross-host manual validation on the accepted runtime-only path.

## 12. Anti-Goals

- do not persist remote sidebar rows in `/tmp`
- do not let `attach` or sidebar refresh silently fall back to stale files
- do not create one owner for remote transport and another owner for remote
  sidebar cache
- do not treat `attachment_id` as the durable identity of a remote session
- do not keep obsolete publication-centric remote rows as a user-visible source
  of truth once the live owner snapshot exists
