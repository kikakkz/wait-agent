# Remote Host Connect And Session Creation Design

Version: `v1.1`
Status: `Accepted for task.feature-remote-session-creation-from-sidebar`
Date: `2026-06-16`

## 1. Purpose

This document defines the accepted architecture, command surface, module
boundaries, and implementation plan for explicit local and remote session
creation in WaitAgent.

It replaces the earlier implicit idea that `Ctrl-N` should create a new session
wherever the sidebar selection happens to point. That implicit behavior is
rejected because session placement would become hidden state.

The accepted model has three explicit user actions:

- create a local session
- connect or prepare a remote host
- create a session on an already connected remote endpoint

This design complements:

- [architecture.md](architecture.md)
- [protocol.md](protocol.md)
- [remote-node-connection-architecture.md](remote-node-connection-architecture.md)
- [remote-runtime-owner-architecture.md](remote-runtime-owner-architecture.md)
- [remote-live-mirror-design.md](remote-live-mirror-design.md)

## 2. Architecture Principles

The implementation must follow the existing WaitAgent architecture principles:

1. Raw terminal semantics stay intact.
   Remote session creation must create a normal remote target-host session. It
   must not create a special app-aware terminal mode.
2. PTY ownership stays on the host node.
   A server can request that a remote node create a session, but the remote node
   owns the resulting PTY.
3. Remote state comes from the runtime owner.
   Sidebar-visible remote sessions must still come from the backend-scoped
   remote runtime owner snapshot and session sync path, not from a new history
   file or command response cache.
4. One node connection multiplexes many logical sessions.
   Create-session messages use the existing node-scoped transport stream. The
   design must not add one SSH tunnel, gRPC stream, or socket per session.
5. Tmux-native chrome remains the local UI substrate.
   Key bindings, footer/menu entrypoints, and sidebar selection state should use
   tmux-native controls and session options where appropriate.
6. Runtime code depends on repo-owned facades.
   Higher-level runtimes should call WaitAgent services and transport facades,
   not generated tonic stubs or ad hoc SSH shell strings directly.
7. No silent fallback across placement boundaries.
   A remote creation failure must not create a local session instead.

## 3. Product Semantics

### 3.1 Key Bindings

Accepted bindings:

| Key | Action |
| --- | --- |
| `Ctrl-N` | Create a new local session. |
| Prefix-c | Hidden tmux-compatible alias for local session creation. |
| `Ctrl-W` | Open remote host connect/bootstrap workflow. |
| `Ctrl-S` | Create a new session on the currently selected remote endpoint. |

Prefix-c must remain available for tmux users, but it must not be displayed in
WaitAgent footer or menu text.

### 3.2 Footer And Menu Copy

The primary footer/menu copy should show only the explicit WaitAgent actions,
for example:

```text
Ctrl-N New · Ctrl-W Connect · Ctrl-S Remote New · Ctrl-O Fullscreen · Ctrl-E Logs · Ctrl-M Sessions
```

The exact wording may be adjusted for width, but the semantic split must remain
visible.

### 3.3 Remote Endpoint Meaning

A remote endpoint is a connected WaitAgent node, not one remote session. A
single endpoint may publish many remote sessions.

When a sidebar row points to a remote session, `Ctrl-S` uses that row's
authority node as the endpoint placement target for a new remote session.

## 4. High-Level Runtime Architecture

```text
Tmux key binding / menu action
        |
        v
WorkspaceCommandRuntime
        |
        +-- Local new session -> MainSlotRuntime -> TargetHostRuntime -> tmux target-host
        |
        +-- Selected remote new session
        |       -> SidebarSelectionState
        |       -> TargetRegistryService / RemoteRuntimeOwner snapshot
        |       -> RemoteSessionCreationService
        |       -> RemoteNodeSessionOwnerRuntime
        |       -> NodeSession stream CreateSessionRequest
        |
        +-- Remote host connect/bootstrap
                -> RemoteHostHistoryStore
                -> RemoteHostConnectRuntime
                -> SshRemoteHostBootstrapper
                -> remote install/start waitagent --connect local server
                -> RemoteRuntimeOwner endpoint wait
                -> RemoteSessionCreationService
```

