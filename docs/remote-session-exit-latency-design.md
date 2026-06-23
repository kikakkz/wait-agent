# WaitAgent Remote Session Exit Latency Design

Version: `v1.0`
Status: `Accepted - implementation split ready`
Date: `2026-06-20`

## 1. Purpose

This document fixes the design for reducing the visible latency after a remote
session exits.

The user-visible symptom is:

- start a local WaitAgent
- connect to a local remote host
- create multiple remote sessions with `Ctrl-S`
- type `exit` in a remote session
- the remote sidebar item disappears noticeably later than expected

Diagnostics show that the delay is not primarily shell exit, transport input,
or sidebar render. The two confirmed local contributors are:

1. remote-session synchronization waits for a periodic catalog diff with a
   worst-case `500ms` delay
2. publication apply calls `live_workspace_socket_names()` on `TargetExited`,
   which scans all WaitAgent tmux sockets and costs about `565-613ms` on a
   machine with many stale sockets

This design removes those two hot-path costs without bypassing the existing
runtime ownership model.

## 2. Non-Negotiable Rules

1. Preserve the unified event loop.
   Session state must continue to advance through runtime-owned events, not
   through UI-side shortcuts or direct sidebar mutation.
2. Keep catalog diff as the state authority.
   A local lifecycle wake may trigger synchronization, but it must not itself
   invent `TargetPublished` or `TargetExited` state.
3. Do not introduce durable event persistence for this slice.
   WaitAgent-managed lifecycle reliability should be improved with acknowledged
   local IPC and startup or reconnect reconciliation, not with a new event log.
4. Do not optimize by polling faster.
   Reducing the interval would add tmux and CPU churn while keeping the wrong
   model on the hot path.
5. Do not scan all tmux sockets on every `TargetExited`.
   Full discovery is a recovery or startup tool, not a per-exit refresh step.
6. Do not cover external tmux mutation in this slice.
   Direct user operations outside WaitAgent, such as manual `tmux kill-session`,
   are explicitly out of scope for real-time guarantees here.

## 3. Accepted Runtime Flow

The accepted state flow remains:

```text
WaitAgent-managed target lifecycle
 -> local catalog changed event
 -> remote session sync loop
 -> sync_local_sessions() catalog diff
 -> TargetPublished / TargetExited envelope
 -> publication runtime apply
 -> remote runtime owner store mutation
 -> affected workspace chrome refresh
 -> sidebar item appears or disappears
```

The optimization changes how quickly the sync loop is woken and how the
publication runtime resolves workspaces to refresh. It does not change who owns
the state.

## 4. Event-Driven Session Sync

### 4.1 Current Problem

The current sync owner periodically calls `sync_local_sessions()`:

```text
recv transport event with timeout(next_sync_at)
if now >= next_sync_at:
    sync_local_sessions()
next_sync_at = now + 500ms
```

When a WaitAgent-managed local target exits just after a sync pass, the
corresponding `TargetExited` may wait almost `500ms` before it is sent.

### 4.2 Accepted Design

The sync loop input becomes explicit:

```rust
enum SessionSyncEvent {
    Transport(RemoteNodeTransportEvent),
    LocalCatalogChanged(LocalCatalogChangeReason),
    Stop,
}
```

The loop processes events through one owner:

```text
Transport(SessionOpened):
    mark active session
    sync_local_sessions()

Transport(SessionClosed | TransportFailed):
    clear active session
    remove remote node state

LocalCatalogChanged:
    if active session exists:
        sync_local_sessions()

Stop:
    exit
```

There is no normal periodic poll in this slice. Instead, reconciliation happens
at lifecycle boundaries:

- sync owner startup runs `sync_local_sessions()` once after a transport session
  is available
- every transport `SessionOpened` runs `sync_local_sessions()`
- every reconnect that opens a new session runs `sync_local_sessions()`

The local event is only a wake. `sync_local_sessions()` remains responsible for
diffing the local catalog against `synced_sessions` and emitting
`TargetPublished` or `TargetExited`.

## 5. Acknowledged Local Notify

### 5.1 Command Shape

Add a local command or hidden runtime entrypoint for catalog-change notification:

```text
__remote-session-sync-owner-notify
  --socket-name <workspace socket>
  --reason local-target-exited
```

The exact CLI name may follow existing hidden-command conventions, but the
runtime boundary is required.

### 5.2 Delivery Semantics

The notifier:

1. connects to the existing session sync owner Unix socket
2. sends `LocalCatalogChanged { reason }`
3. waits for an owner acknowledgement that the event was accepted into the sync
   loop
