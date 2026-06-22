# Reliable Remote Publication Design

Version: `v0.1`
Status: `Draft`
Date: `2026-06-22`

## 1. Purpose

This document freezes the replacement design for remote session runtime
publication.

The current implementation relies on one-off snapshots, fire-and-forget
`TargetPublished` messages, and UI refresh side effects. That model can leave
remote sidebar items stale after runtime changes such as `bash -> codex`, and
it can leave remote sessions missing after a network disconnect until the
server is restarted or the workspace is reattached.

The accepted direction is an event-driven publication protocol:

```text
source semantic event
  -> revisioned publish
  -> receiver apply
  -> receiver ack
  -> source clears pending publication
```

The design intentionally does not add a fixed polling interval as the
synchronization mechanism.

## 2. Scope

This design covers:

- remote target runtime-state publication
- target exit publication
- receiver-side idempotent apply
- publish acknowledgements
- source-side retry with exponential backoff
- disconnect and reconnect recovery
- sidebar visibility semantics for disconnected remote targets

It does not cover:

- TLS or node authentication policy
- PTY byte ordering
- console input ordering
- terminal mirror bootstrap replay
- long-term persistent storage of source publication state

## 3. Non-Negotiable Rules

1. State publication is event-driven.
   A source node publishes only when a semantic target state changes.
2. Source publication state is in memory only.
   The source node does not persist pending publications to disk.
3. Every publication is revisioned.
   Revisions are monotonic only within one source node instance.
4. Every applied publication is acknowledged.
   The source retries unacknowledged publications with exponential backoff.
5. Disconnect does not mean target exit.
   The receiver must mark remote targets disconnected or stale rather than
   removing them immediately.
6. Only explicit target exit removes a target.
   `TargetExited` is the removal signal.
7. UI refresh is downstream of applied state.
   A sidebar refresh is triggered after receiver state is applied, not used as
   the authority for remote runtime state.
8. PaneActivityWatcher is not part of the new publication protocol.
   It may be deleted or bypassed when the semantic event path replaces it.

## 4. Source Identity And Revisions

Each source daemon process creates a fresh `node_instance_id` at startup.

The receiver compares publications by:

```text
(node_id, node_instance_id, target_id, revision)
```

Revision rules:

- revision starts at `1` for each target in a new `node_instance_id`
- revision increments only when the target semantic state changes
- retries resend the same revision
- retries must not allocate a new revision
- a daemon restart creates a new `node_instance_id`
- a new `node_instance_id` is treated as a new source epoch

This avoids requiring durable source state. If the daemon restarts, it reads the
current live targets and publishes a new baseline under the new source epoch.

## 5. Published Runtime State

The source publishes a target state record containing:

- `target_id`
- `authority_node_id`
- `node_instance_id`
- `revision`
- `transport`
- `transport_session_id`
- `selector`
- `availability`
- `command_name`
- `current_path`
- `attached_count`
- `session_role`
- `workspace_key`
- `window_count`
- `task_state`

The source publishes when any semantic field changes.

Examples:

- foreground command changes from `bash` to `codex`
- foreground command changes from `codex` back to `bash`
- agent state changes from `Input` to `Running`
- agent state changes from `Running` to `Confirm`
- current directory changes
- target becomes disconnected
- target reconnects and becomes online
- target exits

## 6. Protocol Changes

Extend `TargetPublished`:

```proto
message TargetPublished {
  string target_id = 1;
  string authority_node_id = 2;
  string transport = 3;
  string transport_session_id = 4;
  optional string selector = 5;
  string availability = 6;
  optional string command_name = 7;
  optional string current_path = 8;
  optional uint64 attached_count = 9;
  optional string session_role = 10;
  optional string workspace_key = 11;
  optional uint64 window_count = 12;
  optional string task_state = 13;
  string node_instance_id = 14;
  uint64 revision = 15;
}
```

Extend `TargetExited`:

```proto
message TargetExited {
  string target_id = 1;
  string transport_session_id = 2;
  string node_instance_id = 3;
  uint64 revision = 4;
}
```

Add publication acknowledgement:

```proto
enum TargetPublicationAckStatus {
  TARGET_PUBLICATION_ACK_STATUS_UNSPECIFIED = 0;
  TARGET_PUBLICATION_ACK_STATUS_APPLIED = 1;
  TARGET_PUBLICATION_ACK_STATUS_STALE_REVISION = 2;
  TARGET_PUBLICATION_ACK_STATUS_FAILED = 3;
}

message TargetPublicationAck {
  string node_id = 1;
  string node_instance_id = 2;
  string target_id = 3;
  uint64 revision = 4;
  TargetPublicationAckStatus status = 5;
  optional string message = 6;
}
```

Add `TargetPublicationAck` to `NodeSessionEnvelope.body`.

## 7. Source Publication Tracker

The source owns an in-memory publication tracker. It is not a durable store.

For each target, it tracks:

- `last_state`
- `latest_revision`
- `pending_revision`
- `pending_payload`
- `acked_revision`
- `next_retry_at`
- `retry_attempt`

### 7.1 State Change

When a semantic state event arrives:

1. Build the new target state.
2. Compare it with `last_state`.
3. If unchanged, do nothing.
4. If changed, increment `latest_revision`.
5. Create a pending publication for that revision.
6. Send immediately if the node session is connected.

### 7.2 Ack

When an ack arrives:

- `applied`: clear matching pending publication
- `stale_revision`: clear matching pending publication
- `failed`: keep pending publication and retry

Acks for older revisions must not clear a newer pending revision.

### 7.3 Disconnect

On transport disconnect:

- keep pending publications
- pause send attempts
- keep the latest in-memory target state

