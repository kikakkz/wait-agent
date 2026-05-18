# WaitAgent Remote Transport Stability Design

Version: `v1.0`
Status: `Accepted — implementation ready`
Date: `2026-05-18`

## 1. Purpose

This document is the comprehensive architectural analysis of the gap between
WaitAgent's remote authority transport reliability and SSH-equivalent stability.
It exists because field experience with cross-host remote sessions revealed
three symptom classes:

- **Output freeze**: remote display stops refreshing while the authority PTY
  continues running; output silently dropped at the application layer.
- **Input loss**: keystrokes sent during a transport blip never reach the PTY.
- **"authority host already stopped" errors**: transient authority host exits
  that the session sync manager cannot self-heal transparently.

Each symptom has a specific root cause in the application-layer buffering and
timeout configuration. The kernel transport (BBR via gRPC's HTTP/2 + TCP) is
shared with SSH and is not the differentiator.

This document fixes the design before implementation: each layer is analyzed,
the fix is specified, and the implementation order is justified.

## 2. Architecture Overview

The remote session data path has four segments:

```
[PTY] ←→ [authority target host] ←→ [authority transport socket]
    ↓                                       ↓
 [tmux]                              [gRPC node session]
    ↓                                       ↓
 [tmux client] ← ... network ... ← [remote server]
```

- The **authority target host** (`src/runtime/remote_authority/remote_authority_target_host_runtime.rs`)
  is a local event loop that bridges the PTY output (from tmux `capture-pane`)
  to the authority transport socket, and reads PTY input from the transport
  socket, writing it to tmux `send-keys`.
- The **authority transport** (`src/runtime/remote_authority/remote_authority_transport_runtime.rs`)
  is a Unix socket pair bridged to a gRPC bidirectional stream. Frames are
  serialized as `AuthorityTransportFrame` envelopes.
- The **gRPC node session** (`src/infra/remote_grpc_transport.rs`) carries the
  stream over HTTP/2 with TCP keepalive (30s idle), HTTP/2 keepalive (1s
  interval, 5s timeout), and kernel BBR congestion control.

Every component between the PTY and the network has correct architecture. The
instability is entirely in **buffer sizing**, **send strategy**, **timeout
alignment**, and **reconnect policy**.

## 3. The Five Layers

### Layer 4 — Read Timeout Alignment (implement first)

**File**: `src/runtime/remote_authority/remote_authority_connection_runtime.rs`

```
AUTHORITY_TRANSPORT_READ_TIMEOUT = 120s
```

**Problem**: 120 seconds is the time to detect that a remote authority stream
has died. Under any network interruption longer than 120s, the local side
continues trying to read, no ping/pong exists to detect the break earlier, and
the user sees a frozen display for 2 full minutes before the reconnect path
activates. Additionally, there is no liveness probe at all — the read timeout
is a passive dead-peer detection mechanism, not an active health check.

**SSH equivalent**: SSH has `ServerAliveInterval` (default 0=off, but commonly
configured to 10-30s) and `ServerAliveCountMax` (default 3). A connection with
`ServerAliveInterval=15, ServerAliveCountMax=3` detects death in 45s without
waiting for TCP timeout. With aggressive settings, detection is 15-30s.

**Fix**:
1. Reduce `AUTHORITY_TRANSPORT_READ_TIMEOUT` from 120s to 15s.
2. Add an authority-level ping/pong frame pair. The authority target host sends
   a `Ping` frame every 10s of inactivity. The transport reader responds with
   `Pong`. Three missed pings = dead connection.
3. On ping timeout: close the transport socket, trigger reconnect.

**Why this is layer 4 (implement first)**: The read timeout governs how quickly
every other layer's recovery logic activates. A 2-minute dead-peer window
renders all other improvements moot because input/output blocking states
persist until the transport is re-established. Shortening this is the highest
leverage change.

**Implementation scope**:
- Add `AuthorityTransportFrame::Ping` and `AuthorityTransportFrame::Pong` variants
- Add a `last_activity` timestamp to the reader side
- Reader: check `last_activity` against 15s; on timeout, close connection
- Sender: after 10s idle, send `Ping` if no other frame was sent
- Pong is a no-op response frame (no action needed on receipt)

---

### Layer 1 — Output Channel Blocking Send (implement second)

**File**: `src/runtime/remote_authority/remote_authority_target_host_runtime.rs`

```
OUTPUT_CHANNEL_BOUND = 500  // sync_channel capacity
output_tx.try_send(msg)     // drops on full channel
```

**Problem**: The authority target host runs two threads:
1. **Output thread** — reads stdout/stderr from the PTY via tmux `capture-pane`
   and sends `TargetOutput` messages through `output_tx`.
2. **Event loop** — reads from `output_rx`, writes `AuthorityTransportFrame`s
   to the transport socket.

When the transport socket write blocks (network congestion, remote peer slow),
the event loop cannot drain `output_rx`. The channel fills to 500. Every
subsequent `try_send` from the output thread silently drops the frame. The
screen freezes. The dropped frames are **gone forever** — there is no replay
buffer, no retry, no gap notification.

Under BBR, TCP writes can block for 1-10s during congestion recovery. During
that window, a single PTY can easily produce >500 capture-pane frames (tmux
capture-pane fires on every pane change event, which can be 60+ events/sec
during heavy output).

**SSH equivalent**: SSH's `ChannelOutputDefender` (OpenSSH 9.6+) uses a
512KB output buffer per channel and **blocks the output producer** (waits on
`write(2)`) when the TCP send buffer is full. It does not drop data. The kernel
applies backpressure: when the remote is slow, the local writer sleeps.
Userspace drop is never the answer.

**Fix**:
1. Replace `try_send` with blocking `send` on the output channel.
2. Increase `OUTPUT_CHANNEL_BOUND` from 500 to 8192 frames.
3. Add a bounded output frame cache (replay ring buffer) of 1024 frames so
   that late-arriving observers can catch up without a full replay handshake.

**Why blocking send is safe**: The channel readers (event loop → transport
socket write) have a bounded buffer. When the transport blocks, the channel
fills, the output thread blocks, and backpressure propagates to the capture
source (tmux). This is correct: the capture source should slow down rather than
discard data, because the user needs every frame for continuous display.

**Why it was `try_send`**: Likely a fast-path assumption that the output
channel should never block the capture thread. But dropping data is worse than
blocking, and the block is bounded by the transport write timeout (500ms × 3
retries = 1.5s).

**Implementation scope**:
- Change `try_send` → `send` in the output thread
- Increase `OUTPUT_CHANNEL_BOUND` → 8192
- Add output frame cache ring buffer (VecDeque<OutputFrame>, cap 1024)
  - On new output: push to ring buffer after sending via channel
  - On SyncRequest (from Layer 3): replay missing frames from ring buffer

---

### Layer 2 — Input Ring Buffer (implement third)

**File**: `src/runtime/remote_authority/remote_authority_target_host_runtime.rs`

```
PENDING_INPUT_MAX = 64 * 1024  // 64KB FIFO capacity
```

**Problem**: Remote input arrives from the authority transport as
`AuthorityTransportFrame::RawPtyInput` and is written to a non-blocking FIFO
pipe (`O_NONBLOCK`). When the PTY write end is slow (tmux `send-keys` blocking
on PTY drain), the FIFO fills up. New input silently fails with `EAGAIN`.
The input is gone — the keystroke is lost.

Under TCP: a short network blip causes the authority transport to close and
reconnect. During the reconnect window, the FIFO has already drained, but any
input sent right at the reconnect boundary can be lost (sent via the old
transport, never processed).

**SSH equivalent**: SSH's input channel has a ~256KB receive buffer with
`read(2)` blocking. When the PTY is slow, TCP window closes, the remote
`write(2)` blocks. No data is dropped. Backpressure propagates to the sender.

**Fix**:
1. Replace the O_NONBLOCK FIFO with a large ring buffer (256KB).
2. Write to tmux `send-keys` in a dedicated thread that blocks on the ring
   buffer and retries on `send-keys` failure.
3. If the ring buffer reaches 75% capacity, send a `CongestionSignal` frame
   back to the remote peer so it can slow its input sender.

**Implementation scope**:
- Replace `PENDING_INPUT_MAX` FIFO with a `VecDeque<u8>` ring buffer (256KB)
- Spawn a dedicated input drain thread that reads from ring buffer and calls
  tmux `send-keys`, blocking on empty buffer
- On ring buffer >75%: send `AuthorityTransportFrame::InputCongestion(true)`
- On ring buffer <25%: send `AuthorityTransportFrame::InputCongestion(false)`

---

### Layer 5 — Reconnect Exponential Backoff (implement fourth)

**File**: `src/runtime/remote_node/remote_node_session_owner_runtime/owner_helpers.rs`

```
SHARED_AUTHORITY_RECONNECT_BASE_DELAY = 100ms
SHARED_AUTHORITY_RECONNECT_MAX_DELAY   = 1s
// Capped at 4 total attempts
```

**Problem**: The reconnect backoff starts at 100ms, doubles to 200ms, 400ms,
800ms, and is capped at 1s. After 4 attempts (~1.5s total), reconnection stops.
If the remote is still recovering (e.g., SSH reconnection, container restart),
those 4 attempts all fail and the session is permanently lost even though the
remote may come back 5 seconds later.

SSH retries for up to 10 minutes with `ServerAlive` + TCP retransmit. The
client doesn't give up after 4 attempts.

**Fix**:
1. Increase `SHARED_AUTHORITY_RECONNECT_MAX_DELAY` from 1s to 30s.
2. Increase the reconnect attempt cap from 4 to unlimited (stop only on
   explicit session close or process exit).
3. Add full jitter: `delay = random_between(base, min(max, base * 2^attempt))`.
4. After 10 failed attempts, add a longer cooldown phase: cap at 30s with
   jitter, continue retrying until the session manager explicitly stops.

**Why this matters**: Once layers 4, 1, and 2 are fixed, the transport is
resilient to short blips. But a longer interruption (10s+ network outage)
requires the reconnect logic to keep trying. Without this change, the user must
manually reconnect.

**Implementation scope**:
- Remove the attempt cap (4 → unlimited)
- Increase max delay (1s → 30s)
- Add full jitter to backoff calculation

---

### Layer 3 — Output Sequence Gap Detection (implement fifth)

**File**: `src/runtime/remote_authority/remote_authority_target_host_runtime.rs`

**Problem**: When the output channel drops frames (layer 1 fix prevents this),
or when a late-arriving observer needs to know what it missed, there is no way
to detect that a gap occurred. Output frames have no sequence number. The
observer sees a jump in screen state without knowing frames were lost.

Even after layers 1 and 4, there is a window between "transport closed" and
"reconnect completed" where output is produced but not sent. When the new
transport is established, the replay must know where to resume.

**SSH equivalent**: SSH's per-channel sequence numbers (`channel_rcv.window`,
`channel_snd.window`) allow both sides to detect dropped packets and trigger
retransmission at the channel level, independent of TCP.

