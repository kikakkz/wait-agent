# WaitAgent Ratatui Plan to Replace tmux

Version: `v1.0`  
Status: `Accepted`  
Date: `2026-07-27`

## 1. Purpose

This document records the decisions, ordering, and boundaries for migrating WaitAgent from a tmux-first architecture to a ratatui-first architecture.

Motivation:
- Using tmux as the display pane brings many complex problems: multi-process communication anomalies, consistency handling, geometry coordination, and more
- The existing vendored tmux build, subprocess lifecycle, hooks/session-options, and related machinery are too heavy
- ratatui + crossterm + an off-the-shelf terminal emulator library are already sufficient to support a single-process TUI

## 2. Decision Summary

### 2.1 No patching on top of tmux; open a parallel path

Launch a new path fully owned by ratatui via the `--ratatui` flag. Keep the tmux path until the ratatui path is stable, then switch the default and remove the tmux backend.

### 2.2 Migration Order

```
Phase 0: --ratatui flag + full placeholder layout + server/client skeleton + detach
Phase 1: real data in the footer status bar
Phase 2: sidebar session catalog + navigation
Phase 3: main pane PTY + terminal emulator
Phase 4: remove the tmux backend
Phase 5: cloud capabilities (local↔cloud, web client deferred)
```

### 2.3 Keep the Distributed Protocol

`proto/waitagent/remote/v1/node_session.proto` already provides distributed primitives such as node, authority, console, and attachment; keep reusing it. The new architecture splits the monolithic binary into a node server + TUI client, and the protocol naturally supports topologies such as local→local, local→cloud, and cloud→local.

## 3. Architecture Goals

```
waitagent-node (server / authority)
  ├─ holds local PTY, terminal emulator, session catalog
  ├─ implements the NodeSessionService gRPC server
  ├─ keeps running after detach
  └─ remote ingress / session sync sidecars retained

waitagent-cli (TUI client)
  ├─ draws sidebar/footer/popup with ratatui
  ├─ renders the main pane with alacritty_terminal
  ├─ connects to the node via local gRPC / Unix socket
  └─ supports attach/detach/reconnect
```

## 4. Key Choices

| Component | Choice | Notes |
|---|---|---|
| UI framework | ratatui 0.27 | Already in Cargo.toml |
| Events/input | crossterm 0.27 | Already present |
| Terminal emulator | alacritty_terminal | Most mature; server and client each keep an identical parser |
| PTY creation | portable-pty or nix | portable-pty preferred for cross-platform |
| Network protocol | tonic/gRPC | Already present; reuse node_session.proto |
| Web client | deferred | xterm.js + WebSocket gateway later |

## 5. Phase 0 Goals

Minimal verifiable loop:

1. `waitagent --ratatui` starts the full three-region placeholder layout
2. The node server keeps running after the client exits
3. Restarting `waitagent --ratatui` can re-attach
4. Pressing `q` exits normally and restores the terminal

## 6. Integration with Existing Code

| Existing module | New role |
|---|---|
| `src/ui/*.rs` | Convert to ratatui widgets |
| `src/terminal/engine.rs` | Optional emulator on the TUI client side; server side uses alacritty_terminal |
| `src/runtime/event_driven/*` | Reuse the event state machine; input sources switch to crossterm + PTY fd |
| `src/runtime/workspace/*` | Gradually replace tmux layout/control/main_slot_runtime |
| `src/infra/tmux_backend.rs` | Deleted eventually; kept as a fallback in intermediate phases |
| `proto/waitagent/remote/v1/node_session.proto` | Continues to serve as the protocol between server/client and node |

## 7. Exit Criteria

Once the `--ratatui` path is feature-complete and stable:
- Make `--ratatui` the default and remove the flag (running the binary directly enters the ratatui TUI)
- Do not keep a `--tmux` fallback
- Delete the tmux backend and the vendored tmux submodule
