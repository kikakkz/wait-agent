# Raw PTY Tunnel Design

## Goal

Remote interactive sessions must behave like an SSH-attached PTY:

- local terminal raw-mode input is forwarded as bytes to the remote PTY
- remote PTY output is written to the local terminal as bytes
- WaitAgent does not synthesize cursor overlays, redraw remote output through a
  local terminal model, or translate interactive PTY data beyond required
  transport framing
- resize, target discovery, open/close, liveness, and publication remain typed
  control-plane messages

This design keeps the existing node-session and authority transport boundaries.
It does not introduce a raw TCP side channel.

## Current State

Remote interactive sessions now use the raw PTY data plane by default. The
remote path keeps two responsibilities:

- control plane: open target, close target, resize, publication, authority
  connection lifecycle
- interactive data plane: `RawPtyInput`, `RawPtyOutput`, optional one-time
  bootstrap bytes, and direct stdout writes

The authority host already uses the correct PTY backend primitive:
`tmux pipe-pane -I -O`.

- `-O` sends pane output to the output pump
- `-I` lets the output pump write bytes back into the pane
- the FIFO-based input path is preserved

## Target Architecture

### Control Plane

Keep typed messages for:

- target publication and withdrawal
- authority connection setup
- open target and close target
- resize request and resize applied
- target liveness and disconnect errors

Control-plane messages may remain on gRPC/node-session and the existing
authority transport facade.

### Data Plane

Introduce an interactive PTY byte stream with this contract:

- ordered bytes from local stdin to the authority-host pipe input
- ordered bytes from authority-host pipe output to local stdout
- no terminal-model replay in the interactive path after attach
- no cursor overlay or cursor reconstruction in the interactive path
- no base64 in internal runtime structs

Interactive remote surfaces must use `RawPtyInput` and `RawPtyOutput`.
`TargetOutput` remains available for observer/model consumers, but the active
remote-main-slot data plane does not use it.

## Attach Sequence

1. Local `remote-main-slot` enters terminal raw mode and opens the target through
   the existing control plane.
2. Authority target host activates `pipe-pane -I -O` for the selected target
   pane.
3. Authority sends a minimal attach acknowledgement.
4. Local side starts byte forwarding:
   - stdin bytes go to the authority-host input path
   - authority output bytes go directly to stdout
5. Resize events continue as sideband control messages.
6. Close or disconnect tears down the pipe and restores local terminal state.

Bootstrap screen replay is optional for the raw path. If retained, it must be a
one-time byte write before live output starts and must not install a local
observer as the source of truth for ongoing interaction.

## Geometry Coordination

The raw data plane is only correct when both panes share identical geometry.
For shared live-pane mirroring (a server viewing a client node's workspace
main pane), geometry cannot be imposed unilaterally, so it is coordinated as
a control-plane concern: negotiated target size, truthful applied-geometry
reporting, geometry-change events, and chrome-preserving resize execution.
The accepted contract is defined in
[remote-geometry-coordination-design.md](remote-geometry-coordination-design.md).
The interactive data plane itself remains raw bytes with no terminal-model
replay.

## Remaining Cleanup

Observer/mirror behavior remains only for non-interactive uses that still need a
terminal model, such as sidebar previews, diagnostics, or retained replay.

Remaining cleanup:

- remove base64 from internal runtime payloads
- keep bytes in protobuf `bytes` fields at the gRPC boundary
- retire mirror bootstrap from the active interactive attach path
- document any remaining observer-only consumers

## SSH Parity Optimization Backlog

The raw PTY path is now functionally correct enough to replace the legacy
remote input route, but the current implementation still contains several
latency sources that make it feel different from direct SSH. These are tracked
as bounded follow-up tasks rather than ad hoc patches.

### `task.t5-08c4d3e`: Decouple Local Input From Synchronous Transport Writes

The local stdin reader currently forwards each input chunk synchronously into
the authority transport. That keeps the UI event loop out of the keystroke
path, but the stdin thread can still block on route lookup, writer mutex
contention, socket backpressure, frame encoding, and `flush()`.

