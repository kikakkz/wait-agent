# Remote Host Connect And Session Creation Design

Version: `v1.2`
Status: `Accepted for task.remote-create-9-ctrl-w-correction`
Date: `2026-06-17`

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

This version corrects a v1.1 implementation mistake: `Ctrl-W` first-connect
semantics must match manually running `waitagent --connect <local-server>`. A
newly bootstrapped remote host publishes its default remote session and that
session is activated. WaitAgent must not create a second remote session after
first connect. Remote create-session is used only when an endpoint is already
connected, or when the user explicitly invokes `Ctrl-S`.

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
| `Ctrl-W` | Connect or reuse a remote host endpoint. |
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

`authority_node_id` is an identity and routing key. In the SSH bootstrap flow it
may be formatted as `<host>#<remote-port>` for stability, but that string is not
a dial target for server-side create-session. The server must not translate it
into `http://<host>:<remote-port>` for this workflow.

### 3.4 Ctrl-W First-Connect Semantics

`Ctrl-W` is the automated version of running this command on the remote host:

```bash
waitagent --connect <local-host>:<local-port>
```

Manual first connect creates or opens the remote WaitAgent's default shell
session, publishes it back to the local server, and does not create an extra
second session. `Ctrl-W` must preserve that behavior.

Accepted `Ctrl-W` behavior:

- if the target host endpoint is not connected, SSH bootstraps remote
  WaitAgent, waits for the default remote session to appear through normal
  catalog/session-sync, and activates that default session
- if the target host endpoint is already connected, `Ctrl-W` reuses that
  endpoint and creates one new ordinary remote session on it

`Ctrl-W` must not unconditionally create a remote session after bootstrap.

### 3.5 Ctrl-S Remote New Semantics

`Ctrl-S` is the explicit command for creating a new session on an already
connected remote endpoint. It does not perform SSH bootstrap and it does not
fall back to local creation.

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
        |       -> local remote-owner control channel
        |       -> existing node-session stream CreateSessionRequest
        |
        +-- Remote host connect/bootstrap
                -> RemoteHostHistoryStore
                -> RemoteHostConnectRuntime
                -> if endpoint already connected:
                |       -> RemoteSessionCreationService
                |       -> activate created remote session
                |
                -> if endpoint not connected:
                        -> SshRemoteHostBootstrapper
                        -> remote install/start waitagent --connect local server
                        -> wait for default remote session from catalog/session-sync
                        -> activate default remote session
```

The important ownership split is:

- `WorkspaceCommandRuntime` owns command dispatch.
- `MainSlotRuntime` owns local main-slot activation and existing local target
  creation.
- `RemoteHostConnectRuntime` owns SSH bootstrap, endpoint reuse decisions, and
  choosing whether to activate the first published default session or create a
  new session on an existing endpoint.
- `RemoteNodeSessionOwnerRuntime` or the node connection owner owns live node
  connections, remote session state, and create-session routing over an
  existing node session.
- `RemoteSessionCreationService` sends create-session requests through the
  local owner/control facade and waits for catalog convergence. It never dials
  a remote host:port directly.
- The remote node's local target-host runtime owns the actual remote PTY.

Unified event dispatch is a hard boundary. Application services may request
remote operations, but they must not create ad hoc remote gRPC/TCP connections
that bypass the owner runtime.

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

### 5.3 Remote Host Connect Command

The hidden command behind `Ctrl-W` is:

```text
waitagent __connect-remote-host   --current-socket-name <socket>   --current-session-name <session>   --profile <name>
```

It also allows direct non-interactive arguments for tests and future scripting:

```text
waitagent __connect-remote-host   --current-socket-name <socket>   --current-session-name <session>   --host <host>   --ssh-user <user>   --auth password|key|agent   [--key-path <path>]   [--remote-port auto|<port>]
```

Runtime semantics:

1. load or create a remote host profile
2. collect runtime-only password or sudo password if needed
3. check the live target catalog for an online endpoint matching the target host
4. if an endpoint is already online, create one new ordinary remote session on
   that endpoint and activate it
5. if no matching endpoint is online, SSH to the host
6. verify/install/update WaitAgent
7. find or start a remote WaitAgent that connects back to the current local
   server
8. wait for the endpoint's default remote session to appear in the shared
   catalog through normal session sync
9. activate that default remote session

No step may create a local fallback session. First-connect bootstrap must not
create an additional remote session after the default session appears.

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
- whether to remember the SSH password when password auth is used
- whether to remember the sudo password when remote install/start requires sudo

Passwords may be remembered, Xshell-style, but password values must be written
only to the dedicated secure store. The profile/history file stores only stable
secret references.

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
ssh_password_secret_id = "waitagent.remote-host.130.ssh-password"
sudo_password_secret_id = "waitagent.remote-host.130.sudo-password"
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
    Password { password_secret_id: Option<RemoteHostSecretId> },
    Key { key_path: PathBuf },
    Agent,
}
```

