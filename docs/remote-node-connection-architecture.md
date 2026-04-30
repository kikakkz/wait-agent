# WaitAgent Remote Node Connection Architecture

Version: `v1.0`
Status: `Accepted for task.t5-08a -> task.t5-08c`
Date: `2026-04-30`

## 1. Purpose

This document defines the accepted runtime architecture for real cross-host
remote node connectivity in WaitAgent.

It exists to make four decisions explicit before more transport work lands:

- how many long-lived connections WaitAgent should keep per remote node
- which runtime owns connection lifecycle, reconnect, and disconnect cleanup
- how multiple opened consoles share one remote target without multiplying
  transport sessions
- which mature framework stack should replace the current local-socket and
  thread-oriented production assumptions

It complements:

- [remote-session-foundation.md](remote-session-foundation.md)
- [remote-network-completion-plan.md](remote-network-completion-plan.md)
- [protocol.md](protocol.md)

## 2. Product And Runtime Rule

The accepted production model is:

- one long-lived node-scoped connection per remote authority node
- many logical remote sessions multiplexed over that node connection
- many console attachments fan out from server-owned target state, not from
  duplicated transport sockets
- remote interaction must preserve the same visible command, output, resize,
  and prompt semantics that the user would experience on the local path

This means:

- WaitAgent must not open one transport connection per target
- WaitAgent must not open one transport connection per observing console
- WaitAgent must not keep production behavior centered on pane-scoped or
  target-scoped Unix socket listeners

The transport session belongs to the node.
Remote sessions and attachments are logical state carried over that session.

User-experience rule:

- a command sent from the server toward a remote client must be rendered and
  applied on the client side the same way a corresponding local action would be
- a user interaction originating inside a remote terminal application must be
  reflected back to the server through generic PTY and control semantics rather
  than application-specific event decoding
- remote and local target interaction should therefore differ by transport
  boundary, not by user-visible interaction contract

## 3. Mature Framework Choice

The accepted framework stack for the production cross-host path is:

1. `tonic`
   gRPC transport, bidirectional streaming RPCs, HTTP/2 multiplexing, and
   integration with the Tower ecosystem.
2. `prost`
   Protobuf message generation for typed node-session payloads.
3. `tokio`
   Async runtime, task scheduling, timers, and bounded coordination channels.
4. `rustls`
   TLS and node-authenticated transport security through the gRPC stack.

This is the accepted stack because it gives WaitAgent:

- a mainstream service-to-service transport instead of a custom framed socket
  stack
- built-in long-lived bidirectional streaming over HTTP/2
- mature backpressure, keepalive, and connection-management behavior
- typed RPC contracts instead of hand-maintained transport envelopes
- production-ready TLS rather than inventing a custom secure transport layer

## 4. Non-Negotiable Rules

1. One node, one production connection
   The steady-state production path keeps one primary long-lived connection per
   remote authority node.
2. Multiplex by envelope, not by socket explosion
   Authority input, output, publication, heartbeats, and future control
   messages travel as logical channels over the node session.
3. Server owns attachment fanout
   Console attachments are server-side logical subscriptions to target output.
   Transport is not duplicated per observer.
4. Reconnect is node-scoped
   Reconnect, disconnect cleanup, and reclaim behavior belong to the node
   connection owner runtime, not to pane-local helpers.
5. Local bridges remain adapters only
   Existing Unix-socket and injected-stream seams remain useful for tests,
   loopback, and local bridge modes, but they are not the accepted cross-host
   production architecture.
6. No framework fork
   WaitAgent should not mix a gRPC-based production path with continued primary
   ownership in `std::thread` plus `std::os::unix::net` listeners.

## 5. Chosen Architecture

### 5.1 Node-Scoped Connection Manager

Add one top-level runtime boundary:

`RemoteNodeConnectionManager`

Responsibilities:

- accept or initiate node-scoped transport connections
- authenticate node identity at connection establishment
- keep one live connection record per `node_id`
- own reconnect and disconnect state
- expose a routing API for outbound target traffic
- publish inbound envelopes into server-side target and attachment state

