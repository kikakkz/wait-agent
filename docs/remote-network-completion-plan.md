# WaitAgent Remote Network Completion Plan

Version: `v1.4`
Status: `Accepted for task.t5-08 -> task.t5-08c`
Date: `2026-05-02`

## 1. Purpose

This document defines the accepted top-down plan for the remaining
cross-host work in `Phase 2: Network Aggregation MVP`.

It exists because the current codebase already has:

- a shared transport-agnostic target catalog
- remote target activation through the server control plane
- a dedicated server-console surface
- loopback and local-socket transport coverage for authority and publication

but it does not yet close the real cross-host product loop.

This document answers three questions explicitly:

- what product outcome is still missing
- which runtime gaps remain acceptable to tackle next
- how the remaining network work must be split into bounded tasks

It complements:

- [remote-session-foundation.md](remote-session-foundation.md)
- [remote-node-connection-architecture.md](remote-node-connection-architecture.md)
- [server-console-scheduling-design.md](server-console-scheduling-design.md)
- [protocol.md](protocol.md)

## 2. Product Outcome

Phase 2 is not complete until WaitAgent can treat a target hosted on another
machine as a real opened target rather than a loopback-only simulation.

The accepted outcome is:

- a PTY-owning authority node can connect through a real cross-host ingress
  boundary
- a local workspace console or dedicated server-console surface can open that
  target through the shared catalog
- input reaches the authority-owned PTY through the accepted control-plane path
- output returns over the same accepted node-session model and becomes visible
  in the opened console surface
- server-originated command interactions are displayed on the remote client in
  the same semantic shape as the local path, and remote terminal-app
  interactions map back to the server without a separate degraded UX contract
- remote session synchronization, disconnect, and reconnect behavior stay owned
  by explicit runtimes rather than ad hoc helper lifecycles

## 3. Non-Negotiable Rules

1. No fake local PTY ownership
   Cross-host completion must not regress to a model where the server pretends
   to own a remote PTY locally.
2. One target-opening model
   Local workspace and server console must keep using the same shared target
   catalog plus activation semantics.
3. Manual-only focus remains locked
   This queue must not reopen auto-switch or queue-ordering behavior.
4. Real node boundaries must become production boundaries
   Existing queued or loopback stream seams may remain as tests or local
   bridges, but the accepted production path must be able to accept real
   cross-host node streams.
5. Output must be visible, not only routed
   Delivering `target_output` into a mailbox or observer state alone is not
   enough for phase completion; the bytes must drive visible console
   presentation on the accepted product path.

## 4. Delivered Baseline

Already in code:

- remote activation routes through the shared target registry and accepted
  control-plane boundary
- authority registration, publication transport, and node-session handshakes
  have explicit runtime boundaries
- local loopback and local-socket transport paths exercise those boundaries
- the server-console product surface exists and stays manual-only

Not yet complete for the phase outcome:

- the main production authority-registration path is still anchored on local
  Unix-socket ingress or injected test streams rather than a real cross-host
  source
- the product still lacks one public always-on listener lifecycle and public
  outbound dial contract, so real cross-host validation cannot yet run through
  a normal user-facing `--port` / `--connect` workflow
- steady-state live node ownership is still split across several runtimes and
  helper boundaries
- remote output delivery is routed, but the final visible render path and
  end-to-end cross-host validation are not yet closed
- the most recent implementation batch also proved that a publication-centric
  discovered-target model is not the accepted product end state, so the
  remaining work is now explicitly session-centric

Design baseline now fixed before implementation resumes:

- the gRPC proto and RPC contract are explicit
- production trust, dialing direction, duplicate-session handling, and
  canonical connection ownership are explicit
- remote render bootstrap, replay, and late-subscriber recovery are explicit

## 5. Remaining Queue

The accepted remaining order is:

1. `task.t5-08a`
   Introduce a real cross-host authority ingress source and make it the
   accepted production registration path above the existing source boundary.
2. `task.t5-08b`
   Centralize live node-session ownership, registration, and reconnect or
   disconnect handling behind one accepted runtime boundary.
3. `task.t5-08c2`
   Synchronize connected remote node sessions into the shared catalog so a
   connected node contributes its default session immediately and later session
   lifecycle changes follow the same catalog model.
4. `task.t5-08c3`
   Route remote control-plane traffic by session and demote attachment to
   observer scope.
5. `task.t5-08c4a -> task.t5-08c4c`
   Replace file-backed remote sidebar state with one backend-scoped runtime
   owner and make the live owner snapshot the only accepted remote catalog
   source.
6. `task.t5-08c4d1`
   Correct the backend-scoped local session export boundary so node
   connections publish only backend-owned local sessions.
7. `task.t5-08c4d2`
   Validate detach or reattach continuity plus owner restart semantics on the
   corrected runtime-only path.
8. `task.t5-08c4d3`
   Validate end-to-end cross-host open, input, output, resize, and shutdown on
   the corrected runtime-only path.

`task.t3-07` remains optional and deferred. It must not preempt this queue
unless acceptance evidence proves compact-layout work is blocking the product.

## 6. Task Boundaries

### 6.1 `task.t5-08a`

This slice owns:

- the first real external authority-stream source
- production ingress ownership for accepted remote authority connections
- preserving the current connection-source abstraction while replacing the
  local-only assumption on the default path

This slice does not own:

- final output rendering
- queue or focus UX changes
- a second transport-specific console model

### 6.2 `task.t5-08b`

This slice owns:

- durable live node-session ownership
- authority and publication lifecycle coordination
- reconnect, disconnect, and registry cleanup semantics

This slice does not own:

- redesigning the target catalog shape again
- ad hoc helper caches that bypass the owner runtime

### 6.3 `task.t5-08c`

This slice owns:

- turning delivered remote output into visible console state on the accepted
  local and server-console paths
- end-to-end validation of real cross-host remote interaction
- retiring any remaining loopback-only assumption from the phase-2 path

This slice does not own:

- optional compact-layout polish
- later diagnostics or security hardening beyond what is necessary to prove
  the network MVP works

## 7. Completion Rule

Phase 2 should be considered complete only when:

- a remote target hosted on another machine can be opened through the shared
  catalog on the accepted product path
- a user can observe visible remote output and send input back successfully
- disconnect and reconnect behavior no longer depends on local loopback-only
  helpers
- the task board, status board, and verification state all reflect that
  completed path