The profile file may store:

- host, username, auth kind, key path, and port preferences
- stable secret ids for remembered SSH or sudo passwords
- last successful endpoint and timestamps

The profile file must not store:

- SSH password values
- sudo password values
- one-time prompt answers
- generated SSH command lines containing secrets

### 7.3 Security Rules

- never store plaintext SSH passwords
- never store sudo passwords
- remembered SSH and sudo passwords are allowed only through
  `RemoteHostSecretStore`
- `RemoteHostHistoryStore` stores secret ids, never secret values
- secure-store backends must keep secret values outside
  `remote-hosts.toml`
- key paths may be stored
- runtime logs must redact secrets
- command construction must avoid printing shell commands containing secrets

### 7.4 Secure Store Boundary

Introduce a dedicated secret boundary:

```rust
trait RemoteHostSecretStore {
    fn put_secret(&self, id: &RemoteHostSecretId, secret: RemoteHostSecretValue) -> Result<(), Error>;
    fn get_secret(&self, id: &RemoteHostSecretId) -> Result<Option<RemoteHostSecretValue>, Error>;
    fn delete_secret(&self, id: &RemoteHostSecretId) -> Result<(), Error>;
}
```

Production code should prefer the platform secure store. On Linux the first
backend can use the FreeDesktop Secret Service through `secret-tool`; other
platforms can add native backends behind the same trait later. Tests use an
in-memory implementation.

Rules:

- secret values are passed to secure-store commands through stdin or native APIs
- secret values are never embedded in shell command strings
- secret ids are stable and non-secret, suitable for storage in profiles
- failure to save a remembered password must fail clearly or continue without
  remembering it according to the user's explicit choice

## 8. Remote Host Bootstrap Design

### 8.1 Module Boundary

Introduce a host bootstrap boundary separate from session creation:

```text
runtime/remote_host/
  remote_host_connect_runtime.rs
  remote_host_history_store.rs
  remote_host_secret_store.rs
  ssh_remote_host_bootstrapper.rs
  remote_port_probe.rs
```

Expected responsibilities:

- `RemoteHostHistoryStore`: read/write user profiles and secret references
- `RemoteHostSecretStore`: persist remembered SSH/sudo passwords in a dedicated
  secure store
- `RemoteHostConnectRuntime`: orchestrate profile selection results,
  endpoint reuse decisions, SSH bootstrap, endpoint wait, first-connect default
  session activation, and remote session creation only for already connected
  endpoints
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
waitagent --port <free-remote-port> \
  --connect <local-server-host>:<local-server-port> \
  --node-id <host#free-remote-port> \
  __remote-daemon