The important ownership split is:

- `WorkspaceCommandRuntime` owns command dispatch.
- `MainSlotRuntime` owns local main-slot activation and existing local target
  creation.
- `RemoteHostConnectRuntime` owns SSH bootstrap and remote process startup.
- `RemoteNodeSessionOwnerRuntime` owns live node connections and remote session
  state.
- `RemoteSessionCreationService` sends create-session requests and waits for
  catalog convergence.
- The remote node's local target-host runtime owns the actual new remote PTY.

## 5. Command Design

### 5.1 Existing Local Command

`__new-target` remains the local creation command:

```text
waitagent __new-target   --current-socket-name <socket>   --current-session-name <session>
```

Semantics:

1. create a target-host session on the current local workspace socket
2. activate the new local session in the main pane
3. refresh chrome through the existing layout/chrome refresh path

This command must not inspect sidebar selection and must not create remote
sessions.

### 5.2 New Selected Remote Command

Add a hidden command for `Ctrl-S`:

```text
waitagent __new-selected-remote-session   --current-socket-name <socket>   --current-session-name <session>
```

Runtime semantics:

1. read the selected sidebar target for the current workspace
2. resolve the selected target through the shared target catalog
3. require `SessionTransport::RemotePeer`
4. derive `authority_node_id`, `session_id`, and cwd hint from the selected
   record
5. send a create-session request to that authority node
6. wait for the new session to appear through normal remote session sync
7. activate the new remote session in the fixed main slot

Failure semantics:

- local selection: fail with a clear message such as `selected target is local;
  use Ctrl-N for a local session`
- missing/offline/exited selection: fail clearly
- create rejection or timeout: fail clearly
- no local fallback is allowed

### 5.3 New Remote Host Connect Command

Add a hidden command for `Ctrl-W` after UI prompt/profile selection:

```text
waitagent __connect-remote-host   --current-socket-name <socket>   --current-session-name <session>   --profile <name>
```

Also allow direct non-interactive arguments for tests and future scripting:

```text
waitagent __connect-remote-host   --current-socket-name <socket>   --current-session-name <session>   --host <host>   --ssh-user <user>   --auth password|key|agent   [--key-path <path>]   [--remote-port auto|<port>]   [--save-profile <name>]
```

Runtime semantics:

1. load or create a remote host profile
2. collect runtime-only password or sudo password if needed
3. SSH to the host
4. verify/install/update WaitAgent
5. find or start a remote WaitAgent that connects back to the current local
   server
6. wait for the endpoint to appear in the remote owner snapshot/catalog
7. create a new remote session on that endpoint
8. activate the new remote session

No step may create a local fallback session.

## 6. Tmux Binding And UI Design

### 6.1 ControlService Bindings

Extend the existing native control binding model:

- keep `Ctrl-N` and Prefix-c bound to local `__new-target`
- bind `Ctrl-S` to `__new-selected-remote-session`
- bind `Ctrl-W` to a popup/profile picker or command prompt that eventually
  calls `__connect-remote-host`

Bindings should continue to be installed through `ControlService` and
`WorkspaceLayoutRuntime`, not by embedding shell logic in unrelated runtimes.

### 6.2 Selected Sidebar Target State

Persist the sidebar selected target in a tmux session option, for example:

```text
@waitagent_sidebar_selected_target=<authority:session>
```

The sidebar pane runtime updates this option when selection changes or when a
snapshot forces selection to a valid row.

Rationale:

- `Ctrl-S` can resolve selection without communicating directly with the sidebar
  process.
- The state is scoped to the tmux workspace session.
- This follows the existing tmux-owned chrome model.

Rules:

- clear the option when no valid selection exists
- never store passwords or host bootstrap data in tmux options
- treat the option as a pointer into the live catalog, not as authoritative
  remote state

