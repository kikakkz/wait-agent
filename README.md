# WaitAgent

[![CI](https://github.com/kikakkz/wait-agent/actions/workflows/ci.yaml/badge.svg)](https://github.com/kikakkz/wait-agent/actions/workflows/ci.yaml)
[![Rust](https://img.shields.io/badge/rust-1.86.0-orange?logo=rust)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](#license)
[![tmux](https://img.shields.io/badge/tmux-vendored-1e90ff?logo=tmux)](https://github.com/tmux/tmux)

WaitAgent is a tmux-style workspace for running multiple Claude Code, Codex CLI, Kimi, shell, and other terminal sessions across local and remote machines.

- One terminal UI for many agent sessions
- Local and remote sessions in one sidebar
- Exactly one session receives input at a time
- No account, no cloud service, no terminal replacement
- Built as one Rust binary with a vendored tmux runtime

Status: early but usable. The local workflow is stable; the remote multi-machine workflow is implemented and actively hardening.

---

## Demo

GIF/video demo coming soon.

The intended 30-second flow:

1. Start `waitagent`.
2. Create two local sessions.
3. Connect a remote host.
4. Switch between `codex@local`, `kimi@remote`, and `bash@remote` from one sidebar.
5. Keep output, history, and input focus tied to the selected session.

The current UI shape:

```text
┌──────────────────────────────────────────────────────────────┬──────────────────────┐
│ Main Slot                                                    │ Sessions  [h] hide   │
│ Active local or remote PTY                                   │──────────────────────│
│                                                              │ > codex@local     I  │
│ - shell, Claude Code, Codex CLI, Kimi, or any terminal app   │ * bash@10.1.29.130   │
│ - raw output, input, and resize flow through this pane       │   claude@remote   C  │
│ - remote targets render through a session-scoped mirror      │                      │
│                                                              │ selected target info │
├──────────────────────────────────────────────────────────────┴──────────────────────┤
│ Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-O Hist · Ctrl-E Logs · Ctrl-M Menu  │
│ Listen 0.0.0.0:7474  Connect 192.168.1.20:7474                         /repo/path │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

Badges show task state: `I` input, `R` running, `C` confirm, `U` unknown.

---

## Install

One-line install for Linux x86_64 and macOS Apple Silicon:

```bash
curl -fsSL https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh | bash
```

The installer resolves the latest GitHub release tag and downloads the matching release artifact. It does not build from `main`.

Manual install is also available from the [GitHub releases page](https://github.com/kikakkz/wait-agent/releases).

Build from source:

```bash
git clone --recursive https://github.com/kikakkz/wait-agent
cd wait-agent
./scripts/install-build-deps.sh
cargo build --release
```

---

## Quick Start

Start a local workspace:

```bash
waitagent
```

Inside the workspace:

- `Ctrl-N` creates a new local session.
- `Ctrl-W` opens the remote host connector.
- `Ctrl-S` creates a new session on the selected remote endpoint.
- `Ctrl-O` opens history for the main slot.
- `Ctrl-E` opens logs.
- `Ctrl-M` opens the menu.

CLI helpers:

```bash
waitagent ls
waitagent attach <target>
waitagent detach
waitagent stop <target>
```

---

## Remote Machines

WaitAgent can aggregate sessions from remote machines into the same local sidebar.

Start a listener:

```bash
waitagent --port 7474
```

Connect a remote node to that listener:

```bash
waitagent --connect <server-ip>:7474
```

From the interactive UI, `Ctrl-W` opens **Connect Remote Host**. It can SSH into a host, install or update `waitagent`, start the remote daemon, and activate the default remote session after the connection signal arrives.

Remote bootstrap supports:

- password or key-based SSH
- optional sudo password
- saved host profiles
- multiple proxy profiles for remote install
- remote session creation with `Ctrl-S`

For WSL2, start the listener with a reachable public endpoint:

```bash
waitagent --public <reachable-wsl-or-windows-ip>:7474
```

WSL remote workflows require the remote machine to dial back into the WaitAgent listener. Use WSL mirrored networking when it is reliable in your environment, or Windows NAT plus a `netsh interface portproxy` rule when mirrored networking is unstable.

---

## Why Not Just tmux?

tmux gives you panes, windows, sessions, and a battle-tested terminal substrate. WaitAgent uses tmux instead of replacing it.

The missing layer is session management for parallel AI-assisted development:

- a catalog of local and remote agent sessions
- one stable main slot that can attach to different PTYs
- task-state badges for agent sessions
- remote session discovery and switching
- input focus discipline so keystrokes go to exactly one target

You can build parts of this manually with tmux, SSH, shell scripts, and discipline. WaitAgent packages that workflow into a single terminal UI.

---

## How It Works

WaitAgent embeds a vendored tmux and builds one persistent workspace out of real tmux panes.

- **Main Slot**: the focused session surface. It receives input and renders the active local or remote PTY.
- **Sidebar**: a fixed session catalog. `>` marks the selected row; `*` marks the active target.
- **Footer**: command/status bar with listener/connect endpoints and workspace path.
- **Session switching**: selecting a sidebar item rebinds the main slot while sidebar and footer stay mounted.
- **Remote sessions**: node-scoped gRPC carries catalog updates and PTY traffic; session-scoped routing keeps targets separate.
- **Fullscreen/history**: the main slot can zoom or enter history without replacing the workspace model.

WaitAgent is not an IDE, not an agent platform, and not an orchestration layer. It is terminal infrastructure underneath the agents you already run.

---

## Security Model

WaitAgent is currently intended for trusted machines on a trusted LAN, VPN, private network, or similarly controlled environment.

- Remote host bootstrap uses SSH.
- The remote runtime connection is gRPC-based.
- Do not expose the WaitAgent listener directly to the public Internet unless you understand and accept the trust boundary.
- Treat connected remote nodes as trusted peers.
- Saved host/proxy configuration should be treated as local machine state, not as a cloud account or managed secret store.

If you need a public-Internet deployment model, put WaitAgent behind your own network controls first.

---

## Current Status

Works today:

- Linux x86_64 release artifacts: `.tar.gz`, `.deb`, `.rpm`
- macOS Apple Silicon release artifacts: `.tar.gz`, `.dmg`
- WSL2 through the Linux build
- local tmux-backed workspace with fixed main slot, sidebar, and footer
- local session create/switch/attach/detach
- main-slot fullscreen/history view
- sidebar task-state badges for Codex, Claude, Kimi, shell, and unknown sessions
- `waitagent --port` server listener
- `waitagent --connect` remote node connection
- `Ctrl-W` SSH remote-host bootstrap
- `Ctrl-S` new session on selected remote endpoint
- remote input/output/resize path
- remote session exit synchronization

Still hardening:

- reconnect edge cases across flaky networks
- broader agent-specific TUI state detection
- Linux aarch64 release artifacts
- remote security model for untrusted networks
- automatic handling rules for session/task states

Planned:

- per-item automatic handling rules
- session switch lists for Codex, Claude, Kimi, and similar CLI agents
- WeChat and Telegram connection support

---

## Compared With Other Tools

| Tool | What it is | Where WaitAgent differs |
|---|---|---|
| tmux / Zellij | Terminal multiplexers | WaitAgent adds a local/remote session catalog, agent state badges, and a fixed main-slot workflow. |
| SSH + tmux manually | Flexible remote workflow | WaitAgent automates discovery, bootstrap, switching, and focus discipline across machines. |
| Warp | Full terminal/agentic IDE product | WaitAgent is a local terminal-native binary with no account and no cloud platform. |
| Cursor / Codex App | IDE or app-level agent surface | WaitAgent sits underneath CLI agents and keeps your terminal workflow. |

WaitAgent is deliberately narrower than an IDE. It is for developers who already use terminal agents and want to run several of them without losing track of where each one lives.

---

## Supported Platforms

| Platform | Status |
|---|---|
| Linux x86_64 | Primary target; release artifacts available |
| macOS Apple Silicon | Release artifacts available |
| Windows | Use WSL2 Linux build |
| Linux aarch64 | Source build expected; release artifact not currently published |
| Intel macOS | Source build may work; release artifact not currently published |

Source builds need Rust plus the dependencies required for the vendored tmux build. `./scripts/install-build-deps.sh` supports Debian/Ubuntu, Fedora, Arch/Manjaro, Alpine, openSUSE/SLES, and Homebrew.

---

## Remote Protocol Notes

Implemented pieces:

- protocol namespace and protobuf envelope: `waitagent.remote.v1`
- node-scoped gRPC connection: `OpenNodeSession`
- `client_hello` / `server_hello` handshake
- session-scoped routing by `session_id`
- remote target catalog publication
- remote open target / input / resize / output envelopes
- authority transport and live PTY host bridge
- session-scoped live mirror lifecycle
- reconnect with bounded replay
- create-session request routing for `Ctrl-S`
- remote exit synchronization

This section is intentionally short in the README. Deeper protocol notes live in [docs/protocol.md](docs/protocol.md).

---

## Documentation

- [Product PRD](docs/wait-agent-prd.md)
- [Architecture](docs/architecture.md)
- [Tmux-First Workspace Plan](docs/tmux-first-workspace-plan.md)
- [Tmux-First Runtime Architecture](docs/tmux-first-runtime-architecture.md)
- [Functional Design](docs/functional-design.md)
- [Remote Node Connection Architecture](docs/remote-node-connection-architecture.md)
- [Remote Network Completion Plan](docs/remote-network-completion-plan.md)
- [Remote Live Mirror Design](docs/remote-live-mirror-design.md)
- [Interaction Flows](docs/interaction-flows.md)
- [Protocol](docs/protocol.md)
- [Local Acceptance Checklist](docs/local-acceptance-checklist.md)
- [Execution Status Board](docs/execution-status-board.md)

---

## Topics

`tmux` `terminal-multiplexer` `multiplexer` `workspace-manager` `terminal` `rust` `cli` `tui` `multi-agent` `ai-agents` `multi-machine` `session-manager` `grpc`

## License

MIT