```

`<local-server-host>:<local-server-port>` comes from the current workspace
network configuration, the same listener identity shown in the footer.

`--node-id` is a stable authority identity. It must not be interpreted by the
server as a direct create-session endpoint.

`__remote-daemon` is a no-TTY startup mode for SSH bootstrap. It must be
semantically equivalent to manual `waitagent --connect <local-server>` for
session publication: the remote node still creates or owns its default shell
session and publishes it through normal session sync. It only suppresses TUI
attach and interactive UI rendering so the SSH command can return.

Starting remote WaitAgent with only `--port` is rejected for this workflow
because it would not publish sessions back to the current server.

The remote process must be started so that the SSH command can return without
blocking on inherited stdout/stderr. The implementation should redirect or
otherwise detach remote output deliberately.

### 8.5 First-Connect Activation

After SSH bootstrap, `RemoteHostConnectRuntime` waits for the first online
publishable target on the expected authority. That target is the remote
WaitAgent default session, matching the manual `waitagent --connect` behavior.

Completion criteria for first connect:

1. expected authority appears in catalog
2. at least one target on that authority is online
3. choose the target created or first published by the bootstrapped endpoint
4. activate that target in the main slot

The first-connect path must not call `RemoteSessionCreationService`.

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

- accept an already connected `authority_node_id` from a selected remote session
  or endpoint reuse decision
- generate request ids
- send create-session request through the local remote owner/session facade
- wait for accepted/rejected response
- wait for catalog convergence
- return the new `ManagedSessionRecord` or a typed failure

It should not:

- run SSH
- inspect tmux panes directly
- fabricate catalog rows
- own remote process lifecycle
- derive `http://host:port` or otherwise directly dial the remote host from
  `authority_node_id`

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

For `Ctrl-W` first-connect, no create-session request is sent, so no `cwd_hint`
is needed. When `Ctrl-W` reuses an already connected endpoint and creates a new
session, use the profile/default remote workspace dir when available. If none is
known, omit `cwd_hint` and let the remote node choose its default.

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
- profile history read/write and secret value omission
- secure-store boundary tests for storing/retrieving remembered SSH/sudo
  passwords without writing them to the profile file
- sidebar selected-target option update behavior
- endpoint resolver local/remote/offline cases
- create-session proto/domain mapping
- remote creation service accepted/rejected/timeout paths

Runtime tests:

- local `Ctrl-N` remains local-only
- `Ctrl-S` rejects local selection
- `Ctrl-S` sends create request for selected remote authority through the local
  owner/control facade, not by dialing remote host:port
- accepted create waits for catalog publication before activation
- `Ctrl-W` first-connect waits for and activates the default published session
- `Ctrl-W` first-connect does not call remote create-session
- `Ctrl-W` reuses an already connected endpoint and creates one new remote
  session
- `Ctrl-W` chooses non-conflicting remote port in bootstrap abstraction tests
- `__remote-daemon` publishes a default session like manual `waitagent --connect`

Manual cross-host validation:

1. start WaitAgent server locally
2. manually run `waitagent --connect <local>` on the remote host and record the
   default-session behavior
3. use `Ctrl-W` to connect `10.1.29.130` with password or key profile
4. verify first `Ctrl-W` publishes and activates exactly the default remote
   session, with no extra remote session
5. verify remote install/update uses the install script when needed
6. verify remote process starts as
   `waitagent --port <free> --connect <local> --node-id <host#free> __remote-daemon`
7. verify saved profile can be reused without retyping host/user
8. verify repeated `Ctrl-W` on the same connected host creates one new remote
   session
9. verify `Ctrl-S` on a remote row creates another session on the same endpoint
10. verify `Ctrl-N` still creates a local session
11. verify local server logs show no direct create-session dialing to remote
    `<host>:<port>`

## 14. Implementation Slices

The original v1.1 slices through `task.remote-create-8` are historical. The
corrective v1.2 implementation resumes from these slices:

1. `task.remote-create-9a-design-lock`
   Documentation-only slice. Lock corrected first-connect semantics and task
   split.

2. `task.remote-create-9b-remove-direct-dial`
   Remove `authority_node_id -> http://host:port` direct dialing from
   `RemoteSessionCreationService` transport. Replace it with a local owner
   transport boundary, even if the first step is a narrow local IPC facade.

3. `task.remote-create-9c-owner-create-session-routing`
   Implement request/reply routing inside the owner event loop using the active
   node session handle for the authority. This preserves the one-node-one-stream
   architecture.