This manager is keyed by `node_id`, not by target id, session name, pane id, or
socket path.

### 5.2 One Connection Actor Per Node

For each connected node, the manager owns one `RemoteNodeConnectionActor`.

The actor owns:

- one `tonic` channel or accepted inbound gRPC stream
- one inbound stream reader task
- one outbound stream sender task
- one bounded outbound queue
- one cancellation tree for coordinated shutdown
- one state machine for `connecting -> handshaking -> connected -> draining ->
  reconnecting -> offline`

The actor must be the only owner of the outbound stream sender.
Other runtimes send commands to it through bounded async channels.

### 5.3 Logical Multiplexing Model

WaitAgent already has the right logical shape in protocol terms:

- authority traffic
- remote-session catalog synchronization traffic
- session identity
- attachment identity
- console identity

The accepted architecture maps that shape onto one typed gRPC node session.

One primary bidirectional streaming RPC should carry:

- `TargetInput`
- `ApplyResize`
- `TargetOutput`
- optional server-to-client control or prompt projection only when that
  projection can be expressed in application-agnostic terminal semantics
- `TargetPublished`
- `TargetExited`
- heartbeat and reconnect metadata

In protocol `v1`, `TargetPublished` and `TargetExited` remain compatibility
wire names, but their product meaning is remote-session synchronization rather
than publication-centric target discovery.

This means WaitAgent does not need a second transport socket per target, and
it does not need a hand-rolled framing layer for production transport.
It needs a better owner runtime around one typed node-session stream.

### 5.4 Proto Contract First

Before the production gRPC ingress lands, WaitAgent must define one explicit
protobuf contract for node-to-node transport.

That contract should live in one repo-owned proto package such as:

- `proto/waitagent/remote/v1/node_session.proto`

The contract must define:

- one node-session service
- one primary bidirectional streaming RPC for steady-state node traffic
- typed messages for authority traffic, publication traffic, heartbeat, and
  reconnect metadata
- stable identifiers for `node_id`, session identity, `attachment_id`, and
  `console_id`

The accepted service shape is:

- one `NodeSessionService`
- one primary bidirectional stream such as `OpenNodeSession`

The accepted message strategy is:

- use protobuf `oneof` for transport message variants
- keep transport-level message types explicit instead of passing opaque blobs
- preserve existing domain semantics, but re-express them as protobuf schema
- keep `node_id` as transport identity, `session_id` as logical routing
  identity inside that node session, and `attachment_id` as a session-local
  observer handle rather than a transport key

This contract is not optional polish.
It is the boundary that fixes the application protocol before more transport
code lands.

### 5.5 Transport Facade Boundary

WaitAgent must not let gRPC client/server calls or raw socket access spread
through runtime code.

The accepted structure is:

- generated protobuf or gRPC code stays in one infra-facing module
- one transport facade wraps all client/server session establishment
- higher runtimes depend on that facade, not on `tonic::transport` directly

Accepted boundary examples:

- `src/infra/remote_grpc_proto.rs`
- `src/infra/remote_grpc_transport.rs`
- `RemoteNodeTransport` trait or equivalent facade

Rules:

- business and runtime modules outside the transport layer must not open TCP
  sockets directly for production node traffic
- business and runtime modules outside the transport layer must not construct
  ad hoc gRPC channels directly
- local bridge adapters may still exist, but they must terminate into the same
  transport facade boundary

This is required specifically to prevent direct socket and transport code from
being written all over the codebase again.

### 5.6 Attachment Fanout Model

For one target:

- many local workspace consoles may observe it
- one or more server-console surfaces may observe it
- all of them subscribe to server-side target state

Output fanout happens in two steps:

1. remote node sends one ordered output stream for the target
2. server fans that stream out to all attachment mailboxes for that target

The transport does not know about individual observer panes.
It only carries node messages and target identity.

### 5.7 Mirrored Interaction Parity

The accepted cross-host model is mirrored interaction, not a degraded remote
mode.

That means:

- when the server sends a command toward the remote client, the client must
  surface it in the same visible way the local path would surface the
  corresponding action
