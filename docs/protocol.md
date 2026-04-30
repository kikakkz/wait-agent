# WaitAgent Protocol

Version: `v2.0`
Status: `Accepted for task.t5-08a1`
Date: `2026-04-30`

## 1. Purpose

This document defines the accepted node-to-node application protocol for
WaitAgent network mode.

It exists to freeze the production transport contract before real cross-host
ingress implementation starts.

It complements:

- [architecture.md](architecture.md)
- [remote-session-foundation.md](remote-session-foundation.md)
- [remote-node-connection-architecture.md](remote-node-connection-architecture.md)
- [functional-design.md](functional-design.md)

## 2. Scope

This protocol covers only `server <-> client node` communication for remote
target publication, open, input, output, and PTY-control traffic.

It does not change the accepted local product path:

- local mode must still work without a live server
- local tmux remains a valid target producer without network transport
- server-hosted consoles keep using the same control-plane model, but their
  events are in-process rather than wire messages

This document intentionally defines:

- the accepted protobuf package and file ownership
- the accepted gRPC service and RPC shape
- the node-session envelope and message families
- the distinction between terminal interaction traffic and transport control
- error, ordering, versioning, and reconnect rules

This document intentionally does not define:

- certificate issuance and trust bootstrap policy
- canonical dialer and duplicate-session ownership policy
- remote render bootstrap, replay, or late-subscriber snapshot policy

Those remaining design gaps belong to:

- `task.t5-08a2` for trust and connection ownership
- `task.t5-08a3` for render bootstrap and replay

## 3. Accepted Transport Model

The accepted production transport stack is:

- `tonic` for gRPC over HTTP/2
- `prost` for protobuf schema generation
- `tokio` for async runtime and bounded queues
- `rustls` for authenticated TLS transport

The accepted production contract is:

- one repo-owned proto file at `proto/waitagent/remote/v1/node_session.proto`
- one protobuf package `waitagent.remote.v1`
- one primary bidirectional streaming RPC `OpenNodeSession`
- one typed protobuf envelope carrying logical message variants over that stream

WaitAgent must not keep the old production assumption of JSON envelopes,
base64 PTY payloads, or ad hoc framed sockets as the primary cross-host path.

## 4. Non-Negotiable Rules

1. The PTY owner stays remote.
   The server never pretends to own a remote PTY locally.
2. The protocol is app-agnostic.
   It carries terminal bytes, resize intent, publication state, and attachment
   control. It must not depend on recognizing Codex, shell, editor, or other
   TUI-specific semantic events.
3. Remote and local parity is terminal parity.
   The user should observe the same visible command, output, resize, and prompt
   behavior. The transport boundary may add latency, but it must not create a
   second interaction model.
4. One node session multiplexes many logical targets.
   WaitAgent must not open one production transport connection per target or
   per observer pane.
5. Input is shared and ordered by the server.
   Multiple consoles may send input to one target, but the server serializes
   target input order.
6. Output is authoritative at the PTY host.
   Ordered PTY bytes emitted by the authority node are the source of truth for
   what observers should display.
7. Viewport resize remains local.
   PTY resize is a target-scoped control-plane action; viewer-local geometry is
   not a transport-level PTY mutation by itself.

## 5. Proto Ownership

The accepted schema ownership is:

- file: `proto/waitagent/remote/v1/node_session.proto`
- protobuf package: `waitagent.remote.v1`
- generated code ownership: infra or transport-facing modules only

Higher-level runtime code must depend on a repo-owned transport facade rather
than calling generated `tonic` client or server stubs directly.

## 6. RPC Surface

The accepted service surface is:

```proto
syntax = "proto3";

package waitagent.remote.v1;

service NodeSessionService {
  rpc OpenNodeSession(stream NodeSessionEnvelope)
      returns (stream NodeSessionEnvelope);
}
```

Rules:

- `OpenNodeSession` is the only required steady-state RPC in protocol `v1`
- the stream is long-lived and node-scoped
- both directions carry discrete typed envelopes, not raw transport blobs
- uplink and downlink both use the same RPC, but their message semantics remain
  explicit by message type

Future unary or auxiliary RPCs may be added later for diagnostics or bulk
recovery, but the accepted phase-2 contract must not depend on them.