4. returns success only after that acknowledgement

If the owner socket is missing:

```text
ensure session sync owner running
retry notify once
```

If retry still fails:

- log the failure
- do not mutate sidebar or remote owner state directly
- rely on the next startup or reconnect reconcile

This is intentionally not durable persistence. It is an acknowledged process
boundary wake for WaitAgent-managed lifecycle paths.

## 6. Workspace Refresh Resolution

### 6.1 Current Problem

`RemoteTargetPublicationRuntime::apply_discovered_remote_session_envelope()`
currently removes the remote session and then calls:

```text
live_workspace_socket_names()
 -> discover_waitagent_sockets()
 -> recover_network_config_for_socket(each socket)
 -> list_sessions_on_socket(each socket)
 -> live_workspace_socket_names_from_sessions()
```

On a machine with many stale historical WaitAgent sockets, this blocks the
`TargetExited` hot path for about `565-613ms`.

### 6.2 Accepted Design

Introduce a runtime-owned live workspace registry at socket granularity:

```text
network port -> set of active workspace socket names
```

The hot path becomes:

```text
TargetExited
 -> remote owner remove
 -> resolve live workspace sockets from registry/current context
 -> refresh those sockets
```

Full tmux socket discovery is moved out of the per-exit path. It may still be
used for startup recovery or explicit fallback, but not for every
`TargetExited`.

### 6.3 Registration Points

The registry should be updated by WaitAgent-owned runtime lifecycle events:

- workspace chrome or workspace command startup registers the current socket
- workspace shutdown unregisters the current socket when the runtime owns that
  shutdown path
- reconnect or owner startup may rebuild registry state from known active
  workspace ownership

The first implementation is socket-granular. A later implementation may refine
this to target-granular refresh if needed.

## 7. Correctness Boundaries

Guaranteed by this slice:

- WaitAgent-managed local target exit wakes session sync without waiting for a
  fixed poll interval
- duplicate local catalog changed events are safe because catalog diff is
  idempotent
- duplicate `TargetExited` envelopes are safe because remote owner removal must
  be idempotent
- publication refresh no longer scans every WaitAgent tmux socket per exit
- startup and reconnect reconcile current catalog state without durable event
  persistence

Not guaranteed by this slice:

- real-time detection of external tmux operations outside WaitAgent
- recovery of an event lost because the whole process was killed with
  `SIGKILL` before notify could complete
- durable replay across power loss

Those guarantees would require a different slice, such as a tmux hook/control
watcher or a durable outbox. They are intentionally excluded here.

## 8. Observability

Exit-latency timing logs are kept behind an explicit environment gate so normal runs do not continuously write temporary investigation data:

```text
WAITAGENT_EXIT_LATENCY_DIAG=1
```

When that variable is set, the exit-latency chain records:

```text
exit_enter
local_catalog_notify_start
local_catalog_notify_acked
sync_event_received
sync_local_sessions_start
sync_local_sessions_end
target_exited_sent
publication_apply_exit_start
publication_apply_remove
workspace_refresh_resolve
publication_refresh_spawn
sidebar_item_gone
```

The gated logs must distinguish:

- event delivery time
- catalog diff time
- transport send time
- publication owner mutation time
- workspace socket resolution time
- chrome refresh and visible sidebar disappearance time

## 9. Expected Result

Current measured shape:

```text
session sync wait: 0-500ms
publication workspace discovery: ~565-613ms
sidebar render: ~6-20ms
observed E2E sample: ~1.145s
```

Target shape:

```text
acknowledged notify + sync diff: ~5-30ms
TargetExited send/apply: ~5-20ms
workspace refresh resolution: ~0-5ms
chrome refresh/render: ~10-30ms
expected E2E: ~50-150ms
```

If the remote shell or remote host itself takes longer to exit, that time is
outside this local optimization and should remain visible in timing logs.

## 10. Implementation Split

This design is split into four tasks:

1. `task.remote-exit-latency-1`
   Add acknowledged session sync local-catalog notify and route it into the
   sync owner event loop.
2. `task.remote-exit-latency-2`
   Wire WaitAgent-managed target lifecycle paths to the notify command and
   remove normal-path dependency on the `500ms` sync poll.
3. `task.remote-exit-latency-3`
   Add live workspace socket registry and stop scanning all WaitAgent tmux
   sockets on each `TargetExited`.
4. `task.remote-exit-latency-4`
   Validate E2E latency, compare timing logs, and remove temporary diagnostics
   that are no longer needed.