- when the remote terminal application emits user-facing interaction, the
  server must receive it through the same generic PTY output and control
  semantics that the local path would expose
- server-side observers should therefore be able to reason about remote target
  behavior with the same abstractions used for local targets

Protocol implication:

- the node-session proto must distinguish server-originated commands from
  client-originated terminal interaction traffic
- both directions still share one native bidirectional stream, but the message
  semantics must remain explicit

The goal is parity:

- remote input feels like local input
- remote output feels like local output
- remote command or prompt interaction feels like local interaction
- transport should be invisible to the user except for unavoidable network
  latency

Source-of-truth rule:

- PTY output produced on the remote host is the authority for what server-side
  observers should display
- the protocol must not depend on recognizing application-specific internal
  events from tools such as Codex, shells, editors, or other TUIs

### 5.8 Resize And Input Rules

Keep the existing accepted interaction rule:

- input is shared and ordered by the server
- output is broadcast
- PTY resize authority is target-scoped and exclusive when enabled
- viewport resize remains local to each console

The connection manager does not change these rules.
It only makes the underlying node connection architecture explicit.

## 6. Accepted Runtime Composition

The production composition should converge toward:

```text
Console Runtime
  -> Target Registry / Attachment Registry
  -> RemoteNodeConnectionManager
     -> RemoteNodeTransport facade
        -> RemoteNodeConnectionActor(node A)
           -> tonic bidirectional stream
           -> rustls-backed HTTP/2 session
        -> RemoteNodeConnectionActor(node B)
           -> tonic bidirectional stream
           -> rustls-backed HTTP/2 session
```

Inbound routing:

```text
Node Connection Actor
  -> envelope decode
  -> route by channel + target_id
  -> target state / publication state / attachment mailboxes
```

Outbound routing:

```text
Console or Target Runtime
  -> target-scoped command
  -> route by authority node_id
  -> RemoteNodeTransport facade
  -> bounded outbound queue on node actor
  -> gRPC stream send
```

## 7. Event Loop Coordination

### 7.1 Separate Loop Classes

WaitAgent must keep three loop classes separate:

1. Network event loop
   Owns gRPC stream IO, keepalive, reconnect, inbound decode, and outbound
   send queues for remote nodes.
2. Application or transaction event loop
   Owns target state transitions, attachment fanout, publication application,
   availability changes, and command routing decisions.
3. UI event loop
   Owns local chrome redraw, sidebar selection, main-slot presentation, and
   terminal-facing render updates.

The network loop must not block on UI work.
The UI loop must not depend on direct network calls.
The application loop is the translation boundary between them.

### 7.2 Coordination Rule

The accepted coordination model is:

- network runtime receives or sends transport messages
- network runtime translates transport messages into application events
- application services update target, attachment, or publication state
- UI-facing runtimes consume resulting application state or UI-safe events

In other words:

- network loop does transport
- application loop does state transitions
- UI loop does rendering

No layer should skip over the middle layer.

### 7.3 Internal Communication Protocol

WaitAgent needs two protocol layers, both defined in advance:

1. Node-to-node protocol
   The protobuf and gRPC contract for remote transport.
2. In-process runtime protocol
   The typed Rust event contract that connects network runtime to application
   orchestration and then to UI-facing runtimes.

The second layer must be explicit too.
Do not rely on ad hoc method calls or hidden cross-runtime side effects.

The accepted internal event classes are:

- `RemoteTransportEvent`
  Examples: node connected, node disconnected, transport failed, stream opened,
  stream message received.
- `RemoteDomainEvent`
  Examples: target output received, target published, target exited, node marked
  offline, remote attachment opened, resize authority changed.
- `UiProjectionEvent` or equivalent existing local-runtime event reuse
  Examples: session catalog updated, main-slot render update available,
  remote-status badge changed.

### 7.4 Local Event-Driven Runtime Integration

The accepted local path already has an event-driven boundary:

- `bootstrap::run -> CommandDispatcher -> WorkspaceCommandRuntime`
- local event publication through `LocalRuntimeEventBus`