Target state:

- stdin reads enqueue bytes into a dedicated raw-input writer queue
- the stdin reader never blocks on network or authority-transport writes
- the writer may coalesce immediately-available burst bytes, but must not add a
  fixed sleep before sending
- backpressure is explicit and observable instead of silently stalling terminal
  input

### `task.t5-08c4d3f`: Replace FIFO And Output-Pump Bridging With Direct PTY Ownership

The authority host still reaches the target PTY through tmux `pipe-pane` plus a
separate output-pump process and FIFO-backed input. That adds process,
filesystem, framing, and flush boundaries that SSH does not have.

Target state:

- the authority host owns a direct read/write PTY handle or an equivalent tmux
  primitive that avoids a FIFO input hop
- input bytes are written directly to the remote PTY path from the authority
  host
- output bytes are read directly by the authority host without an extra
  output-pump process where practical
- teardown removes temporary sockets and FIFOs only as compatibility fallback,
  not as the normal data plane

### `task.t5-08c4d3g`: Use A Lightweight Raw PTY Frame Path

Raw PTY input and output still travel through payload structs that carry full
control-plane identity and message metadata on every chunk. Each frame is
flushed immediately.

Target state:

- raw input/output use a compact binary frame on the authority transport
- stable attachment/session/target route state is negotiated at mirror open and
  not repeated on every byte chunk unless needed for recovery
- per-frame allocation and string cloning in the hot path are minimized
- flush behavior is tuned for interactive latency without forcing one full
  metadata frame per typed character

### `task.t5-08c4d3h`: Make Authority Bridge Discovery Event-Driven

The ingress server still performs periodic authority-socket refresh scans. Raw
input events skip the refresh path, but the 250 ms timeout scan remains
background work that can create periodic jitter under many sessions.

Target state:

- authority sockets register and unregister through explicit lifecycle events
- ingress refresh no longer scans the temp directory on a fixed interval during
  steady-state interaction
- stale bridge cleanup remains reliable on disconnect or process death
- raw input/output routing does not compete with bridge discovery work

### `task.t5-08c4d3i`: Reduce Attach Bootstrap Capture Cost

Remote attach still performs multiple tmux queries to capture screen, cursor
position, and terminal flags before live bytes take over. That affects the
perceived speed of switching to a remote session.

Target state:

- attach uses the smallest bootstrap needed before live raw output starts
- tmux capture, cursor, and flag queries are batched or removed from the active
  interactive path
- placeholder-to-bash transition does not redraw through a stale local terminal
  model once the raw PTY stream is available
- cross-host validation compares attach latency and first-keystroke latency
  against SSH on the same machines

## Base64 Rule

Base64 must not be used as an internal raw PTY representation.

Allowed:

- compatibility shims where an existing text-framed transport still requires a
  string payload

Not allowed:

- local input translator producing base64 as the runtime source of truth
- authority host decoding base64 as a semantic PTY step
- using base64 to distinguish control messages from PTY data

The final data plane representation is `Vec<u8>` until it reaches a transport
codec. Any encoding is owned by that codec only.

## Test Strategy

Each slice must have a local simulated-remote test before cross-host testing.

Minimum local checks:

- start one local waitagent server and one local connected node
- activate a remote target
- type `ls` followed by Enter
- verify the target shell executes the command
- verify the local surface does not leave a stale cursor after `ls`
- run a simple full-screen command, resize, and exit

Cross-host checks:

- local node attaches to `10.1.29.130`
- remote shell prompt appears
- `ls Enter` behaves like SSH
- resize propagates
- disconnect restores the local terminal

Cleanup must stop waitagent test processes, tmux helper processes, temporary
authority sockets, and temporary authority FIFOs on both machines.

## Non-Goals

- no raw TCP side channel
- no return to `send-keys`
- no application-specific redraw hacks
- no fake cursor overlay
- no protocol-wide rewrite before the raw byte path is proven locally
