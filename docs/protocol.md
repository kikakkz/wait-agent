# WaitAgent Protocol

Version: `v1.1`  
Status: `Accepted`  
Date: `2026-04-28`

## 1. Purpose

This document defines the accepted client and server signaling contract for
WaitAgent network mode.

It exists to make `task.t5-07` and later remote-console work implementable
without another protocol redesign.

It complements:

- [architecture.md](architecture.md)
- [remote-session-foundation.md](remote-session-foundation.md)
- [functional-design.md](functional-design.md)

## 2. Scope

This protocol covers only `server <-> client node` communication.

It does not change the local-only product path:

- local mode must still work without a live server
- local tmux remains a valid target producer without any network transport
- server-hosted consoles use the same control-plane state machine, but their
  events are in-process server events rather than wire messages

This document intentionally defines:

- the canonical wire envelope
- canonical target and console identity fields
- the message set required for target catalog sync, remote open, input, output,
  and PTY resize
- ordering and authority rules
- reconnect and failure behavior

This document intentionally does not define:

- transport-specific authentication mechanisms
- a binary snapshot format
- rich remote task-state probing heuristics

## 3. Non-Negotiable Rules

The remote-session foundation remains authoritative:

1. The PTY owner stays remote.
   The server never pretends to own a remote PTY locally.
2. Local and remote targets share one target model.
   The protocol must not fork a remote-only identity shape.
3. Input is shared.
   Multiple consoles may send input to one target.
4. Output is broadcast.
   PTY output fans out to every opened console.
5. Resize scope is explicit.
   Attachment viewport resize is local to each console. PTY resize, when
   routed through the control plane, is exclusive.
6. The server control plane serializes cross-console input order.
7. The PTY owner serializes PTY output order.

## 4. Wire Format

Each protocol message is one UTF-8 JSON object.

Framing rule:

- on WebSocket, one WebSocket message carries exactly one protocol envelope
- on framed TCP, one length-prefixed frame carries exactly one protocol envelope

Binary PTY data is base64-encoded inside JSON payloads.

This keeps the logical protocol transport-agnostic while still defining one
concrete message representation.

## 5. Actors

- `client node`
  A WaitAgent node that owns local PTYs and may also host a local workspace
  console.
- `server`
  The aggregate control plane that maintains replicated target state, tracks
  console attachments, serializes multi-console input, and arbitrates PTY
  resize when that control path is active for a target.
- `console host`
  A runtime that hosts one or more consoles.
  This can be a client node or the server process itself.

Important boundary:

- only client nodes speak this wire protocol
- server-hosted consoles participate in the same attachment and authority
  model, but not over the wire

## 6. Canonical Identities

### 6.1 Node Identity

Every connected client node has a stable `node_id`.

Rules:

- `node_id` is stable across reconnect when the local installation persists
- `node_id` is unique within one server deployment
- the server keys connection recovery on `node_id`

### 6.2 Target Identity

Every target published to the server carries:

- `target_id`
  Stable WaitAgent target identity.
  This is the primary protocol identifier for one target.
- `authority_id`
  The current PTY-owning node id.
- `transport`
  One of `local-tmux` or `remote-peer`.
- `transport_session_id`
  Transport-local session identity on the PTY owner.
- `selector`
  Optional compatibility or authority-local selector that can resolve the
  concrete transport target on the PTY-owning node without replacing
  `target_id`.

Rules:

- `target_id` is stable across reconnect if the target still exists
- `target_id` is not derived by the server
- `selector` is compatibility metadata, not the canonical identity key
- when present, `selector` may also be used by authority-local runtimes to
  resolve a concrete PTY host such as a tmux `socket:session` target for
  loopback or same-node bridging

### 6.3 Console Identity

Every console attachment is described by:

- `console_id`
  Stable identity for one console surface on one console host
- `console_location`
  `local-workspace` or `server-console`
- `console_host_id`
  The node id of the client host, or the literal `server` for server-hosted
  consoles

The server creates:

- `attachment_id`
  Stable identity for one `open target in console` relationship

## 7. Envelope Contract

Every message must use this envelope:

```json
{
  "protocol_version": "1.1",
  "message_id": "msg-uuid",
  "message_type": "client_hello",
  "timestamp": "2026-04-27T12:00:00Z",
  "sender_id": "node-a",
  "payload": {}
}
```

Required top-level fields:

- `protocol_version`
- `message_id`
- `message_type`
- `timestamp`
- `sender_id`
- `payload`

Optional top-level fields:

- `correlation_id`
- `target_id`
- `attachment_id`
- `console_id`

Rules:

- `message_id` is unique per sender
- `correlation_id` links replies or errors to the initiating message
- `target_id`, `attachment_id`, and `console_id` are duplicated at envelope
  level only when they improve routing or diagnostics

## 8. Capability Contract

