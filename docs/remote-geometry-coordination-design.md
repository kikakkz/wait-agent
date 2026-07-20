# WaitAgent Remote Geometry Coordination Design

Version: `v1.0`
Status: `Accepted`
Date: `2026-07-19`

## 1. Purpose

This document defines how a server-side viewer and a client node's mirrored
main pane keep identical terminal geometry at all times, so that the raw PTY
data plane stays faithful when a server mirrors a live workspace main pane on
another host.

It complements:

- [raw-pty-tunnel-design.md](raw-pty-tunnel-design.md)
- [remote-live-mirror-design.md](remote-live-mirror-design.md)
- [remote-transport-stability-design.md](remote-transport-stability-design.md)

## 2. Problem Statement

The raw PTY tunnel is only correct when both panes have identical geometry.
The byte stream produced by the remote shell embeds cursor arithmetic computed
for the remote pane's width and height; replaying it verbatim onto a pane of a
different size corrupts any multi-row redraw (reverse-i-search, full-screen
TUIs), while plain sequential output appears to work.

Verified root cause on a live cross-host pair (server `192.168.31.178`,
client node `192.168.31.182`):

- the mirrored pane was the client node's workspace main pane, living in the
  chrome window together with the sidebar and footer panes
- `resize_pane_on_socket` skips resizing for multi-pane windows
  (`src/infra/tmux_backend/remote.rs:235-240`), so the pane stayed at 47x22
  while the viewer pane was 176x48
- `ResizeApplied` echoes the *requested* geometry instead of the *applied*
  geometry
  (`src/runtime/remote_authority/remote_authority_target_host_runtime.rs:753-760`),
  so the mismatch was invisible to every downstream gate

Controlled A/B comparison on the same host, same viewer, same code path: a
dedicated single-pane target session was resized to exactly the viewer
geometry and rendered perfectly (including reverse-i-search), while the
multi-pane workspace main pane garbled on the first multi-row redraw.

## 3. Requirements

- R1: both the server viewer and the client node's local user must always see
  complete content; neither side is ever clipped.
- R2: chrome layout is pinned on both hosts — sidebar at the right screen
  edge with fixed width, footer at the bottom screen edge with fixed height;
  coordination must never stretch or displace them.
- R3: server-side input must keep working at all times (central-management
  semantics); no read-only degraded mode, no input gating.
- R4: the raw PTY rendering path is preserved; no terminal-model replay is
  introduced into the interactive path.
- R5: the only acceptable visual concession is blank padding inside the main
  pane area on the larger host.
- R6: the last negotiated geometry is persisted per server on the managed
  node; different servers have independent entries.
- R7: visible flicker during negotiation is minimized.

## 4. Non-Goals

- no VT model / terminal-engine replay in the interactive path
- no read-only degraded mode and no input gating for mismatched geometry
- no dedicated handling for simultaneous multi-viewer contention beyond the
  min rule
- the separately tracked defects `task.bug-remote-main-slot-reconnect-seq-fatal`
  and `task.bug-remote-main-slot-snapshot-clobber`

## 5. Core Invariant and Coordination Rule

Invariant: the two main panes always share one geometry `T`. Because both
panes are the same size, raw byte relay is faithful by construction.

Coordination rule (network-wide `smallest` semantics, mirroring what tmux
does for same-host multi-client attach):

```
T.w = min(server_capacity.w, local_user_capacity.w)
T.h = min(server_capacity.h, local_user_capacity.h)
```

- capacities are taken per dimension, so each side uses as much of its screen
  as the other side allows
- server capacity = the operator's main-pane display area =
  `client size − server-side chrome overhead`, computed live (currently
  `209 − 32 − 1` × `51 − 1 − 1` = 176x48)
- local user capacity = the geometry the client node's attached tmux clients
  impose on the target pane (min over multiple attached clients); treated as
  unbounded while no client is attached
- multiple simultaneous server viewers participate in the same min rule

Because `T` is by definition within every viewer's capacity, both sides
always render the complete content (R1); the price is blank padding on the
larger side (R5).

## 6. Authority-Side Execution (client node)

When applying a negotiated `T` to a target pane:

- local user **not attached**: resize the window to `T + local chrome
  overhead` (overhead = `window size − target pane size`, queried live via
  tmux formats), then `resize-pane` the target pane to exactly `T`. Chrome
  panes keep their fixed sizes; the target pane absorbs the remainder.