## 7. Envelope Contract

The accepted stream item is one typed envelope:

```proto
message NodeSessionEnvelope {
  string message_id = 1;
  google.protobuf.Timestamp sent_at = 2;
  string session_instance_id = 3;
  optional string correlation_id = 4;
  optional RouteContext route = 5;

  oneof body {
    ClientHello client_hello = 10;
    ServerHello server_hello = 11;
    Heartbeat heartbeat = 12;
    SessionNotice session_notice = 13;
    CommandRejected command_rejected = 14;

    TargetPublished target_published = 20;
    TargetExited target_exited = 21;

    OpenTargetRequest open_target_request = 30;
    OpenTargetAccepted open_target_accepted = 31;
    OpenTargetRejected open_target_rejected = 32;
    CloseTargetRequest close_target_request = 33;

    ConsoleInput console_input = 40;
    TargetInputDelivery target_input_delivery = 41;

    PtyResizeRequest pty_resize_request = 50;
    ApplyPtyResize apply_pty_resize = 51;
    PtyResizeApplied pty_resize_applied = 52;

    TargetOutput target_output = 60;
  }
}
```

`RouteContext` exists to carry the stable routing identifiers that matter for
diagnostics, fanout, and correlation:

```proto
message RouteContext {
  optional string authority_node_id = 1;
  optional string target_id = 2;
  optional string attachment_id = 3;
  optional string console_id = 4;
  optional string console_host_id = 5;
}
```

Rules:

- `message_id` is unique within one `session_instance_id`
- `correlation_id` points back to the initiating message when the current
  envelope is a reply or rejection
- `route` must carry only stable protocol identifiers, not transport-local
  socket names or pane ids
- message payloads use protobuf `bytes` for terminal data; no base64 wrapper is
  used in gRPC mode

## 8. Handshake And Session Control

### 8.1 Hello Exchange

The first envelope sent by the client node on a new stream must be
`ClientHello`.

The first successful reply from the server must be `ServerHello`.

Accepted handshake shape:

```proto
message ProtocolVersion {
  uint32 major = 1;
  uint32 minor = 2;
}

message ClientHello {
  string node_id = 1;
  string node_instance_id = 2;
  ProtocolVersion min_supported_version = 3;
  ProtocolVersion max_supported_version = 4;
  NodeCapabilities capabilities = 5;
  optional ResumeHint resume = 6;
}

message ServerHello {
  string server_id = 1;
  string session_instance_id = 2;
  ProtocolVersion negotiated_version = 3;
  google.protobuf.Duration heartbeat_interval = 4;
  RecoveryPolicy recovery_policy = 5;
}
```

Rules:

- the stream is not considered established until `ServerHello` is accepted
- the authoritative `session_instance_id` is assigned by the server
- transport authentication must already have happened underneath the RPC; the
  claimed `node_id` is still validated against that authenticated transport
  identity by later trust-policy design
- `ResumeHint` may reference the previously observed stream, but it does not
  guarantee replay or attachment restoration in protocol `v1`
- `NodeCapabilities` must at least advertise whether the node can publish
  targets, host observing consoles, send observing-console input, and consume
  authority-directed terminal control
- `RecoveryPolicy` must at least state whether authority republish is required,
  whether observer-side attachment reopen is required, and whether any replay
  facility exists at all

### 8.2 Heartbeat And Session Notices

The accepted control-plane keepalive messages are:

- `Heartbeat`
- `SessionNotice`

`Heartbeat` is symmetric and exists for application-level liveness and coarse
diagnostics in addition to HTTP/2 keepalive.

`SessionNotice` is server or client initiated and covers graceful session-level
state such as:

- `draining`
- `going_offline`
- `resync_required`

If the stream is rejected before `ServerHello`, the RPC must fail with a gRPC
status rather than an in-band envelope.

## 9. Message Families

### 9.1 Publication Messages

Authority nodes publish and withdraw targets through:

- `TargetPublished`
- `TargetExited`

Accepted `TargetPublished` fields:

- `target_id`
- `authority_node_id`
- `transport`
- `transport_session_id`
- optional `selector`
- `availability`
- optional presentation metadata such as command name, path, and attached count

