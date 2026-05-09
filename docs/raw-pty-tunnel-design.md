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

## Remaining Cleanup

Observer/mirror behavior remains only for non-interactive uses that still need a
terminal model, such as sidebar previews, diagnostics, or retained replay.

Remaining cleanup:

- remove base64 from internal runtime payloads
- keep bytes in protobuf `bytes` fields at the gRPC boundary
- retire mirror bootstrap from the active interactive attach path
- document any remaining observer-only consumers

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