The remote design must integrate with that model instead of bypassing it.

Accepted integration rule:

- remote network runtime publishes into an application-owned remote event bus or
  into a generalized event service adjacent to the existing local event bus
- `WorkspaceCommandRuntime`, remote main-slot runtime, and server-console
  runtime consume translated application events
- pane runtimes and UI runtimes keep consuming UI-safe events rather than raw
  transport messages

This means remote ingress is not a second UI loop and not a second hidden
control path around `CommandDispatcher`.

### 7.5 No Raw Transport In UI Or Runtime Surfaces

The following are not acceptable on the production path:

- pane runtime reading gRPC streams directly
- UI runtime owning reconnect logic
- workspace runtime opening ad hoc transport channels directly
- transport callbacks mutating chrome state without an application event
  boundary

The only acceptable path is:

`network transport -> remote application event -> state transition -> UI-safe event or projection`

### 7.6 Suggested In-Process Contract

The concrete Rust naming may evolve, but the architecture should converge
toward something like:

```text
RemoteNodeConnectionManager
  -> publish RemoteTransportEvent
  -> RemoteConnectionApplicationService
     -> publish RemoteDomainEvent
     -> SessionRegistryService / TargetRegistryService / AttachmentRegistry
     -> LocalRuntimeEventBus or sibling RemoteRuntimeEventBus
        -> WorkspaceCommandRuntime
        -> RemoteMainSlotRuntime
        -> RemoteServerConsoleRuntime
        -> EventDrivenPaneRuntime only through UI-safe projections
```

This preserves the accepted local event-driven rule that state changes are
published explicitly and consumed by the correct owners rather than rediscovered
by polling or hidden side effects.

## 8. Framework Usage Rules

### 8.1 `tonic`

Use `tonic` as the production node-to-node transport.

Accepted usage:

- one long-lived bidirectional streaming RPC per connected node for the
  steady-state node session
- generated protobuf message types for transport payloads
- generated client/server stubs wrapped behind one repo-owned transport facade
- tonic transport configuration for keepalive, flow control, and connection
  limits instead of custom socket plumbing

WaitAgent may still preserve domain-level message concepts such as target,
attachment, and console identity, but it should express them as protobuf
messages and one transport wrapper layer, not as a custom framing framework.

### 8.2 `tokio`

Use `tokio` for:

- async service startup
- connection actor loops around the gRPC stream
- bounded `mpsc` queues
- timers for heartbeat and reconnect backoff

Do not keep growing the production path with more blocking listener threads.

### 8.3 `rustls`

Use `rustls` through the gRPC transport stack for cross-host transport
security.

Accepted policy:

- node identity must be bound to authenticated transport establishment
- raw claimed `node_id` in an application payload is not sufficient production
  trust by itself
- mTLS is the preferred production model for authority-node trust

### 8.4 Local Adapters

Keep local bridge adapters, but demote them:

- Unix-socket listeners stay valid for loopback and tests
- queued injected streams stay valid for harnesses
- neither is the default production ownership model once cross-host ingress is
  implemented

## 9. Reconnect And Failure Model

The accepted reconnect model is:

- connection loss marks the node `offline`
- affected targets remain in the catalog but become unavailable
- existing attachments remain logical server state
- outbound target commands fail fast while the node is offline
- one reconnect owner loop attempts to restore the node session
- after reconnect, the node republishes live target state and resumes ordered
  target output delivery

Reconnect must not be spread across:

- pane runtime
- publication sidecar
- per-target helper bridge

Those pieces may observe connection state, but they must not each invent their
own reconnect policy.

## 10. Node Trust, Dialing, And Canonical Ownership Policy

### 10.1 Accepted Production Topology

The accepted phase-2 production topology remains server-centric for session
ownership, but listener lifecycle is now universal:

- every `waitagent` process starts one node-session listener at startup
- the default listener bind address is `0.0.0.0`
- the listener port is configured by CLI flag `--port <port>`
- if `--port` is omitted, WaitAgent uses default port `7474`
- the designated listener node remains the accepted rendezvous point for
  phase-2 remote sessions