The first accepted protocol version is `1.1`.

Capability negotiation is explicit but minimal.

Each side may advertise:

- `max_frame_bytes`
- `supports_screen_snapshot`
- `supports_server_console_observation`
- `supports_server_console_input`

The first accepted baseline assumes:

- snapshot delivery is optional
- server-console observation is supported later, but its state model is already
  reserved
- client nodes must tolerate receiving catalog messages for targets they do not
  currently open

## 9. Message Set

## 9.1 Connection Messages

### `client_hello`

Direction:

- client node -> server

Payload:

- `node_id`
- `client_version`
- `capabilities`
- `auth_material`

Meaning:

- begin session negotiation
- resume node identity if reconnecting

### `server_hello`

Direction:

- server -> client node

Payload:

- `server_id`
- `server_version`
- `accepted_protocol_version`
- `capabilities`
- `heartbeat_interval_ms`
- `session_recovery_policy`

Meaning:

- accept or reject the node connection
- establish protocol version and heartbeat cadence

### `heartbeat`

Direction:

- client node -> server
- server -> client node

Payload:

- `sender_runtime`
- `active_target_count`
- `open_attachment_count`
- `last_catalog_version`

Meaning:

- liveness
- coarse recovery diagnostics

## 9.2 Target Catalog Messages

### `target_published`

Direction:

- authority client node -> server

Payload:

- `transport_session_id`
- `selector`
- `availability`
  One of `online`, `offline`, `exited`
- `session_role`
- `workspace_key`
- `command_name`
- `current_path`
- `attached_clients`
- `window_count`

Envelope rules:

- `sender_id` is the publishing `authority_id`
- `target_id` is the canonical remote target id such as
  `remote-peer:<authority_id>:<transport_session_id>`

Rules:

- the authority client is the only writer of authoritative target metadata
- the server derives `opened_by` and resize ownership from attachment messages,
  not from `target_published`
- repeated `target_published` messages replace the replicated target metadata for
  that `target_id`
- the current local development path may deliver these messages through a
  socket-backed publication listener that is fed by a socket-scoped publication
  agent; current tmux hooks only signal agent reconciliation before real node
  or server transport lands

### `target_exited`

Direction:

- authority client node -> server

Payload:

- `transport_session_id`

Envelope rules:

- `sender_id` is the publishing `authority_id`
- `target_id` remains the canonical remote target id

Rules:

- after `target_exited`, the server removes or marks the replicated target as
  exited according to the server lifecycle policy
- existing attachments may remain briefly for UI cleanup, but no new input or
  resize is valid for that target
- the current local development path may emit `target_exited` from the same
  publication-agent reconcile pass when a previously published target drops out
  of the authority socket view

### `node_offline`

Direction:

- server -> interested client nodes

Payload:

- `node_id`
- `offline_at`

Meaning:

- the PTY owner or observing node is currently disconnected
- affected remote targets should project `offline` until reconciled

## 9.3 Console Attachment Messages

### `open_target`

Direction:

- observing client node -> server

Payload:

- `console_id`
- `console_location`
- `target_id`
- `cols`
- `rows`

Rules:

- `open_target` means “open this target in this console”
- `cols` and `rows` describe the opening console's local viewport size, not an
  immediate PTY resize
- a repeated `open_target` for the same console refreshes the attachment and
  updates the console's last-known viewport size
- the newest successful `open_target` becomes PTY resize authority by default
  only for the PTY-resize control path

### `open_target_ok`

Direction:

- server -> observing client node

Payload:

- `target_id`
- `attachment_id`
- `console_id`
- `resize_epoch`
- `resize_authority_console_id`
- `resize_authority_host_id`
- `availability`
- `initial_snapshot`
  Optional inline snapshot or reference

Rules:

- the server creates `attachment_id`
- the `resize_*` fields describe PTY-resize authority state, not ownership of
  the attachment's local viewport
- if `open_target` changed PTY resize authority, the server increments
  `resize_epoch`

### `open_target_rejected`

Direction:

- server -> observing client node

Payload:

- `target_id`
- `console_id`
- `code`
- `message`

Typical reasons:

- `unknown_target`
- `target_offline`
- `unauthorized`

### `close_target`

Direction:

- observing client node -> server

Payload:

- `attachment_id`
- `target_id`

Rules:

- `close_target` removes one console attachment
- if the removed attachment held PTY resize authority, the server reassigns
  authority to the most recently opened remaining attachment
- when authority changes, the server increments `resize_epoch`

### `resize_authority_changed`

Direction:

- server -> affected client nodes

Payload:

- `target_id`
- `resize_epoch`
- `resize_authority_console_id`
- `resize_authority_host_id`
- `cols`
  Optional last-known PTY column count
- `rows`
  Optional last-known PTY row count

Rules:

- this message announces PTY-resize authority, not attachment viewport
  ownership
