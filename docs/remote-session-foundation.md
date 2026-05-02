# WaitAgent Remote Session Foundation

Version: `v1.3`
Status: `Accepted`
Date: `2026-05-02`

## 1. Purpose

This document defines the accepted product and architecture baseline for
resumed remote session work.

It exists to answer four questions clearly:

- how remote targets should feel in the product
- where remote PTY ownership lives
- how local and server consoles interact with the same remote target
- how the resumed remote queue should be split into bounded implementation slices

It complements:

- [architecture.md](architecture.md)
- [protocol.md](protocol.md)
- [tmux-first-workspace-plan.md](tmux-first-workspace-plan.md)
- [remote-network-completion-plan.md](remote-network-completion-plan.md)
- [remote-node-connection-architecture.md](remote-node-connection-architecture.md)

## 2. Product Rule

Remote sessions are not a second workspace UX.

They are the same `target` product surface with a different interaction
backend:

- local targets appear in the sidebar
- remote targets also appear in the sidebar
- local and remote targets both open in the same persistent main slot
- local and remote targets both keep the fixed workspace chrome mounted
- local and remote targets both support fullscreen through the same main-slot
  presentation model

The user-visible distinction is not `local mode` versus `remote mode`.
The distinction is only:

- local target -> tmux-backed main-slot rebinding
- remote target -> network-backed interact runtime in the main slot
- `waitagent --connect` creates one node-level connection, not one session
- a connected remote node contributes its current local session set into the
  same shared catalog immediately after connect succeeds, including the current
  default session and any other already-existing local sessions owned by that
  backend

## 3. Terms

- `target`
  A user-selectable session-like object that appears in sidebar or footer
  chrome and can be opened in a console.
- `session`
  The concrete runtime object hosted by one node. In remote mode this is the
  primary product routing identity; the current UI may still render it as a
  target row.
- `local session set`
  The backend-scoped set of publishable local sessions owned by one node. This
  is the set that `--connect` exports to a remote server.
- `console`
  One interaction surface such as the local workspace main slot or a
  future server-side workspace console.
- `attachment`
  One console-to-session open handle. It is not a session identity and it is
  not a transport connection.
- `open target in console`
  Bind one console surface to one target so that the console can observe and,
  when permitted, interact with it.
- `PTY owner`
  The machine that directly owns the live PTY for a target.
- `server control plane`
  The server-side routing, aggregate registry, and authority-coordination
  surface for remote targets. It is not the PTY owner for remote targets.

Session export rule:

- a session is a user-visible, switchable shell or PTY context
- a session is not a tmux pane
- a session is not the fixed workspace chrome container
- a node connection is not a session
- current internal helper sessions such as `workspace-chrome` are local
  implementation details and must not surface as remote-visible sessions

## 4. Non-Negotiable Rules

1. One target model
   Local and remote targets must share one transport-agnostic catalog shape.
   Remote work must not fork a second session model just for network mode.
2. Remote PTY ownership stays remote
   A remote target remains PTY-owned by its host node.
   The server must not pretend to own that PTY locally.
3. One main-slot presentation model
   Opening a remote target must preserve the same fixed sidebar, fixed main
   slot, and fixed footer or menu contract used by the local tmux-first path.
4. Multiple consoles may open the same remote target
   A local workspace console and a server-side console may both observe the
   same remote target at the same time.
5. Shared input, broadcast output, scoped resize
   Multiple consoles may send input to the same remote target through the
   server control plane, remote output is broadcast to all opened consoles,
   attachment viewport resize remains local to each console, and only PTY
   resize is exclusive when the runtime chooses to propagate terminal-size
   changes to the PTY owner.
6. Status probing is a follow-on concern
   The resumed remote foundation work must first establish the correct target
   model and routing boundaries. Rich target-state inference for sidebar rows
   may follow later.
7. Export only backend-owned local sessions
   `--connect` must publish only the current backend's local session set.
   It must not scan and publish every tmux session visible on the machine.
8. Never republish remote projections
   Sessions learned from another node are remote projections. They may appear
   in the local catalog, but they must never be published again through this
   node's own outbound connection.

## 5. Interaction Model

The accepted remote interaction path is:

```text
remote PTY host
  <- ordered input from server control plane
  <- optional PTY resize from the current PTY-resize authority
  -> stdout or screen updates
  -> published remote target metadata

server control plane
  <- input from local and server consoles
  <- PTY resize requests when the runtime chooses to propagate them
  <- output and metadata from the PTY host
  -> ordered input to the PTY host
  -> output fanout to all opened consoles
  -> aggregate target catalog updates

console surfaces
  - local workspace main slot
  - future server-side workspace console
```