### 6.3 Remote Host Connect UI

`Ctrl-W` should open a tmux-native popup/menu workflow.

Minimum accepted first version:

```text
Connect Remote
  <saved profile rows>
  + New connection
```

Selecting a saved profile runs `__connect-remote-host --profile <name>`.

Creating a new connection may use a prompt sequence or popup form. It must
collect:

- host
- ssh user
- auth method: password, key, or agent
- key path when auth is key based
- preferred remote port: auto or explicit
- optional profile name

Passwords are collected only at runtime and are not saved.

## 7. Connection History Design

### 7.1 Storage

Connection history is user-level configuration, not workspace runtime state.

Recommended path:

```text
~/.config/waitagent/remote-hosts.toml
```

This file is not a source of remote sidebar truth. It only stores connection
profiles for user convenience.

### 7.2 Data Model

Suggested model:

```toml
[[hosts]]
name = "130"
host = "10.1.29.130"
ssh_user = "kk"
auth_kind = "password"
key_path = ""
preferred_remote_port = "auto"
last_remote_port = 7476
last_endpoint = "10.1.29.130:7476"
last_connected_at = "2026-06-16T00:00:00Z"
```

Rust boundary:

```rust
struct RemoteHostProfile {
    name: String,
    host: String,
    ssh_user: String,
    auth: RemoteHostAuthProfile,
    preferred_remote_port: RemotePortPreference,
    last_remote_port: Option<u16>,
    last_endpoint: Option<String>,
    last_connected_at: Option<SystemTime>,
}

enum RemoteHostAuthProfile {
    Password,
    Key { key_path: PathBuf },
    Agent,
}
```

### 7.3 Security Rules

- never store plaintext SSH passwords
- never store sudo passwords
- key paths may be stored
- runtime logs must redact secrets
- command construction must avoid printing shell commands containing secrets

## 8. Remote Host Bootstrap Design

### 8.1 Module Boundary

Introduce a host bootstrap boundary separate from session creation:

```text
runtime/remote_host/
  remote_host_connect_runtime.rs
  remote_host_history_store.rs
  ssh_remote_host_bootstrapper.rs
  remote_port_probe.rs
```

Expected responsibilities:

- `RemoteHostHistoryStore`: read/write user profiles
- `RemoteHostConnectRuntime`: orchestrate profile selection results,
  bootstrap, endpoint wait, and remote session creation
- `SshRemoteHostBootstrapper`: execute SSH commands and file-safe remote checks
- `RemotePortProbe`: choose or verify a remote listener port

The first implementation may use the system `ssh` command behind an explicit
trait boundary. That keeps the workflow practical while preventing SSH shell
logic from leaking through workspace and remote owner runtimes.

### 8.2 Install Or Update

Remote check sequence:

```bash
command -v waitagent
waitagent --version
```

If WaitAgent is missing or the version is not acceptable, run on the remote
host:

```bash
curl -fsSL https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh | bash
```

Rules:

- installation runs over SSH
- if `curl` is missing, first implementation may fail clearly rather than
  adding package-manager-specific installation logic
- install failure stops the workflow
- stdout/stderr may be summarized, but secrets must not be logged

### 8.3 Remote Port Selection

Port selection happens on the remote host.

Rules:

- explicit profile port: try it first
- `auto`: try 7474 first, then the next available port
- if a WaitAgent process already uses a port and is connected to the current
  local server, reuse that endpoint
- if a WaitAgent process is running but connected to another server, start a new
  WaitAgent on a non-conflicting port
- if a non-WaitAgent process owns the desired port, choose another port in auto
  mode or fail for explicit port mode

The design should prefer protocol verification over process-name heuristics
where possible. Process inspection is allowed only as an optimization or
fallback diagnostic.

### 8.4 Remote Startup

A remote WaitAgent started by `Ctrl-W` must listen on the remote host and connect
back to the current local server:

```bash
waitagent --port <free-remote-port> --connect <local-server-host>:<local-server-port>
```