- `cols` and `rows` are the server's last-known PTY size for the new authority
  when one is known, and may be absent before any explicit PTY resize has been
  accepted
- after authority changes, the server may send `apply_resize` to the PTY owner
  using the same `resize_epoch` when the runtime is propagating PTY resize

Local viewport rule:

- pane resize, fullscreen, and other viewer-local geometry changes are not
  `resize_request` messages in protocol v1.1
- local viewport changes must be handled by the console runtime and must not be
  rejected by PTY-resize authority rules
- if wider history or richer redraw is needed after viewport growth, the
  runtime should rely on local terminal buffering or later `screen_snapshot`
  support rather than forcing a PTY resize

## 9.4 Input Messages

### `console_input`

Direction:

- observing client node -> server

Payload:

- `attachment_id`
- `target_id`
- `console_id`
- `console_seq`
- `bytes_base64`

Rules:

- `console_seq` is monotonic per console
- `console_input` is valid only for an open attachment
- the server does not forward raw local console order directly to the PTY owner

### `target_input`

Direction:

- server -> authority client node

Payload:

- `attachment_id`
- `target_id`
- `console_id`
- `console_host_id`
- `input_seq`
- `bytes_base64`

Rules:

- `input_seq` is assigned by the server and is monotonic per target
- the authority client applies input strictly in `input_seq` order
- multi-console input ordering is therefore serialized by the server control
  plane rather than inferred at the PTY host

## 9.5 Output Messages

### `target_output`

Direction:

- authority client node -> server
- server -> observing client nodes

Payload:

- `target_id`
- `output_seq`
- `stream`
  The first accepted value is `pty`
- `bytes_base64`

Rules:

- `output_seq` is assigned by the PTY owner and is monotonic per target
- the server forwards output in the same order and must not renumber it
- observers render output in `output_seq` order
- different observers may render the same ordered output into different local
  viewport sizes

### `screen_snapshot`

Direction:

- authority client node -> server
- server -> observing client nodes

Payload:

- `target_id`
- `screen_version`
- `snapshot_encoding`
- `snapshot_data`

Rules:

- `screen_snapshot` is optional in protocol v1.1
- it exists to support later focus restore, late join, replay, and fuller
  redraw after local viewport expansion

## 9.6 PTY Resize Messages

### `resize_request`

Direction:

- observing client node -> server

Payload:

- `attachment_id`
- `target_id`
- `console_id`
- `cols`
- `rows`

Rules:

- `resize_request` is only for PTY resize, not attachment viewport resize
- only the current PTY-resize authority may send an accepted `resize_request`
- non-authority PTY-resize requests are rejected with
  `error.code = resize_denied`
- the server stores the authority console's latest size on every accepted
  PTY-resize request
- a local pane resize or fullscreen transition must not generate
  `resize_request` by itself

### `apply_resize`

Direction:

- server -> authority client node

Payload:

- `target_id`
- `resize_epoch`
- `resize_authority_console_id`
- `cols`
- `rows`

Rules:

- the PTY owner must ignore stale `apply_resize` messages whose `resize_epoch`
  is older than the latest one it has applied for that target
- authority changes and accepted PTY-resize requests both fan out through
  `apply_resize`

### `resize_applied`

Direction:

- authority client node -> server

Payload:

- `target_id`
- `resize_epoch`
- `cols`
- `rows`
- `applied_at`

Rules:

- the server accepts `resize_applied` only for the current `resize_epoch`
- the server may reflect the result to observing clients by updating target
  state or by resending `resize_authority_changed`

## 9.7 Error Message

### `error`

Direction:

- either direction

Payload:

- `code`
- `message`
- `details`

The first required error codes are:

- `unauthorized`
- `unsupported_protocol_version`
- `unknown_target`
- `node_offline`
- `attachment_not_open`
- `write_failed`
- `resize_denied`
- `stale_resize_epoch`

`resize_denied` applies only to PTY-resize requests. It must not be used for
purely local viewport changes.

## 10. Ordering Rules

The protocol must preserve:

- target metadata replacement order per `target_id`
- input order per `target_id` as assigned by `input_seq`
- output order per `target_id` as assigned by `output_seq`
- PTY resize authority changes per `resize_epoch`

The protocol does not require one global total order across all targets.

## 11. Open, Input, Output, and Resize Flow

The accepted remote open flow is:

1. The observing client sends `open_target`.
2. The server creates or refreshes the attachment.
3. The server records the attachment viewport and assigns PTY resize authority
   to the newest opener for the PTY-resize control path.
4. The server replies `open_target_ok`.
5. If PTY resize authority changed, the server emits
   `resize_authority_changed`.
6. If the runtime is propagating PTY size, the server emits `apply_resize`.
7. If `apply_resize` was sent, the authority client replies `resize_applied`.