**Fix**:
1. Add a `sequence_number: u64` field to every `AuthorityTransportFrame::TargetOutput`.
2. On the reader side (remote observer), track `last_sequence_number`. If
   `next_seq != last + 1`, emit a `SyncRequest(last_sequence_number, next_seq)`.
3. On the writer side (authority target host), on receiving `SyncRequest`:
   replay missing frames from the output frame ring buffer (layer 1) or
   trigger a full `capture-pane` sync if the ring buffer has been overwritten.
4. Add `AuthorityTransportFrame::SyncRequest` and
   `AuthorityTransportFrame::SyncResponse(sync_payload)` frame types.

**Why this is layer 5 (implement last)**: Without layers 1 and 4, gap detection
would constantly fire because frames are legitimately dropped. The detection
would create noise, not value. It only becomes useful after the underlying
stability layers prevent spurious gaps.

**Implementation scope**:
- Add `sequence_number` to `TargetOutput` frames
- Add `SyncRequest(u64, u64)` and `SyncResponse(Vec<OutputFrame>)` frame variants
- Reader: track sequence numbers, request sync on gap
- Writer: on sync request, replay from ring buffer or full capture

## 4. Implementation Order Rationale

| Order | Layer | Why this position |
|-------|-------|-------------------|
| 1 | Layer 4 (read timeout) | Prerequisite for all recovery paths; without it, dead-peer detection takes 120s |
| 2 | Layer 1 (output blocking send) | Highest user impact; fixes silent frame drops on every session |
| 3 | Layer 2 (input ring buffer) | Second-highest user impact; fixes lost keystrokes |
| 4 | Layer 5 (reconnect backoff) | Depends on layers 4, 1, 2 being stable to be meaningful |
| 5 | Layer 3 (seq gap detection) | Depends on all other layers; only useful after drops are eliminated |

## 5. Non-Goals

- **gRPC or HTTP/2 tuning**: The gRPC transport already has sane defaults (1s
  HTTP/2 keepalive, 5s timeout, TCP_NODELAY, BBR). Kernel-level tuning is
  orthogonal to this design.
- **PTY architecture changes**: The existing authority target host event loop
  is correct. This design does not propose rewriting it.
- **Adding a new transport protocol**: gRPC is sufficient. The issues are in
  how the application layer uses the transport, not in the transport itself.

## 6. Verification

Each layer must pass:
- `cargo test` — no regressions
- Manual test: connect to remote, induce network interruption (`tc qdisc` or
  `iptables` drop), verify recovery within layer's specified time bound
- Manual test: rapid keystroke during recovery window, verify no lost input