Rules:

- the authority node is the only writer of authoritative target-host metadata
- repeated `TargetPublished` messages replace the current replicated metadata
  for that `target_id`
- `selector` is compatibility metadata, not the primary identity key

### 9.2 Observer Attachment Messages

Client-hosted observing consoles use:

- `OpenTargetRequest`
- `OpenTargetAccepted`
- `OpenTargetRejected`
- `CloseTargetRequest`

`OpenTargetRequest` carries:

- `target_id`
- `console_id`
- `console_location`
- opening viewport `cols` and `rows`

`OpenTargetAccepted` carries at least:

- `target_id`
- `attachment_id`
- `console_id`
- `availability`
- `resize_epoch`
- `resize_authority_console_id`

`OpenTargetRejected` carries at least:

- `target_id`
- `console_id`
- rejection `reason`
- structured `status`

Rules:

- opening viewport size describes the viewer surface, not an automatic PTY
  resize
- the server creates `attachment_id`
- attachment state is server-owned even though the request originates on a
  client-hosted console

### 9.3 Client-Originated Terminal Interaction Messages

Client-hosted console interaction sent toward the server uses:

- `ConsoleInput`
- `PtyResizeRequest`

`ConsoleInput` carries:

- `attachment_id`
- `target_id`
- `console_id`
- `console_seq`
- `input_bytes`

`PtyResizeRequest` carries:

- `attachment_id`
- `target_id`
- `console_id`
- `cols`
- `rows`
- `resize_epoch`

Rules:

- `console_seq` is monotonic per console
- `PtyResizeRequest` is only for PTY resize, not local viewer resizing
- a pure viewer resize stays local and must not be rejected by transport rules

### 9.4 Server-Originated Terminal Control Messages

The server sends authority-directed terminal control over the same stream using:

- `TargetInputDelivery`
- `ApplyPtyResize`

`TargetInputDelivery` carries:

- `attachment_id`
- `target_id`
- `console_id`
- `console_host_id`
- `input_seq`
- `input_bytes`

`ApplyPtyResize` carries:

- `target_id`
- `resize_epoch`
- `resize_authority_console_id`
- `cols`
- `rows`

Rules:

- `input_seq` is assigned by the server and is monotonic per target
- the authority node applies input strictly in `input_seq` order
- the server never sends application-specific semantic commands such as
  "show this Codex prompt"
- the only accepted server-originated interaction contract is generic terminal
  input or PTY-control delivery

### 9.5 Authority-Originated Output Messages

PTY-owning nodes send terminal output through:

- `TargetOutput`
- `PtyResizeApplied`

`TargetOutput` carries:

- `target_id`
- `output_seq`
- `stream`
- `output_bytes`

Rules:

- `output_seq` is assigned by the PTY owner and is monotonic per target
- the server forwards `TargetOutput` in order and must not renumber it
- observers render the same ordered bytes according to their local terminal
  surface
- protocol `v1` does not define separate application-level prompt or command
  event types beyond these terminal bytes

`PtyResizeApplied` confirms accepted PTY resize state for the current
`resize_epoch`.

## 10. Direction And Ownership Matrix

The accepted direction model is:

| Message family | Client node -> server | Server -> client node |
| --- | --- | --- |
| session control | `ClientHello`, `Heartbeat`, `SessionNotice` | `ServerHello`, `Heartbeat`, `SessionNotice` |
| target publication | `TargetPublished`, `TargetExited` | none in `v1` |
| observer attachment | `OpenTargetRequest`, `CloseTargetRequest` | `OpenTargetAccepted`, `OpenTargetRejected` |
| terminal interaction from observing console | `ConsoleInput`, `PtyResizeRequest` | none directly |
| authority-directed terminal control | none directly | `TargetInputDelivery`, `ApplyPtyResize` |
| authority terminal output | `TargetOutput`, `PtyResizeApplied` | `TargetOutput` fanout to observing nodes when applicable |
| recoverable command rejection | `CommandRejected` when server-issued command cannot be applied | `CommandRejected` when client-issued command is rejected |

Important ownership rule:

- server-hosted console interaction uses the same domain semantics, but it is
  routed in-process rather than emitted as wire messages

## 11. Error And Status Model

