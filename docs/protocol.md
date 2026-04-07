# WaitAgent Protocol

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

## 1. Purpose

This document defines the protocol between WaitAgent clients, servers, and attached consoles in network mode.

It exists to support:

- Cross-machine session visibility
- Mirrored interaction between local and server consoles
- Session lifecycle replication
- Resize propagation
- Reconnect and identity recovery

It complements:

- [architecture.md](architecture.md)
- [module-design.md](module-design.md)
- [functional-design.md](functional-design.md)

## 2. Scope

This protocol is only required for network mode.

Local mode must not depend on a live protocol implementation to function.

Local-first rule:

- The local MVP can ship before this protocol is implemented end to end
- Protocol-facing boundaries should still be designed now so network mode can layer on later

## 3. Protocol Principles

- PTY ownership stays on the PTY host
- Protocol messages carry terminal events and metadata, not semantic agent intent
- Sessions keep stable identities across reconnect whenever possible
- One session may be attached by multiple consoles
- Input and output propagation must preserve ordering as observed at the PTY boundary

## 4. Actors

### 4.1 Client

Owns local PTYs and agent processes.

### 4.2 Server

Aggregates node and session views, hosts server-side attached consoles, and routes remote input.

### 4.3 Attached Console

An interactive UI surface bound either to a client-local runtime or the server runtime.

## 5. Transport Assumptions

Recommended transport properties:

- Long-lived bidirectional connection
- Ordered delivery within one connection
- Backpressure support
- Heartbeat or liveness signaling
- Explicit reconnect handling

Suggested implementations:

- WebSocket
- Framed TCP

The protocol definition should remain transport-agnostic above the framing layer.

## 6. Identity Model

## 6.1 Node Identity

Each client node must have a stable `node_id`.

Suggested properties:

- Persistent across restarts when configuration persists
- Unique within one server deployment

## 6.2 Session Identity

Each session must expose:

- `session_id`
- `node_id`
- `address = <node_id>/<session_id>`

The PTY-owning client is the source of truth for session identity.

## 6.3 Console Identity

Each attached console should have:

- `console_id`
- `runtime_location`
- `attached_sessions`

This identity is useful for multi-console awareness and diagnostics.

## 7. Message Categories

The protocol should use explicit message envelopes.

Suggested high-level categories:

- Connection messages
- Node messages
- Session lifecycle messages
- Terminal stream messages
- Attach and focus messages
- Diagnostics and error messages

## 8. Message Envelope

Suggested envelope fields:

- `protocol_version`
- `message_id`
- `timestamp`
- `sender`
- `message_type`
- `payload`

Optional fields:

- `correlation_id`
- `session_address`
- `console_id`

## 9. Connection Messages

## 9.1 Client Hello

Purpose:

- Start protocol negotiation

Suggested payload:

- `node_id`
- `client_version`
- `capabilities`
- `auth_material`

## 9.2 Server Hello

Purpose:

- Accept or reject the connection

Suggested payload:

- `server_version`
- `accepted_protocol_version`
- `capabilities`
- `session_recovery_policy`

## 9.3 Heartbeat

Purpose:

- Maintain liveness

Suggested payload:

- `node_id`
- `session_count`
- `last_local_event_id`

## 10. Node Messages

## 10.1 Node Registered

Sent when:

- The server accepts a node into the active registry

## 10.2 Node Offline

Sent when:

- The server marks a node disconnected or unreachable

## 11. Session Lifecycle Messages

## 11.1 Session Started

Direction:

- Client -> Server

Payload:

- `session_id`
- `node_id`
- `address`
- `title`
- `created_at`
- `process_id`

## 11.2 Session Updated

Direction:

- Client -> Server

Payload:

- `address`
- `status`
- `last_output_at`
- `last_input_at`
- `screen_version`

## 11.3 Session Exited

Direction:

- Client -> Server

Payload:

- `address`
- `exit_code`
- `exited_at`

## 12. Terminal Stream Messages

## 12.1 Stdout Chunk

Direction:

- PTY owner -> attached observers

Payload:

- `address`
- `stream`
  Typically `stdout` or merged PTY output
- `bytes`
- `sequence`

Rules:

- Sequence must reflect PTY emission order
- Broadcast must preserve message order per session

## 12.2 Stdin Chunk

Direction:

- Console host -> PTY owner

Payload:

- `address`
- `console_id`
- `bytes`
- `sequence`

Rules:

- The PTY owner serializes write order
- Multi-console input is not semantically merged

## 12.3 Resize Applied

Direction:

- Console host -> PTY owner

Payload:

- `address`
- `cols`
- `rows`
- `applied_at`

## 12.4 Screen Snapshot Available

Direction:

- PTY owner -> observers

Payload:

- `address`
- `screen_version`
- `snapshot_ref` or inline compact snapshot

This supports focus restore and Peek.

## 13. Attach and Focus Messages

## 13.1 Attach Session

Direction:

- Console host -> server or local runtime

Payload:

- `console_id`
- `address`

Meaning:

- The console wants to receive mirrored state for the session

## 13.2 Detach Session

Direction:

- Console host -> server or local runtime

Payload:

- `console_id`
- `address`

## 13.3 Focus Changed

Direction:

- Console runtime -> local observers

Payload:

- `console_id`
- `from_session`
- `to_session`

Note:

- Focus is console-local
- Focus is not a protocol-global lock

## 14. Error Messages

Minimum error types:

- `unauthorized`
- `unsupported_protocol_version`
- `unknown_session`
- `node_offline`
- `attach_denied`
- `write_failed`

Suggested payload:

- `code`
- `message`
- `correlation_id`

## 15. Ordering Rules

The protocol must preserve:

- Output ordering per session
- Input ordering as applied by the PTY host
- Lifecycle ordering for one session

It does not need to preserve a single total order across all sessions.

## 16. Recovery Rules

## 16.1 Reconnect

On reconnect:

- The client presents the same `node_id`
- The client republishes active session state
- The server attempts to reconcile sessions by `address`

## 16.2 Session Recovery

If the client reconnects and a session still exists locally:

- Reuse the same `session_id`
- Restore the same `address`
- Resume mirrored streaming

## 16.3 Duplicate Prevention

The server should avoid duplicate session rows by treating `address` as the primary external identity.

## 17. Security Rules

Minimum rules:

- Client enrollment must be authenticated
- Session streams must not be anonymously attachable
- Credentials must be revocable

Not required for the first local MVP:

- Fine-grained RBAC
- Multi-tenant isolation
- Full audit replay pipeline

## 18. Local-First Implementation Notes

To support `local first, network later`, the protocol layer should be designed as an adapter boundary.

Recommended approach:

- Define protocol message types early
- Keep them isolated in `transport`
- Do not require a real server to run local sessions
- Optionally provide a `LoopbackTransport` for tests

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