- local user **attached**: the window follows the smallest attached client
  (per dimension), so the local user always sees the complete chrome layout.
  Note: waitagent windows run with `window-size manual`, so tmux does not
  snap the window on attach — the coordinator resizes the window explicitly.
  Chrome stays pinned at the screen edges (R2). Resize only the target pane
  to `T`; insert blank **padding panes** to absorb the slack beside and below
  the target pane, so the sidebar and footer are never stretched (R2).
  Padding panes are created silently (`split-window -d`) and the whole layout
  is then applied atomically with a single `select-layout` using a computed
  layout string (derived from `#{window_layout}`).
- after every resize, read back `#{pane_width}`/`#{pane_height}` and use the
  read-back values as the truth; if the applied geometry differs from the
  requested one, report the actual values.
- when the server disconnects, the window is left at `T`; there is no
  shrink-back (the pre-mirror size was an arbitrary detached default).
- chrome panes are identified by pane title/command
  (`waitagent-sidebar`/`waitagent-footer`), never by hardcoded index; chrome
  sizes are queried live, never hardcoded, so hidden-sidebar or custom-width
  layouts keep working.

## 7. Server-Side Execution

- the remote view window is resized to `T`; when the remote view occupies a
  bare dedicated window, tmux pads the unused client area automatically.
- the server capacity is recomputed whenever the operator's terminal changes
  (existing SIGWINCH path), producing a fresh desired geometry for the next
  coordination round.

## 8. Protocol

- **Truthful geometry**: `ResizeApplied` and the attach acknowledgement carry
  the read-back applied geometry. Echoing requested values is removed.
- **Geometry-change push**: the authority pushes unsolicited target-geometry
  changes to the server. Sources (implementation choice): tmux hooks
  (`client-attached`, `client-detached`, `client-resized`,
  `window-layout-changed` — same pattern as the existing `session-created`
  lifecycle hook) or a control-mode client listening for `%layout-change`.
  Pushes are debounced (150–300 ms quiet period) so terminal drag-resizes
  collapse into one coordination round.
- **Per-server geometry store**: the managed node persists
  `server authority id → last negotiated T` (with timestamp). Entries are
  per server (R6). Uses: (a) default size when creating a headless detached
  session (replaces the tmux 80x24 fallback); (b) initial geometry at mirror
  open, avoiding a large reflow jump. The store only provides initial
  values; runtime coordination always re-negotiates. The key must not depend
  on the per-boot session name; use server authority id plus node identity.
- protocol additions are backward-compatible extensions to
  `proto/waitagent/remote/v1` and `src/infra/remote_protocol.rs`.

## 9. Sequencing and Anti-Flicker

- **Negotiate before attach**: geometry is settled and read back before
  `pipe-pane` is installed and the bootstrap is captured, so the attach shows
  only the single bootstrap repaint that already exists today.
- **Runtime geometry change** (local user attaches/detaches/resizes): the
  server performs a controlled re-sync — resize its own window, clear the
  pane, re-run the bootstrap replay at the new geometry (erasing any
  transient redraw artifacts from the transition window), then resume raw
  output. This is one deliberate blink per geometry change, gated by
  debounce.
- **Atomic layout application** on the client node: padding panes first
  (silent), then one `select-layout`; no visible intermediate layout states.
- **Debounce on both sides**: a coordination round runs only after the
  geometry has been stable for the debounce window.

## 10. Edge Cases

- local user splits his window or changes layout: overhead is recomputed from
  live tmux state on every round; layout-change events trigger re-negotiation.
- local user resizes his terminal: debounced re-negotiation; his shell and
  TUI apps receive one SIGWINCH and redraw once.
- local user has several attached clients of different sizes: his capacity is
  their per-dimension minimum, matching tmux `window-size smallest`.
- computed sizes are clamped to tmux layout minimums; if a clamp prevents
  applying `T`, the read-back actual values are reported and used instead.
- the geometry store key is stable across reboots of the managed node
  (per-boot session names are not part of the key).

## 11. Test Strategy

- unit: `T` computation (per-dimension min, unbounded local capacity),
  chrome-overhead math for representative layouts (chrome present/hidden,
  extra splits).
- integration: simulated authority with a multi-pane chrome window; drive
  attach/detach/resize events; assert the pane geometry, padding pane
  lifecycle, truthful reporting, and that raw output renders cleanly through
  a geometry change.
- manual cross-host acceptance:
  - mirror a workspace main pane; run reverse-i-search with a long history
    line, `vim`, `htop`; output must be pixel-correct on both hosts
  - while mirrored, the local user attaches, detaches, and resizes his
    terminal; both sides stay complete and chrome stays pinned
  - operator resizes the server terminal; coordination follows
  - two remote sessions attached sequentially behave identically
  - input from the server keeps working through all of the above