- non-server WaitAgent nodes may still dial the server outbound
- the same long-lived outbound node session carries authority publication,
  authority input or resize delivery, observer open, observer input, and
  observer output fanout traffic

The accepted default is therefore:

- every node listens
- the server accepts inbound node sessions as the default rendezvous endpoint
- client or authority nodes initiate outbound node sessions toward a configured
  server endpoint when remote connectivity is desired
- server-originated dial-back is still not part of the accepted phase-2
  production contract

Reasons:

- always-on listeners give the product one predictable network surface instead
  of hidden late-start listeners tied to remote pane activation
- a fixed default bind address plus default port gives operators and users a
  concrete connection target even before custom deployment tuning
- retaining server-centric outbound dial as the default phase-2 flow still
  fits NAT and firewall realities better than requiring peer-to-peer inbound
  reachability everywhere
- one bidirectional stream already gives the server a native downlink channel
  without inventing a second transport

Future peer-to-peer or server-initiated dialing would require a separate design
task. They are not part of the accepted phase-2 production contract.

### 10.1.1 Accepted CLI Network Contract

The accepted user-facing configuration contract is CLI-first, not
environment-variable-first.

Rules:

- listener configuration must come from `--port`
- outbound remote dialing configuration must come from `--connect <host:port>`
- WaitAgent must not require environment variables as the primary product
  interface for production remote networking
- existing environment variables may survive only as temporary internal
  compatibility shims during migration, and they must be retired from active
  docs and user-facing guidance

Accepted defaults:

- listener bind address: `0.0.0.0`
- listener default port: `7474`

Accepted examples:

- `waitagent --port 7474`
- `waitagent --connect server.example.com:7474`
- `waitagent --port 7474 --connect hub.example.com:7474`

### 10.2 Transport Trust Model

The accepted production trust model is authenticated `rustls` mTLS.

Rules:

- the server presents a server certificate signed by a deployment-trusted CA
- every production client node presents a client certificate signed by a
  deployment-trusted CA
- the server listener requires client-certificate authentication on the gRPC
  path
- the client validates the server certificate before sending `ClientHello`
- local Unix-socket adapters and injected-stream test seams may remain
  unauthenticated local helpers, but they must not weaken the production gRPC
  admission rule

This means the production cross-host path does not rely on bearer tokens or ad
hoc shared secrets inside `ClientHello`.
Transport authentication must complete before application session negotiation is
considered valid.

### 10.3 Identity Binding Rule

The accepted identity binding is:

- one stable logical `node_id` per persisted WaitAgent installation
- one authenticated certificate identity per admitted node
- one allowlisted mapping from certificate identity to stable `node_id`

The certificate identity should use a mature URI SAN pattern rather than a
custom opaque string convention.
The accepted form is a SPIFFE-style URI SAN such as:

- `spiffe://waitagent/<deployment-id>/nodes/<node_id>`

The server identity should use the same family, for example:

- `spiffe://waitagent/<deployment-id>/server/control-plane`

Admission rules:

- the presented client certificate chain must validate to one trusted root
- the URI SAN must map to exactly one admitted `node_id`
- the `node_id` claimed in `ClientHello` must match the admitted `node_id`
- node capabilities claimed in `ClientHello` must not exceed the role or
  capability class configured for that admitted node

If certificate identity and claimed `node_id` disagree, the server must reject
the session before it becomes canonical.

### 10.4 Certificate Lifecycle Expectations

The accepted operational model is short-lived leaf certificates with overlap
rotation.

Rules:

- deployments own one CA bundle or small active root set for WaitAgent nodes
- node and server leaf certificates should be rotated with overlap rather than
  treated as effectively permanent
- root rotation may use overlapping trust bundles during migration windows
- production trust should prefer short certificate TTL plus explicit admission
  control over bespoke in-band revocation logic in phase 2
- self-signed leaf certificates are acceptable only for local development or
  explicit non-production bootstrap modes

Phase 2 does not require designing CRL or OCSP handling.
The accepted baseline is:

- authenticated trust chain
- bounded certificate lifetime
- explicit admission allowlist or registry

### 10.5 Admission Policy

The server-side connection manager is the admission authority for node sessions.

For every new inbound gRPC stream, the server must evaluate:

1. did the TLS layer authenticate the peer certificate successfully
2. does the certificate identity map to one admitted `node_id`
3. does `ClientHello.node_id` match that admitted identity
4. is the node currently allowed to connect in this deployment
5. do claimed protocol version and capabilities fit the accepted contract

Accepted outcomes:

- reject at RPC establishment with gRPC auth or precondition status if trust or
  identity checks fail
- accept the stream into handshake only after trust and identity checks pass
- allow the session to become canonical only after `ClientHello` and
  `ServerHello` complete successfully

### 10.6 Canonical Session Ownership

The server is the sole canonical-session arbiter for each `node_id`.

Rules:

- there may be many transient connect attempts, but at most one canonical live
  node session per `node_id`
- handshakes for the same `node_id` must be serialized under the node-scoped
  connection manager
- the stream that completes authenticated handshake last becomes canonical for
  that `node_id`
- any previously canonical stream for that `node_id` moves to `draining`
  immediately
- once a stream is marked `draining`, its future envelopes must not mutate
  authority, publication, attachment, or output state

When a canonical replacement happens, the server should:

- emit a session notice equivalent to `superseded`
- stop routing new outbound messages to the older stream
- require the new canonical session to republish live target state

This keeps ownership simple:

- nodes own outbound dialing and reconnect attempts
- the server owns final truth about which live stream is canonical

### 10.7 Duplicate Session And Split-Brain Resolution

Duplicate-session handling must distinguish trust failure from overlap recovery.

Case 1:
same claimed `node_id`, different authenticated certificate identity

- reject the new stream
- keep the existing canonical stream unchanged
- record a security-significant warning or metric

Case 2:
same claimed `node_id`, same authenticated identity, overlapping live streams

- accept handshake serialization
- newest successfully handshaken stream becomes canonical
- older canonical stream is drained and then closed

Case 3:
same logical node credential used concurrently by multiple real processes or
hosts

- WaitAgent cannot reliably infer operator intent from transport alone
- the same containment rule still applies: newest successful handshake wins
- the server should emit an operator-visible split-brain warning because this
  indicates credential reuse or deployment drift

This is intentionally a containment policy, not magical conflict resolution.
The product rule is:

- preserve one canonical session
- fail closed on identity mismatch
- fail over deterministically on same-identity overlap

### 10.8 Dialing Ownership And Reconnect Authority

Reconnect ownership is deliberately split in one precise way:

- the client node owns dialing, retry timing, and exponential backoff for its
  outbound session
- the server owns offline projection, canonical-session replacement, and drain
  or cleanup after stream loss

Client-side rules:

- keep at most one active production dial attempt in flight per node process
- do not keep multiple speculative outbound sessions alive hoping the server
  will choose one
- after disconnect, reconnect through one owner loop with bounded backoff and
  jitter

Server-side rules:

- mark the node offline only when no canonical live stream remains
- keep attachments as logical state while the node is offline
- reject or fail fast target commands that require the offline authority node
- accept a later successful reconnect as the new canonical session

This prevents reconnect policy from being split across panes, target helpers,
publication sidecars, or ad hoc transport wrappers.

### 10.9 Why Not Bidirectional Dialing In Phase 2

WaitAgent should not allow both sides to initiate competing production sessions
for the same node in phase 2.

That would immediately force extra policy around:

- simultaneous cross-dials
- double registration races
- more complex split-brain resolution
- firewall and reachability asymmetry
- duplicated reconnect owners

Because the accepted bidirectional gRPC stream already gives the server a
native downlink, the extra complexity does not buy the product anything in the
current phase.

## 11. Remote Render Bootstrap, Replay, And Late-Subscriber Recovery

### 11.1 Accepted Baseline

The accepted phase-2 render bootstrap model is ordered output replay, not a
separate remote-only screen-snapshot protocol.

That means:

- `TargetOutput` remains the primary remote render source of truth
- the server keeps a bounded per-target replay window of ordered output chunks
- a newly opened observer attachment replays that ordered window before
  consuming the live tail
- the observer continues to use the same local terminal engine and UI-safe
  projection path after bootstrap

This is accepted because it:

- preserves the app-agnostic terminal protocol
- reuses the same byte-stream semantics already used during live remote output
- aligns with the existing observer runtime that rebuilds terminal state from
  ordered `target_output`
- avoids inventing a second rendering protocol before the production ingress
  path even lands

### 11.2 Replay Ownership

Replay ownership belongs to the server-side application layer, adjacent to
target and attachment state, not to the UI layer and not to the authority node
transport facade alone.

The accepted runtime shape is one per-target replay store such as:

- `RemoteRenderReplayStore`

The store owns:

- ordered `TargetOutput` chunks by `target_id`
- the earliest retained output sequence
- the latest retained output sequence
- a bounded byte or chunk window
- target lifecycle markers such as `published`, `offline`, and `exited`

Rules:

- the replay store is fed only by authority-ordered `TargetOutput`
- the replay store is keyed by canonical `target_id`
- replay retention is bounded; phase 2 must not keep an unbounded transcript in
  memory for every remote target
- replay ownership is server-side because many observers may need the same
  bootstrap source

### 11.3 Observer Bootstrap Flow

The accepted open-target bootstrap flow is:

1. the observer opens a target and receives normal attachment metadata first
2. the server snapshots the target's current replay range
3. the attachment enters `bootstrapping`
4. the server replays retained `TargetOutput` chunks in original `output_seq`
   order into that attachment
5. once replay reaches the captured replay tip, the attachment enters `live`
6. subsequent live `TargetOutput` delivery continues on the same attachment in
   normal sequence order

Important rule:

- replayed historical output and live output use the same message type and the
  same `output_seq` space

This keeps the observer runtime simple:

- it does not need a separate bootstrap decoder
- it only needs ordered `TargetOutput` plus attachment metadata
- bootstrap and steady-state delivery differ by source window, not by message
  contract

### 11.4 Ordering And Catch-Up Rule

Late-subscriber recovery must preserve strict per-target output order.

Rules:

- when an attachment starts bootstrap, the server captures one replay tip
  sequence for that attachment
- live output with higher sequence numbers may continue to arrive globally, but
  that attachment must not observe them until replay reaches its captured tip
- attachment-local bootstrap state therefore has its own cursor:
  `next_output_seq_to_deliver`
- once bootstrap catches up to the captured tip, the attachment joins the live
  fanout set

This avoids interleaving:

- old replay chunk
- newer live chunk
- older replay chunk again

which would corrupt terminal state.

### 11.5 Reconnect And Reopen Semantics

In phase 2, observer-side recovery is reopen-based rather than attachment-id
reuse.

That means:

- if an observing client node disconnects, its remote attachment is treated as
  lost transport state
- after reconnect, the observer opens the target again and receives a fresh
  bootstrap pass from the replay store
- the new attachment may receive a new `attachment_id`

Authority-side reconnect is different:

- if the same target survives on the authority node, the server may keep the
  retained replay window already accumulated for that `target_id`
- after authority reconnect and republish, new output continues to append to
  that target's replay store
- if the target exits or is republished as a new target identity, the old replay
  store is closed and a new one starts

### 11.6 Truncation Policy

Because replay retention is bounded, late bootstrap can be degraded.

The accepted phase-2 rule is explicit truncation, not pretending recovery is
perfect when the retained replay window is incomplete.

Rules:

- the replay store may discard the oldest retained output once its configured
  memory or chunk budget is exceeded
- if an observer opens after truncation has already discarded earlier output,
  the server replays the earliest retained sequence it still has
- the attachment must be marked internally as `bootstrap_partial` until later
  redraw or further output naturally converges the visible state

Product implication:

- phase 2 guarantees ordered live remote interaction
- phase 2 does not guarantee perfect historical reconstruction for arbitrarily
  old or arbitrarily chatty remote sessions without a later checkpoint
  extension