Remote interaction semantics are:

- `input`
  Shared across opened consoles and ordered by the server control plane.
- `output`
  Produced by the PTY owner and broadcast to all opened consoles.
- `viewport resize`
  Local to one console surface such as pane growth, pane shrink, or
  fullscreen. It must not be rejected by remote PTY-resize authority rules.
- `PTY resize`
  Optional target-level terminal-size propagation to the PTY owner. When used,
  it is accepted from exactly one console at a time.

The first accepted PTY-resize rule is:

- whichever console most recently opened the remote target becomes the resize
  authority until another explicit open or authority handoff occurs

## 6. Session Export Boundary

The accepted publishable session model is:

- one backend may own many local sessions
- each local session contributes exactly one user-visible row
- each published row must map to one routable `session_id`
- the exported set is backend-scoped, not machine-scoped

Current implementation guidance:

- the local workspace chrome session is not publishable
- the publishable local session is the user-facing target-host side of the
  backend session model, or a future equivalent exportable session abstraction
- local detached tmux artifacts that are not current backend-owned user
  sessions are not publishable just because tmux can still enumerate them

Initial synchronization rule:

- once `--connect` succeeds, the node must publish the full current local
  session set immediately
- the default local session is included in that set, but it is not the only
  session that must be published
- later session create, rename, exit, and availability changes flow as deltas

Projection rule:

- remote sessions learned from another node enter the local shared catalog as
  remote projections
- remote projections are locally consumable and switchable
- remote projections are not part of the local session set and must never be
  re-exported by `--connect`

## 7. Target Model Requirements

The shared target catalog must support at least:

- a transport discriminator such as `LocalTmux` or `RemotePeer`
- a stable target identity internal to WaitAgent
- the authority identity for the PTY owner
- a transport-local session identity
- an optional authority-local selector that can resolve the concrete PTY host on
  the authority node without replacing the canonical target identity
- coarse availability such as `online`, `offline`, or `exited`
- the list of consoles currently opened on that target
- each attachment's last-known local viewport size
- which console currently has PTY resize authority when PTY resize is active
  for that target

Remote-session rule:

- the product-facing record synchronized from a connected node is a remote
  session, not a publication-only target stub
- one node may contribute many remote sessions to the shared catalog
- one fresh outbound connection must contribute the node's current local
  session set before any manual open on the server is required
- the current default session must appear immediately as part of that set
- each contributed remote session must appear exactly once in the shared
  catalog and sidebar-visible surfaces

Compatibility rule:

- local tmux selectors such as `socket:session` may remain as compatibility or
  CLI-facing selectors, but they must stop serving as the only internal target
  identity shape

## 8. Implementation Split

The accepted resumed remote queue is:

1. `task.t5-06a`
   Lock the remote-session foundation docs and split the implementation queue
   into bounded slices.
2. `task.t5-06b`
   Generalize target identity and catalog records so local and remote targets
   share one transport-agnostic model.
3. `task.t5-06c`
   Extract the target-registry service boundary, keep local tmux as the first
   producer, and route current chrome consumers through unified target records.
4. `task.t5-07`
   Implement remote target input routing plus clean viewport-versus-PTY resize
   boundaries through the server control plane.
5. `task.t6-01`
   Implement the server-side workspace console as another target-opening
   surface that consumes the same shared catalog.
6. `task.t5-08 -> task.t5-08c4`
   Close the remaining cross-host network gap by replacing local-only
   authority ingress assumptions, centralizing live node ownership, correcting
   the remote shared-catalog model to `node -> sessions -> attachments`,
   routing interaction by session, and then binding delivered remote output
   into visible console rendering.

## 9. Anti-Goals

- do not model remote targets as server-owned PTYs
- do not let remote work reintroduce detached-client switching or attach-based
  target changes
- do not make local tmux inspection the universal source of truth for target
  identity
- do not expose `workspace-chrome`, pane ids, or machine-global tmux leftovers
  as remote-visible business sessions
- do not republish remote sessions through another node just because they are
  present in the local shared catalog
- do not force remote-state probing into the same slice as registry-shape and
  transport-boundary work
- do not create a remote-only catalog shape that later has to be merged back
  into local chrome consumers