`<local-server-host>:<local-server-port>` comes from the current workspace
network configuration, the same listener identity shown in the footer.

Starting remote WaitAgent with only `--port` is rejected for this workflow
because it would not publish sessions back to the current server.

The remote process must be started so that the SSH command can return without
blocking on inherited stdout/stderr. The implementation should redirect or
otherwise detach remote output deliberately.

## 9. Remote Create-Session Protocol

### 9.1 Protocol Extension

Extend `proto/waitagent/remote/v1/node_session.proto` with new oneof variants on
`NodeSessionEnvelope`:

```proto
CreateSessionRequest create_session_request = 70;
CreateSessionAccepted create_session_accepted = 71;
CreateSessionRejected create_session_rejected = 72;
```

Suggested messages:

```proto
message CreateSessionRequest {
  string request_id = 1;
  string authority_node_id = 2;
  optional string cwd_hint = 3;
  uint32 cols = 4;
  uint32 rows = 5;
}

message CreateSessionAccepted {
  string request_id = 1;
  string session_id = 2;
  string target_id = 3;
}

message CreateSessionRejected {
  string request_id = 1;
  string reason = 2;
  google.rpc.Status status = 3;
}
```

Routing rules:

- route by `authority_node_id` to the node connection
- the created session receives a normal `session_id`
- the resulting visible target still enters the catalog through
  `TargetPublished`
- `CreateSessionAccepted` confirms the command was accepted; it is not the
  catalog source of truth

### 9.2 Domain Payloads

Mirror the proto messages in `infra::remote_protocol` domain payloads so local
loopback tests and gRPC transport remain aligned.

Expected payloads:

```rust
CreateSessionRequestPayload
CreateSessionAcceptedPayload
CreateSessionRejectedPayload
```

Mapping between proto and domain payloads should stay in transport-facing
modules, consistent with existing mirror and raw PTY mappings.

### 9.3 Remote Node Handler

On the PTY-owning node, handling `CreateSessionRequest` means:

1. validate that the node can host sessions
2. choose cwd from `cwd_hint` if valid, otherwise use the node default
3. create a normal local target-host session through existing local target-host
   runtime capabilities
4. respond with `CreateSessionAccepted` or `CreateSessionRejected`
5. rely on existing remote session sync to publish the new session

The handler must not create a special remote-only PTY path.

### 9.4 Server-Side Completion

The server-side command completes in two phases:

1. request accepted/rejected by the authority node
2. new session appears in the shared target catalog through session sync

Only after phase 2 should the command activate the target in the main slot.

Use a bounded wait window. Timeout should fail clearly and leave the remote
session sync path as the source of future eventual catalog appearance.

## 10. Session Creation Services

### 10.1 RemoteSessionCreationService

Introduce an application/runtime service such as `RemoteSessionCreationService`.

Responsibilities:

- resolve an authority endpoint from a selected remote session or connected
  profile
- generate request ids
- send create-session request through the remote owner/session facade
- wait for accepted/rejected response
- wait for catalog convergence
- return the new `ManagedSessionRecord` or a typed failure

It should not:

- run SSH
- inspect tmux panes directly
- fabricate catalog rows
- own remote process lifecycle

### 10.2 RemoteEndpointResolver

A small resolver should map between:

- sidebar selected target
- `ManagedSessionRecord`
- `authority_node_id`
- endpoint connection state

This resolver should consume `TargetRegistryService` and remote owner snapshots,
not file-backed remote caches.

### 10.3 Activation

Activation remains a main-slot responsibility.

After `RemoteSessionCreationService` returns a synced `ManagedSessionRecord`,
`MainSlotRuntime` or a thin command coordinator activates it using the same
remote activation path used for existing remote sessions.

## 11. Working Directory Rules

For `Ctrl-S`, cwd hint fallback order is:

1. selected remote session `current_path`
2. selected remote session `workspace_dir`
3. remote node default startup directory