4. `task.remote-create-9d-ctrl-w-first-connect`
   Change `RemoteHostConnectRuntime` decision logic: already connected endpoint
   creates a new remote session; newly bootstrapped endpoint waits for default
   session and activates it.

5. `task.remote-create-9e-daemon-parity`
   Verify and fix `__remote-daemon` so SSH bootstrap publishes a default session
   with the same semantics as manual `waitagent --connect`, without TUI attach.

6. `task.remote-create-9f-acceptance`
   Cross-host and localhost acceptance pass, including no direct server-to-remote
   create-session dialing.

Each implementation slice must remove or disable the superseded v1.1 behavior
it replaces. Do not leave the direct-dial path as fallback compatibility.

## 15. Acceptance Checklist

The corrected workflow is accepted only when all of the following are true:

- `Ctrl-N` creates and activates a local session only.
- `Ctrl-S` creates a new ordinary session on the selected connected remote
  endpoint and fails clearly for local/offline/missing selections.
- Manual `waitagent --connect <local>` on a remote host publishes one default
  remote session.
- First `Ctrl-W` to an unconnected host publishes and activates that same kind
  of default session and does not create an extra session.
- Repeated `Ctrl-W` to an already connected host creates and activates one new
  ordinary remote session.
- `__remote-daemon` is a no-TTY startup mode only; it does not change default
  session publication semantics.
- `authority_node_id` is never translated into a direct create-session
  `http://host:port` dial target by application/runtime service code.
- Remote input, output, resize, mirror, and exit behavior for sessions created
  by Ctrl-W/Ctrl-S matches ordinary remote session behavior.

## 16. Correction Plan From v1.1

The v1.1 implementation drifted in two important ways and those changes must be
corrected before further feature work builds on them.

### 16.1 Remove Direct Remote Dialing

Remove the server-side create-session path that converts an authority identity
into a remote TCP endpoint:

```text
host#port -> http://host:port -> RemoteNodeSessionRuntime::connect(...)
```

This is architecturally wrong because `Ctrl-W` relies on the remote node dialing
back to the local listener. The local server must route create-session requests
through the active node session already owned by the remote owner/ingress
runtime.

### 16.2 Restore First-Connect Semantics

Change `RemoteHostConnectRuntime` so that:

- existing endpoint: create and activate a new remote session
- newly bootstrapped endpoint: wait for and activate the default session

The new endpoint path must not call remote create-session.

### 16.3 Preserve `__remote-daemon` But Narrow Its Meaning

`__remote-daemon` remains valid only as no-TTY SSH startup mode. It must start
the same backend runtime needed for default session publication. If it does not
publish a default session like manual `waitagent --connect`, it is incomplete.

### 16.4 Keep Unified Event Dispatch

Create-session transport must be a local control facade into the owner runtime.
It must not open a separate production remote connection. This correction is
required to preserve the one-node-one-connection model from
`remote-node-connection-architecture.md`.

## 17. Correction Task Split

1. `task.remote-create-9a-design-lock`
   Lock v1.2 design and mark v1.1 Ctrl-W first-connect behavior as superseded.

2. `task.remote-create-9b-remove-direct-dial`
   Remove `authority_node_id -> http://host:port` direct create-session dialing
   and replace the transport boundary with a local owner/control facade.

3. `task.remote-create-9c-owner-create-session-routing`
   Route create-session requests through the active node session owned by the
   local remote owner/ingress event loop, including request/reply correlation.

4. `task.remote-create-9d-ctrl-w-first-connect`
   Change Ctrl-W so newly bootstrapped endpoints activate the default published
   session and only existing endpoints create a new session.

5. `task.remote-create-9e-daemon-parity`
   Verify and fix `__remote-daemon` so it publishes a default session with the
   same semantics as manual `waitagent --connect`, without attaching TUI.

6. `task.remote-create-9f-acceptance`
   Validate localhost, 10.1.29.130, manual `waitagent --connect`, first Ctrl-W,
   repeated Ctrl-W, Ctrl-S, and no local-server direct dialing to remote
   `<host>:<port>` for create-session.
