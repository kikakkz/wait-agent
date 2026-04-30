# WaitAgent Remote Session Foundation

Version: `v1.2`
Status: `Accepted`
Date: `2026-04-30`

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

## 3. Terms

- `target`
  A user-selectable session-like object that appears in sidebar or footer
  chrome and can be opened in a console.
- `console`
  One interaction surface such as the local workspace main slot or a
  future server-side workspace console.
- `open target in console`
  Bind one console surface to one target so that the console can observe and,
  when permitted, interact with it.
- `PTY owner`
  The machine that directly owns the live PTY for a target.
- `server control plane`
  The server-side routing, aggregate registry, and authority-coordination
  surface for remote targets. It is not the PTY owner for remote targets.

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

## 6. Target Model Requirements

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

Compatibility rule:

- local tmux selectors such as `socket:session` may remain as compatibility or
  CLI-facing selectors, but they must stop serving as the only internal target
  identity shape

## 7. Implementation Split

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
6. `task.t5-08 -> task.t5-08c`
   Close the remaining cross-host network gap by replacing local-only
   authority ingress assumptions, centralizing live node ownership, binding
   delivered remote output into visible console rendering, and now front-loading
   a dedicated node-connection architecture design.

## 8. Anti-Goals

- do not model remote targets as server-owned PTYs
- do not let remote work reintroduce detached-client switching or attach-based
  target changes
- do not make local tmux inspection the universal source of truth for target
  identity
- do not force remote-state probing into the same slice as registry-shape and
  transport-boundary work
- do not create a remote-only catalog shape that later has to be merged back
  into local chrome consumers