For `Ctrl-W`, use the profile/default remote workspace dir when available. If
none is known, omit `cwd_hint` and let the remote node choose its default.

The server must not assume that a local path exists on the remote host.

## 12. Error Handling And User Feedback

Failures must be explicit and non-destructive.

Required failure cases:

- `Ctrl-S` selected target is local
- selected target is offline, exited, or missing
- no current local server listener is available for `Ctrl-W`
- SSH authentication failure
- remote install/update failure
- remote WaitAgent cannot start on any usable port
- endpoint does not appear after startup
- create-session request is rejected or times out
- new session does not appear in catalog within the bounded wait window

User-facing feedback can be `display-message`, popup text, and error log. The
first implementation may prioritize clear error log entries plus short tmux
messages.

Secrets must be redacted from all logs.

## 13. Testing Strategy

Use focused tests and resource limits consistent with this repository's test
constraints.

Unit tests:

- CLI parsing for new commands
- profile history read/write and secret omission
- sidebar selected-target option update behavior
- endpoint resolver local/remote/offline cases
- create-session proto/domain mapping
- remote creation service accepted/rejected/timeout paths

Runtime tests:

- local `Ctrl-N` remains local-only
- `Ctrl-S` rejects local selection
- `Ctrl-S` sends create request for selected remote authority
- accepted create waits for catalog publication before activation
- `Ctrl-W` reuses already connected endpoint
- `Ctrl-W` chooses non-conflicting remote port in bootstrap abstraction tests

Manual cross-host validation:

1. start WaitAgent server locally
2. use `Ctrl-W` to connect `10.1.29.130` with password or key profile
3. verify remote install/update uses the install script when needed
4. verify remote process starts as `waitagent --port <free> --connect <local>`
5. verify saved profile can be reused without retyping host/user
6. verify `Ctrl-S` on a remote row creates another session on the same endpoint
7. verify `Ctrl-N` still creates a local session

## 14. Implementation Slices

Recommended order:

1. UI copy and key binding cleanup
   - keep `Ctrl-N` local-only
   - hide Prefix-c from footer/menu copy
   - add placeholders/bindings for `Ctrl-W` and `Ctrl-S`
2. Sidebar selected target state
   - write selected target to tmux session option
   - add tests around selection changes and invalidation
3. Create-session protocol
   - proto extension
   - domain payloads
   - gRPC/local mapping tests
4. Remote node create-session handler
   - create normal local target-host session on authority node
   - respond accepted/rejected
5. Server-side remote creation service
   - send request
   - wait for acceptance
   - wait for catalog publication
   - activate synced target
6. `Ctrl-S` command path
   - selected remote endpoint resolution
   - failure handling for local/offline selection
7. Connection history store
   - TOML profile storage
   - no secret persistence
8. Remote host bootstrap runtime
   - SSH abstraction
   - install/update via install script
   - remote port selection
   - start `waitagent --port <free> --connect <local-server>`
9. `Ctrl-W` profile picker/new profile flow
   - saved profile reuse
   - new profile entry
   - endpoint reuse or bootstrap then remote session creation
10. Cross-host acceptance and cleanup

## 15. Acceptance Criteria

The task is complete when:

- `Ctrl-N` always creates a local session
- Prefix-c still works as a hidden local alias and is absent from primary
  menu/footer text
- `Ctrl-S` creates a session on the selected connected remote endpoint and
  activates it
- `Ctrl-S` on local/offline/missing selection fails clearly
- `Ctrl-W` can reuse a saved host profile
- `Ctrl-W` can add a new host profile without storing passwords
- `Ctrl-W` installs or updates remote WaitAgent using:

```bash
curl -fsSL https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh | bash
```

- `Ctrl-W` starts remote WaitAgent with:

```bash
waitagent --port <free-remote-port> --connect <local-server-host>:<local-server-port>
```

- already connected endpoints are reused rather than duplicated
- new remote sessions enter the sidebar through existing remote session sync
  and owner snapshot paths
- no new file-backed remote sidebar source is introduced