The accepted error model has two layers.

### 11.1 Transport Or Session Establishment Failures

Use standard gRPC status for:

- authentication failure
- authorization failure
- unsupported protocol version
- resource exhaustion
- unavailable server
- internal transport failure

Recommended gRPC mappings:

- `UNAUTHENTICATED`
- `PERMISSION_DENIED`
- `UNIMPLEMENTED`
- `RESOURCE_EXHAUSTED`
- `UNAVAILABLE`
- `FAILED_PRECONDITION`
- `INTERNAL`

These failures terminate or reject the RPC itself.

### 11.2 In-Stream Command Rejections

Recoverable application-level rejections use an in-band message:

```proto
message CommandRejected {
  CommandRejectedReason reason = 1;
  google.rpc.Status status = 2;
}
```

The first required rejection reasons are:

- `UNKNOWN_TARGET`
- `TARGET_OFFLINE`
- `ATTACHMENT_NOT_OPEN`
- `RESIZE_DENIED`
- `STALE_RESIZE_EPOCH`
- `WRITE_FAILED`
- `UNSUPPORTED_CAPABILITY`

Rules:

- `CommandRejected` must include `correlation_id`
- recoverable rejection must not tear down the whole node session by default
- transport failure must not be hidden inside `CommandRejected`

## 12. Ordering And Flow Control

The protocol must preserve:

- target metadata replacement order per `target_id`
- target input order per server-assigned `input_seq`
- PTY output order per authority-assigned `output_seq`
- PTY resize authority and application order per `resize_epoch`

The protocol does not require one total order across all targets.

Flow-control rules:

- each node connection actor owns one bounded outbound queue
- backpressure is applied at the node-session boundary, not by opening extra
  sockets
- slow consumers may delay delivery, but they must not change per-target order

Protocol `v1` deliberately defines discrete envelopes, not token-stream
semantics. Frequent PTY output chunks are expected; downlink command traffic is
typically much sparser.

## 13. Reconnect And Recovery

The accepted reconnect rule is session-scoped, not target-scoped.

When a node session drops:

- the server marks the node offline
- published targets from that authority remain in the catalog but become
  unavailable
- existing attachments remain logical server state
- new target input or PTY resize toward that offline authority fails fast

When the node reconnects:

- it opens a fresh `OpenNodeSession` stream
- it sends the same stable `node_id`
- it may send `ResumeHint` referencing the previous session
- after `ServerHello`, authority nodes must republish every live target
- observer-side client nodes must explicitly reopen any client-hosted
  attachments they want restored unless a later replay or recovery design says
  otherwise
- future PTY output resumes on the new stream with normal per-target ordering

Protocol `v1` does not guarantee:

- retroactive PTY output replay across disconnect
- late-subscriber screen snapshots
- attachment transcript recovery
- attachment-id reuse across observer reconnect

Those recovery semantics belong to `task.t5-08a3`.

## 14. Versioning Rules

The accepted version baseline is:

- protobuf package: `waitagent.remote.v1`
- negotiated protocol version: `1.0`

Compatibility rules:

- additive fields and additive `oneof` variants within `v1` are allowed when
  older peers can safely ignore them
- existing field numbers and existing `oneof` tags must not be renumbered
  within `v1`
- field meanings must not be reinterpreted incompatibly inside `v1`
- removing fields, changing semantics incompatibly, or changing ordering rules
  requires a new package version such as `waitagent.remote.v2`
- unsupported negotiated versions must be rejected during hello or RPC
  establishment, before steady-state payload handling begins

The proto package version and the negotiated protocol version should move
together unless a later migration explicitly justifies a different scheme.

## 15. Implementation Rules

Implementation must follow these constraints:

1. Generated gRPC or protobuf code stays behind one repo-owned transport
   boundary.
2. Business and UI-facing runtime code must not open raw production sockets or
   direct `tonic` channels on their own.
3. Network events must translate into application-owned events before any
   workspace or server-console UI runtime consumes them.
4. The protocol must remain terminal-oriented and app-agnostic.
5. No code may revive the old JSON or frame-level wire contract as the primary
   cross-host production path.

This is the frozen application protocol for the next implementation slice
`task.t5-08a`.