### 7.4 Reconnect

On transport reconnect:

- replay all pending publications
- rebuild current live state
- emit new revisions for live-state differences
- publish a baseline for targets that are live but not known by the tracker

## 8. Retry Policy

Retry applies only to unacknowledged pending publications.

Retries use exponential backoff:

```text
attempt 1: 250ms
attempt 2: 500ms
attempt 3: 1s
attempt 4: 2s
attempt 5: 4s
attempt 6: 8s
attempt N: 10s max
```

Rules:

- retry the same `(target_id, revision)`
- do not allocate a new revision for retries
- cap delay at 10 seconds
- optionally add 0-20% jitter to avoid synchronized retries
- pause retries while disconnected

## 9. Receiver Apply Rules

The receiver stores the latest applied revision for:

```text
(node_id, node_instance_id, target_id)
```

On `TargetPublished`:

1. Validate the payload.
2. Compare revision with latest applied revision.
3. If the revision is stale, do not apply it.
4. Return `stale_revision` ack.
5. If the revision is new, upsert runtime owner state.
6. Mark the target online.
7. Trigger workspace/sidebar refresh for affected workspaces.
8. Return `applied` ack.

On `TargetExited`:

1. Compare revision with latest applied revision.
2. If stale, do not remove the target.
3. Return `stale_revision` ack.
4. If new, remove the target.
5. Trigger workspace/sidebar refresh.
6. Return `applied` ack.

If apply fails, return `failed` and include a diagnostic message.

## 10. Disconnect Semantics

Transport failure is not target exit.

On node disconnect, the receiver must:

- keep previously known remote targets
- mark them disconnected or stale
- refresh visible workspaces
- keep sidebar items visible
- avoid sending `TargetExited` on behalf of the source

On reconnect, the source replays pending publications and publishes the current
live baseline. The receiver marks targets online as new applied publications
arrive.

Only an explicit `TargetExited` removes a target from the sidebar.

## 11. Event Sources

The publication protocol must be driven by semantic events, not fixed polling.

Target/session lifecycle events:

- target created
- target exited
- attachment count changed
- window count changed
- current path changed

Command lifecycle events:

- foreground command changed
- foreground command exited

Agent runtime events:

- agent plugin reports `Input`
- agent plugin reports `Running`
- agent plugin reports `Confirm`
- agent plugin reports `Unknown`

Codex, Claude, Kimi, and future plugins should all emit the same normalized
agent runtime state.

## 12. Migration Notes

`PaneActivityWatcher` currently polls tmux pane metadata and signals chrome
refresh. It should not be used as the new state publication mechanism.

During migration:

- keep UI refresh working only where needed
- introduce semantic event publishers beside the existing code
- move remote publication onto the reliable path
- delete or bypass `PaneActivityWatcher` once it no longer owns useful UI
  behavior

## 13. Task Breakdown

### Task 1: Protocol Extension

- Add `node_instance_id` and `revision` to `TargetPublished`.
- Add `node_instance_id` and `revision` to `TargetExited`.
- Add `TargetPublicationAckStatus`.
- Add `TargetPublicationAck`.
- Add ack to `NodeSessionEnvelope.body`.
- Regenerate protobuf bindings.
- Update transport codec mappings and tests.

### Task 2: Receiver Revision Table

- Track latest applied revision by `(node_id, node_instance_id, target_id)`.
- Reject stale revisions without applying state.
- Ack stale revisions as `stale_revision`.
- Ack applied revisions as `applied`.
- Preserve existing compatibility until all senders include revisions.

### Task 3: SourcePublicationTracker

- Add an in-memory source publication tracker.
- Track last state, latest revision, pending payload, acked revision, retry
  attempt, and next retry time.
- Expose `on_state_changed`, `on_ack`, `on_disconnected`, and
  `on_reconnected`.
- Do not persist tracker state to disk.

### Task 4: Reliable Publish Sender

- Send pending publications immediately when connected.
- Retry unacknowledged publications with exponential backoff.
- Pause retries while disconnected.
- Replay pending publications on reconnect.
- Ensure retries reuse the same revision.

### Task 5: Disconnect State

- Replace disconnect-time remote node removal with disconnected/stale marking.
- Keep sidebar items visible after transport failure.
- Trigger workspace refresh after disconnected marking.
- Remove targets only after explicit `TargetExited`.

### Task 6: Reconnect Replay

- On `SessionOpened`, replay pending publications.
- Rebuild live target state.
- Publish baseline revisions for unknown live targets.
- Mark targets online when receiver applies new publications.

### Task 7: Semantic Event Sources

- Add target lifecycle event emission.
- Add command lifecycle event emission.
- Add normalized agent runtime event emission.
- Route Codex, Claude, Kimi, and future plugins through the normalized state
  interface.

### Task 8: PaneActivityWatcher Removal

- Remove state-protocol reliance on `PaneActivityWatcher`.
- Delete it when semantic events fully cover UI refresh needs.
- Keep only explicit UI refresh signals that are downstream of applied state.

### Task 9: Sidebar/UI Integration

- Display disconnected/stale remote targets instead of dropping them.
- Refresh sidebar after receiver apply and disconnect marking.
- Keep command labels sourced from revision-backed runtime state.

### Task 10: Tests

- Unit test revision apply and stale rejection.
- Unit test ack clears pending publication.
- Unit test failed ack keeps pending publication.
- Unit test exponential backoff schedule.
- Unit test retry reuses the same revision.
- Integration test `bash -> codex` updates sidebar label.
- Integration test network disconnect keeps item visible as disconnected.
- Integration test reconnect replays state and restores online item.
- Integration test old revision cannot overwrite newer state.