This tradeoff is accepted because it keeps the protocol app-agnostic and lets
the first cross-host path land without inventing an expensive snapshot
subsystem first.

### 11.7 UI And Event-Boundary Integration

Bootstrap state must cross runtime boundaries as application events, not raw
transport callbacks.

The accepted event shape should include equivalents of:

- `RemoteReplayBuffered`
- `RemoteAttachmentBootstrapStarted`
- `RemoteAttachmentBootstrapAdvanced`
- `RemoteAttachmentBootstrapCompleted`
- `RemoteAttachmentBootstrapPartial`

Responsibilities:

- network runtime delivers ordered `TargetOutput`
- application service appends output to the replay store
- attachment bootstrap service replays output into observer mailboxes
- UI-facing runtimes consume only the resulting attachment state and terminal
  snapshots

This keeps bootstrap aligned with the same event-driven rule already accepted
for local runtime work.

### 11.8 Anti-Goals

Phase 2 remote render bootstrap must not:

- require the server to understand Codex, shell, editor, or other
  application-specific semantic events
- add a second remote-only rendering protocol alongside `TargetOutput` without
  a new architecture decision
- make UI runtimes read transport buffers directly
- promise perfect long-horizon replay while only retaining bounded history

## 12. Backpressure And Fairness

The accepted backpressure model is:

- bounded outbound queue per node actor
- bounded mailbox per attachment or per render consumer
- slow observer handling must not block the node transport writer forever

Rules:

- node actor write queues apply backpressure first
- fanout may drop stale render work at the attachment layer if the product only
  needs the latest visible state, but transport ordering for the node stream
  itself must stay intact
- the server should not allocate one unbounded queue per observer attachment on
  the hot output path

## 13. Why We Are Choosing gRPC Instead Of WebSocket Or Custom Framed TCP

WaitAgent should use `tonic` gRPC for the production cross-host path.

Why not raw framed TCP:

- it pushes framing, keepalive, stream lifecycle, and service semantics back
  onto WaitAgent
- that is exactly the kind of transport-framework ownership we want to avoid

Why not WebSocket:

- it is better suited when browser compatibility is a primary requirement
- WaitAgent node-to-node traffic is service-to-service, long-lived, typed, and
  internal
- with WebSocket we would still need to define more request, response, and
  operational semantics ourselves

Why gRPC is the accepted fit:

- typed protobuf contracts are a better match for control-plane messages
- HTTP/2 already gives us long-lived multiplexed transport behavior
- mature client/server stacks already exist in Rust
- Tower integration gives us well-understood middleware hooks for limits,
  retries, and observability

## 14. Implementation Mapping

### 14.1 `task.t5-08a`

Land the first production node ingress built on the chosen stack:

- `tonic` server/client transport
- protobuf node-session service definition
- `rustls`-backed authenticated transport
- one node-scoped connection source boundary
- one repo-owned transport facade that isolates generated gRPC code from the
  rest of the runtime

### 14.2 `task.t5-08b`

Converge ownership:

- create or reshape the node connection manager and actor runtime
- move reconnect and disconnect cleanup there
- fold publication and authority steady-state ownership into that model

### 14.3 `task.t5-08c`

Close the user-visible path:

- land node-scoped remote target discovery in the shared catalog before any
  target is manually opened, and do not depend on tmux publication bindings as
  the discovery gate
- bind output fanout into visible local and server-console rendering
- validate real cross-host open/input/output/resize behavior
- retire loopback-only assumptions from the accepted production path

## 15. Acceptance Rule

This architecture is accepted only if later implementation preserves all of the
following:

- one node-scoped long-lived production connection per authority node
- logical multiplexing by typed node-session messages instead of socket
  multiplication
- explicit reconnect ownership in one runtime boundary
- authenticated node identity binding and canonical-session arbitration in one
  runtime boundary
- explicit bootstrap, replay, and late-subscriber recovery ownership without a
  hidden second rendering protocol
- mature async runtime and service libraries rather than more hand-rolled
  connection plumbing
