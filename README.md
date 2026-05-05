<p align="center">
  <img src="docs/logo.svg" alt="WaitAgent" width="120">
</p>

# WaitAgent

[![CI](https://github.com/kikakkz/wait-agent/actions/workflows/ci.yaml/badge.svg)](https://github.com/kikakkz/wait-agent/actions/workflows/ci.yaml)
[![Rust](https://img.shields.io/badge/rust-1.86.0-orange?logo=rust)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](#license)
[![tmux](https://img.shields.io/badge/tmux-vendored-1e90ff?logo=tmux)](https://github.com/tmux/tmux)

> **terminal · multiplexer · workspace · agent**

WaitAgent is a terminal-native interaction scheduler for multi-agent workflows.

It does not try to replace agents, IDEs, or orchestration platforms. It focuses on a narrower problem:

> Let multiple AI agent sessions share one terminal, instead of forcing the user to switch between many terminals.

The target UX is workspace-first:

- On a single machine, the user starts one `waitagent`
- Inside that WaitAgent workspace, the user creates and manages multiple background sessions
- Remote session aggregation is under active development: nodes can connect, discover sessions, and interact through a unified catalog

## Current Positioning

The core goals of WaitAgent are:

- Provide one workspace shell entrypoint per machine
- Run multiple independent agent sessions behind a single terminal experience
- Expose only one active session for interaction at a time within each attached console
- Detect sessions that are likely waiting for user input
- Preserve raw TTY behavior without semantic parsing or agent-specific behavior changes

## Deployment Mode

### Local Mode

The user starts one `waitagent` workspace on the machine and creates multiple managed sessions inside it.

## Core Experience

- Single focus: each attached console interacts with exactly one session at a time
- Session switching stays explicit in the current local product
- Minimal UI: no multi-panel dashboard, no card layout, no summary-first interface

## Current State

This repository contains product documentation and an active Rust implementation.

Current implementation status:

- **Local workspace** is stable: one `waitagent` creates and manages multiple shell-backed sessions inside a tmux-native workspace with fixed sidebar, main slot, and footer
- **Terminal fidelity** hardened for Codex-like TUIs: application cursor keys, managed viewport sizing, UTF-8, cursor visibility, wide-character rendering
- **Remote networking** is the active phase: gRPC-based node session protocol, authority transport with mTLS-style handshake, session-scoped routing, reconnect and replay, publication ownership
- **Current gate**: explicit session-scoped live-mirror control (`task.t5-08c4d3b`) so opened remote sessions show the client's real screen instead of placeholder state

Current documents:

- [Product PRD](docs/wait-agent-prd.md)
- [Architecture](docs/architecture.md)
- [Tmux-First Workspace Plan](docs/tmux-first-workspace-plan.md)
- [Tmux-First Runtime Architecture](docs/tmux-first-runtime-architecture.md)
- [Functional Design](docs/functional-design.md)
- [Module Design](docs/module-design.md)
- [UI Design](docs/ui-design.md)
- [Interaction Flows](docs/interaction-flows.md)
- [Protocol](docs/protocol.md)
- [MVP Plan](docs/mvp-plan.md)
- [Local Acceptance Checklist](docs/local-acceptance-checklist.md)
- [Execution Status Board](docs/execution-status-board.md)
- [Remote Node Connection Architecture](docs/remote-node-connection-architecture.md)
- [Remote Network Completion Plan](docs/remote-network-completion-plan.md)
- [Remote Live Mirror Design](docs/remote-live-mirror-design.md)

## Build Prerequisites

WaitAgent now compiles vendored tmux as part of the default `cargo build` path.

To install the required system packages, run:

```bash
./scripts/install-build-deps.sh
```

To preview the detected package-manager command without executing it, run:

```bash
./scripts/install-build-deps.sh --print
```

### Download & Install

Pre-built packages are available from the [GitHub Releases](https://github.com/kikakkz/wait-agent/releases) page.

#### Linux

| Format | Architecture | Command |
|--------|-------------|---------|
| `.deb` | x86_64 | `sudo dpkg -i waitagent_<version>_amd64.deb` |
| `.rpm` | x86_64 | `sudo rpm -i waitagent-<version>-1.x86_64.rpm` |
| `.tar.gz` | x86_64 | `tar xzf waitagent-<version>-x86_64-linux.tar.gz` |

After installing via `.deb` or `.rpm`, the `waitagent` binary is available system-wide.

#### macOS

| Format | Architecture | Command |
|--------|-------------|---------|
| `.tar.gz` | x86_64 / aarch64 | `tar xzf waitagent-<version>-<arch>-macos.tar.gz` |
| `.dmg` | x86_64 / aarch64 | Open the `.dmg` and drag **WaitAgent.app** to Applications |

The `.dmg` contains a bundled `.app` with the CLI binary inside (`WaitAgent.app/Contents/MacOS/waitagent`). You can symlink it:

```bash
ln -s /Applications/WaitAgent.app/Contents/MacOS/waitagent /usr/local/bin/waitagent
```

### Build from Source

Build prerequisites are listed above. To build from source instead of using a pre-built package:

```bash
git clone --recursive https://github.com/kikakkz/wait-agent
cd wait-agent
./scripts/install-build-deps.sh
cargo build --release
```

The binary is written to `target/release/waitagent`.

## Single-Machine Usage

```bash
# Start a workspace (creates a tmux-backed workspace on this machine)
waitagent

# List available sessions
waitagent ls

# Attach to a session
waitagent attach <target>

# Detach from current session
waitagent detach
```

## Multi-Machine Usage

WaitAgent supports remote session aggregation across machines through a gRPC-based node protocol. One machine runs as the server (listener), the other connects as a remote node.

### On the server machine (listener)

```bash
# Start waitagent with the public port enabled
waitagent --port 7474
```

This starts the workspace and opens a listener on `0.0.0.0:7474`. Remote nodes can connect, discover the server's sessions, and interact through the shared catalog.

### On the remote machine (connecting node)

```bash
# Connect to the server and attach
waitagent --connect <server-ip>:7474 attach <target>
```

Remote sessions are surfaced in the same unified session catalog as local sessions. The remote node sends input and receives output over an authenticated transport with session-scoped routing, reconnect support, and replay on reconnect.

### Current remote protocol status

The remote networking layer is under active development. The current implementation covers:

| Feature | Status |
|---|---|
| gRPC node session protocol (`waitagent.remote.v1.NodeSessionService`) | ✅ Implemented |
| `--port` / `--connect` CLI contract | ✅ Implemented |
| Session-scoped routing and authority transport | ✅ Implemented |
| Reconnect with bounded replay | ✅ Implemented |
| Publication ownership and target discovery | ✅ Implemented |
| Remote terminal bootstrap and replay | ✅ Implemented |
| Live-mirror open/close protocol | ✅ Implemented |
| PTY-owner mirror lifecycle | ✅ Basic (needs hardening) |
| Cross-host visible parity validation | ✅ Basic (manual validation passed, needs hardening) |

## Recommended Next Step

- Close the current phase-2 gate: implement explicit session-scoped mirror open/close protocol and server-side per-session mirror-route ownership on the public `--port` + `--connect` path

## Why This Exists

Existing tools solve adjacent but different problems:

- `tmux / Zellij` solve terminal multiplexing
- `Claude Code / Codex CLI` solve single-agent CLI execution
- `Codex App / Cursor / Warp` solve vendor-owned multi-agent management

What is still missing is a terminal-native, vendor-neutral, low-intrusion interaction layer for human-in-the-loop multi-agent CLI workflows.

---

## Topics

`tmux` `terminal-multiplexer` `multiplexer` `workspace-manager` `terminal` `rust` `cli` `tui` `multi-agent` `ai-agents`

*Add these topics on the [repo settings page](https://github.com/kikakkz/wait-agent/settings) → "Topics" for better discoverability on GitHub.*
