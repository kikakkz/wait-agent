# Remote Peer Reconnection Plan

## Background

With the new outbound-dial mode the control host dials the remote peer's
`waitagent` daemon.  The legacy inbound (`--connect`) mode keeps the remote peer
dialing the control host.  Both modes must coexist because:

* Cloud/public remote peers can be reached from the control host → outbound dial.
* LAN remote peers behind NAT cannot be reached from a control host on the public
  internet → the LAN host must `--connect` back.

This document describes a unified reconnection design that handles both
directions robustly without requiring SSH restarts for transient network
failures.

## Current Problems

1. **No server-side reconnect for outbound dial.**
   `ReconnectWorker` only rebuilds the local Unix authority socket.  It does not
   re-establish the gRPC node session.  When the control host's outbound gRPC
   session drops, the remote daemon keeps listening but the control host never
   redials it.

2. **Credentials are regenerated on every connect.**
   `SshRemoteHostBootstrapper::ensure_waitagent_and_start` always runs
   `__generate-node-credentials`, even when a compatible daemon is already
   running.  The running daemon keeps the old certificate/key while the control
   host expects a new TLS pin, so reuse fails and the daemon is restarted.

3. **Duplicate daemon starts on repeated ctrl+W.**
   `RemoteHostConnectRuntime::connect` runs the port probe and bootstrapper
   **before** calling `find_connected_endpoint`.  If an online endpoint already
   exists, a new daemon is still started on another port and then left unused.

4. **No stored connection direction per profile.**
   A profile currently does not record whether it was established via outbound
   dial or inbound `--connect`.  Reconnect logic therefore cannot choose the
   correct direction automatically.

## Goals

* Reconnect without SSH-ing into the remote host to restart the daemon.
* Reuse an existing daemon when its identity, port, and credentials still match.
* Keep outbound-dial and inbound `--connect` paths separate and consistent.
* Preserve one-to-one isolation: one control server ↔ one remote daemon.
* Avoid orphan daemons when the user repeatedly connects to the same host.

## Design

### 1. Store connection metadata

Add a side table in `SharedState` keyed by `authority_node_id`:

```rust
pub(crate) struct RemoteNodeConnectionInfo {
    pub mode: RemoteNodeConnectionMode,
    pub host: String,
    pub port: u16,
    pub tls_pin_sha256: String,
    pub operator_key_path: Option<PathBuf>,
    pub profile_name: String,
}

pub(crate) enum RemoteNodeConnectionMode {
    /// Control host dials the remote peer.
    OutboundDial,
    /// Remote peer dials the control host (legacy --connect).
    InboundConnect,
}
```

The table is populated when a connection succeeds and cleared when the node is
marked offline permanently.

### 2. Mode-aware reconnect

When `StateEventLoop` receives `RemoteSessionDisconnected` or
`RemoteNodeOffline`:

| Mode | Who reconnects | Mechanism |
|------|----------------|-----------|
| `OutboundDial` | Control host | Spawn a worker that re-sends `InternalEvent::InitiateOutboundConnection` with the stored `OutboundNodeSessionRequest`, using exponential backoff and a max-attempt budget. |
| `InboundConnect` | Remote peer | Control host just keeps listening.  The remote peer's existing session-sync loop already redials automatically.  `ReconnectWorker` only rebuilds the local authority socket. |

For outbound dial the retry worker lives outside `StateEventLoop` so the single
writer is not blocked by network I/O.  Progress is reported back via
`StateEvent`:

* `RemoteNodeOnline` (new) when the gRPC session reopens.
* `RemoteNodeOffline` when the retry budget is exhausted.

### 3. Reuse an existing daemon

`RemoteHostConnectRuntime::connect` should first try to reuse before
bootstrapping:

```text
1. Load saved profile.
2. If profile has last_remote_port and tls_pin_sha256:
     a. SSH probe: is a waitagent running with the same node_id and port?
     b. If yes:
          - OutboundDial: skip credential generation, directly send
            InitiateOutboundConnection with stored endpoint/pin/key.
          - InboundConnect: skip bootstrap, just listen for it to reconnect.
3. If no compatible daemon is running, fall through to port probe + bootstrap.
```

To make reuse reliable:

* Do **not** regenerate credentials when a compatible daemon is detected.
* Store `last_remote_port`, `last_endpoint`, and `tls_pin_sha256` in the saved
  profile after the first successful connection.
* The TLS pin is derived from the remote daemon's certificate SPKI, so it is
  stable as long as the daemon keeps the same cert.

### 4. Connection direction is fixed per daemon

A daemon is started either in outbound-dial mode or inbound (`--connect`) mode.
Its direction cannot be changed without restarting it.

Rules:

* When reconnecting, use the mode stored in the profile.
* If the user explicitly changes the connection mode for a profile, kill the old
  daemon and start a new one with the new mode.
* If an existing `--connect` daemon points at the current control host, reuse it
  by listening (do **not** also outbound-dial the same daemon).
* If an existing `--connect` daemon points at a different control host, treat it
  as belonging to that host: pick another port and start a new daemon.

### 5. Fix duplicate daemon on repeated ctrl+W

Move the `find_connected_endpoint` check to the beginning of
`RemoteHostConnectRuntime::connect`:

```text
1. find_connected_endpoint(profile)
   - If found and online → create_remote_session(endpoint), done.
2. Try to reuse existing daemon (Section 3).
3. Only if neither works → port probe + bootstrap + dial/listen.
```

This prevents orphan daemons when the user ctrl+W's an already-connected host.

## Implementation Phases

### Phase 1: Store metadata and server-side outbound-dial reconnect

* Add `RemoteNodeConnectionInfo` and the side table in `SharedState`.
* Populate the table on successful `SessionOpened` / first online target.
* Add an outbound-dial retry worker triggered by `RemoteSessionDisconnected`
  / `RemoteNodeOffline`.
* Add `RemoteNodeOnline` event and wire it to cancel retry + re-run
  `ReconnectWorker`.

### Phase 2: Daemon reuse

* Change `SshRemoteHostBootstrapper` to skip credential generation when a
  compatible daemon is detected.
* Add remote probe for "daemon running with expected node_id and port".
* Update `RemoteHostConnectRuntime::connect` to try reuse before bootstrap.

### Phase 3: Mode-aware connect and explicit mode change

* Add `connection_mode` to `RemoteHostProfile`.
* Use stored mode to decide outbound dial vs. listening on reconnect.
* Detect mode changes and restart the daemon when necessary.

### Phase 4: Fix ctrl+W duplicate daemon

* Move `find_connected_endpoint` before bootstrap in
  `RemoteHostConnectRuntime::connect`.
* Ensure the bootstrapper is not invoked when an online endpoint already exists.

## Known Issues Recorded

* `find_connected_endpoint` is currently called after bootstrap in
  `RemoteHostConnectRuntime::connect`, causing unused orphan daemons on repeated
  ctrl+W.  Fix is Phase 4 above.