The accepted remote input flow is:

1. The console host sends `console_input`.
2. The server assigns `input_seq`.
3. The server forwards `target_input` to the PTY owner.
4. The PTY owner writes bytes to the PTY in `input_seq` order.

The accepted remote output flow is:

1. The PTY owner reads PTY bytes.
2. The PTY owner emits `target_output` with `output_seq`.
3. The server forwards the same ordered stream to every observing client node.

The accepted remote PTY-resize flow is:

1. The current PTY-resize authority console sends `resize_request`.
2. The server validates authority and records the latest PTY size.
3. The server emits `apply_resize` with the current `resize_epoch`.
4. The PTY owner applies the resize and replies `resize_applied`.

## 12. Reconnect and Recovery

Reconnect rules:

- a reconnecting client node must send the same `node_id`
- after `server_hello`, the client republishes every live target with
  `target_upsert`
- the server reconciles by `target_id`

Recovery rules:

- if a previously published target still exists on the PTY owner, it keeps the
  same `target_id`
- existing open attachments remain server-owned state
- if the authority node disconnects, the server marks affected targets
  `offline` and rejects new `console_input` or PTY `resize_request` traffic
  until the authority node reconnects or the target exits

Duplicate-prevention rule:

- the server must treat `target_id` as the canonical deduplication key
- `selector` may collide or change and must not be used as the primary key

## 13. Implementation Notes

Protocol v1.1 intentionally maps cleanly onto the current target catalog work:

- `target_id` aligns with the stable WaitAgent target identity
- `authority_id` aligns with PTY ownership
- `transport` and `transport_session_id` align with transport-aware target
  addressing
- `availability`, `opened_by`, attachment viewport state, and PTY resize
  authority can map into one shared transport-agnostic target record

The first implementation slices should follow this order:

1. use the accepted shared target model and registry boundary
2. add the wire message types above
3. route remote open, input, and resize through the server control plane
4. add server-hosted consoles on top of the same attachment and authority state

Anti-goals:

- do not reintroduce attach-based switching semantics for remote targets
- do not bypass the server for remote input ordering
- do not make `selector` the canonical remote identity
- do not invent a fake local PTY on the server for remote rendering

This lets network mode grow later without forcing the local MVP to simulate a full distributed system.

## 19. MVP Protocol Subset

The first network-capable subset should support only:

- Client hello / server hello
- Heartbeat
- Session started
- Session updated
- Session exited
- Stdout chunk
- Stdin chunk
- Resize applied

Everything else can layer on later.

## 20. Executable Schema Baseline

The repository now includes an executable baseline for protocol schema and versioning in:

- `src/transport.rs`

This baseline currently defines:

- `ProtocolVersion`
- `ConnectionId`
- `MessageId`
- `TransportEnvelope`
- `TransportPayload`
- Explicit payload structs for the MVP protocol subset
- Validation rules for protocol version, session identity, and console identity

Current implementation rule:

- The code-level schema is the source of truth for transport-facing types during implementation
- This document remains the source of truth for behavior and message semantics

## 20.1 Client Runtime Skeleton Baseline

The earlier experimental client runtime skeleton was removed during the local-architecture reset.

The repository does not currently ship a live network client runtime.

Current implementation rule:

- Future remote-connect behavior must be reintroduced only on top of the cleaned tmux-native local architecture
- No deleted `client/server/transport` surface should be treated as the source of truth for resumed remote work

## 20.2 Registration And Liveness Baseline

The earlier executable registration and liveness baseline was also removed with the legacy network runtime.

Current implementation rule:

- When remote work resumes, registration and liveness must flow through one explicit transport model
- The next design must start from the current local session catalog and chrome/runtime ownership model rather than from deleted bridge code

## 20.3 Remote Session Publication Baseline

There is currently no live remote session publication implementation in the repository.

Current implementation rule:

- Future session publication must reuse the same transport envelope model as registration and liveness
- Remote session publication should be designed only after the unified local/remote target model is agreed

## 21. Versioning Rules

The current versioning baseline is:

- `CURRENT_PROTOCOL_VERSION = 1`
- `MIN_SUPPORTED_PROTOCOL_VERSION = 1`

Rules:

- Every transport envelope must carry an explicit protocol version
- Unsupported versions must be rejected before payload handling
- Version negotiation may widen later, but the first implementation uses one accepted version only
- Backward-compatible additions should prefer extending payload structs over reinterpreting existing fields

## 22. Internal Event Bus Baseline

The repository now includes an internal event bus baseline in:

- `src/event.rs`

This baseline defines:

- `EventGroup`
- `EventBusMessage`
- `EventEnvelope`
- `EventBus`

Current implementation rule:

- Local and network runtimes should share the same event-envelope model
- Transport integration should publish into the internal event bus rather than bypassing runtime boundaries with ad hoc direct calls
