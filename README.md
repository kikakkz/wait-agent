<p align="center">
  <img src="docs/logo.svg" alt="WaitAgent" width="320">
</p>

# WaitAgent

[![CI](https://github.com/kikakkz/wait-agent/actions/workflows/ci.yaml/badge.svg)](https://github.com/kikakkz/wait-agent/actions/workflows/ci.yaml)
[![Rust](https://img.shields.io/badge/rust-1.86.0-orange?logo=rust)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](#license)
[![tmux](https://img.shields.io/badge/tmux-vendored-1e90ff?logo=tmux)](https://github.com/tmux/tmux)

> **A tmux-like multi-machine, multi-agent session manager — focused on developer project parallelism.**

WaitAgent is a terminal-native workspace manager that lets you run multiple AI agent sessions across machines from a single terminal interface. It does not replace your terminal, your agents, or your workflow — it gives you one place to manage them all.

---

## What WaitAgent Is

WaitAgent is a **tmux-first session manager** built for the reality of parallel AI-assisted development:

- **One workspace, many sessions** — create and switch between multiple agent sessions (Claude Code, Codex CLI, etc.) inside a single terminal workspace
- **Multi-machine aggregation** — connect remote machines over gRPC and interact with their sessions through the same unified catalog
- **Single-focus interaction** — exactly one session is visible and receives input at a time, so input never goes to the wrong place
- **Terminal-native, no login** — runs as a local binary with vendored tmux; no account, no cloud service, no registration required

WaitAgent is **not** an IDE, not an agent platform, and not an orchestration layer. It is the terminal multiplexing and session management layer that sits underneath your agents.

---

## WaitAgent vs Warp

| | WaitAgent | Warp |
|---|---|---|
| **Paradigm** | tmux-like session manager | Complete agentic IDE |
| **Surface** | Terminal-native (vendored tmux) | Custom GPU-accelerated terminal + web app |
| **Account** | None — local binary only | Login and registration required |
| **Focus** | Multi-machine multi-agent session parallelism | Full-stack agentic development environment |
| **Architecture** | One binary, vendored tmux, gRPC for remote | Proprietary terminal + cloud-backed agent platform |
| **Extensibility** | Vendor-neutral — works with any CLI agent | Warp-native agent ecosystem |
| **Target user** | Developers already using Claude Code / Codex CLI who need to parallelize | Developers looking for an all-in-one agentic IDE |

Warp is a complete development environment: it replaces your terminal, provides its own agent, and ties into a cloud platform. WaitAgent solves a narrower problem: when you are already running multiple agent sessions across machines, how do you manage them all from one place without changing your tools or signing up for a service.

---

## How It Works

WaitAgent embeds a vendored tmux and builds one persistent workspace out of real tmux panes. The chrome stays mounted while only the main slot is rebound to the selected target.

```text
┌──────────────────────────────────────────────────────────────┬──────────────────────┐
│ Main Slot                                                    │ Sessions  [h] hide   │
│ Active local or remote PTY                                  │──────────────────────│
│                                                              │ > codex@local     🔊I │
│ - shell, Claude Code, Codex CLI, or any terminal app         │ * bash@10.1.29.130  │
│ - raw output, input, and resize flow through this pane       │   claude@remote   📢C│
│ - remote targets render through a session-scoped mirror      │                      │
│                                                              │ selected target info │
├──────────────────────────────────────────────────────────────┴──────────────────────┤
│ Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-O Hist · Ctrl-E Logs · Ctrl-M Menu  │
│ Listen 0.0.0.0:7474  Connect 192.168.1.20:7474                         /repo/path │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

- **Main Slot** — the focused session surface. It receives input and renders the active local or remote PTY.
- **Sidebar** — a fixed session catalog. `>` marks the selected row, `*` marks the active target, and badges show task state (`I` input, `R` running, `C` confirm, `U` unknown).
- **Footer** — fixed command/status bar. It shows WaitAgent actions, listener/connect endpoints, and the current path.
- **Session switching** — selecting a sidebar or footer item rebinds the main slot; sidebar and footer remain in place.
- **Fullscreen/history** — the main slot can be zoomed or switched into history without replacing the workspace chrome model.

---

## Deployment Modes

### Local Mode

One machine, one `waitagent` workspace, multiple managed sessions:

```bash
waitagent
```

Creates a tmux-backed workspace. Sessions run as PTY-backed shell environments where you can launch Claude Code, Codex CLI, or any terminal workflow.

### Multi-Machine Mode

Connect remote machines over gRPC so their sessions appear in your local catalog:

**Server (listener):**

```bash
waitagent --port 7474
```

**Remote node (connects to server):**

```bash
waitagent --connect <server-ip>:7474
```

Remote sessions appear in the sidebar alongside local sessions. Input flows through the server control plane to the PTY-owning node; output synchronizes back to attached consoles over a node-scoped gRPC connection with session-scoped routing.

From an interactive workspace, `Ctrl-W` opens **Connect Remote Host**. It can SSH into a host, install or update `waitagent`, start the remote daemon, and activate the default remote session after the connection signal arrives. `Ctrl-S` creates an additional session on the selected connected remote endpoint.

Remote host bootstrap supports password or key authentication, optional sudo password, saved host profiles, and an install proxy configuration. **Proxy Configuration** in the Connect Remote Host popup exports `all_proxy`, `https_proxy`, uppercase variants, and `no_proxy` for the remote install command so GitHub release/API fetches inherit the proxy.

---

## Current Support Status

### Systems

GitHub Markdown does not provide real tabs in README files, so the system matrix is grouped with collapsible sections.

<details open>
<summary><strong>Linux x86_64</strong></summary>

| Item | Status |
|---|---|
| Prebuilt release | Supported: `.tar.gz`, `.deb`, `.rpm` |
| One-line installer | Supported |
| Source build | Supported |
| Build dependency helper | Debian/Ubuntu, Fedora, Arch/Manjaro, Alpine, openSUSE/SLES |
| Primary development target | Yes |

</details>

<details>
<summary><strong>macOS Apple Silicon</strong></summary>

| Item | Status |
|---|---|
| Prebuilt release | Supported: `.tar.gz`, `.dmg` |
| One-line installer | Supported |
| Source build | Supported with Homebrew dependencies |
| Intel Mac release artifact | Not published |

</details>

<details>
<summary><strong>Windows via WSL2</strong></summary>

| Item | Status |
|---|---|
| Native Windows binary | Not supported |
| WSL2 Linux build | Supported |
| Recommended networking | Mirrored networking or NAT with Windows portproxy |
| Remote host bootstrap | Supported when the remote host can reach the WSL listener endpoint |

WSL configuration is documented in [WSL2 Setup](#wsl2-setup).

</details>

<details>
<summary><strong>Other Linux / source builds</strong></summary>

| Item | Status |
|---|---|
| Linux aarch64 prebuilt artifact | Not currently published |
| Linux aarch64 source build | Expected to build if Rust and tmux dependencies are available; not a release target |
| Unsupported distributions | Install `bison/yacc`, `pkg-config`, libevent headers, ncurses headers, C compiler, make, automake, and autoconf manually |

</details>

### Runtime Features

| Feature | Status |
|---|---|
| Local tmux-backed workspace with fixed main slot, sidebar, and footer | Stable |
| Local session create/switch/attach/detach | Stable |
| Dedicated content pane per session | Stable |
| Main-slot fullscreen/history view | Stable |
| Sidebar task-state badges and manual attention cues | Stable; no auto-focus switching |
| Codex, Claude, and Kimi task-state integration | Implemented through explicit agent signals where available |
| Footer menu and keyboard actions (`Ctrl-N`, `Ctrl-W`, `Ctrl-S`, `Ctrl-O`, `Ctrl-E`, `Ctrl-M`) | Implemented |
| `waitagent --port` server listener | Implemented |
| `waitagent --connect` remote node connection | Implemented |
| Remote session catalog over node-scoped gRPC | Implemented |
| Remote main-slot open/input/output/resize path | Implemented |
| `Ctrl-W` SSH remote-host bootstrap | Implemented and user-validated for default-session activation |
| Remote install proxy configuration | Implemented |
| `Ctrl-S` new session on selected remote endpoint | Implemented |
| Reconnect and bounded replay | Implemented baseline; edge-case hardening continues |
| Remote session exit synchronization | Implemented |
| Remote Codex/Kimi/complex TUI parity | Implemented for current local and remote validation paths; hardening continues for agent-specific edge cases |

### Planned Features

| Feature | Status |
|---|---|
| Per-item automatic handling rules | Planned |
| Session switch lists for Codex, Claude, Kimi, and similar CLI agents | Planned |
| WeChat and Telegram connection support | Planned |

## Remote Protocol Status

| Area | Status |
|---|---|
| Protocol namespace and protobuf envelope (`waitagent.remote.v1`) | Implemented |
| Node-scoped gRPC connection (`OpenNodeSession`) | Implemented |
| `client_hello` / `server_hello` handshake | Implemented |
| Session-scoped routing by `session_id` | Implemented |
| Remote target catalog publication | Implemented; runtime-owner path is current, older file-backed catalog sources are being retired |
| Remote open target / input / resize / output envelopes | Implemented |
| Authority transport and live PTY host bridge | Implemented |
| Session-scoped live mirror lifecycle | Implemented enough for current remote main-slot use; hardening continues |
| Reconnect with bounded replay | Implemented baseline |
| Create-session request routing for `Ctrl-S` | Implemented through the local owner/control path |
| Remote exit synchronization | Implemented |
| Cross-host visible parity validation | Implemented for current release paths; ongoing for new agent integrations |

---

## Quick Start

**One-line install (Linux x86_64 / macOS Apple Silicon):**

```bash
curl -fsSL https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh | bash
```

**Build from source:**

```bash
git clone --recursive https://github.com/kikakkz/wait-agent
cd wait-agent
./scripts/install-build-deps.sh
cargo build --release
```

### WSL2 Setup

WaitAgent works in WSL2 through the Linux build. For remote host workflows, WSL networking must allow the remote machine to dial back into the WaitAgent listener shown in the footer. Windows WSL can use either mirrored networking or the default NAT mode with a Windows portproxy rule. Mirrored networking is simpler, but Windows WSL mirrored networking has known stability bugs; use NAT plus portproxy when mirrored mode is unreliable.

1. Update WSL from Windows PowerShell:

```powershell
wsl --update
wsl --version
```

2. Choose a WSL networking mode.

Option A: mirrored networking. This is the simplest configuration when it works, but Windows WSL mirrored networking has known stability bugs.

```ini
[wsl2]
networkingMode=mirrored
dnsTunneling=true
firewall=true
autoProxy=true
```

Restart WSL after changing `.wslconfig`:

```powershell
wsl --shutdown
```

Then reopen your Linux distribution.

Option B: default WSL NAT with Windows portproxy. Leave `networkingMode` unset or remove the mirrored `.wslconfig` entries, restart WSL, then run the following commands from an elevated Windows PowerShell. The `connectaddress` must be the current WSL NAT IP; it can be read from inside WSL with `hostname -I` or from Windows through `wsl.exe`.

```powershell
$WslIp = ((wsl.exe hostname -I) -split "\s+" |
    Where-Object { $_ -match "^\d+\.\d+\.\d+\.\d+$" } |
    Select-Object -First 1)

netsh interface portproxy delete v4tov4 `
    listenaddress=0.0.0.0 `
    listenport=7474

netsh interface portproxy add v4tov4 `
    listenaddress=0.0.0.0 `
    listenport=7474 `
    connectaddress=$WslIp `
    connectport=7474

New-NetFirewallRule `
    -DisplayName "WSL-WaitAgent-7474" `
    -Direction Inbound `
    -Protocol TCP `
    -LocalPort 7474 `
    -Action Allow
```

WSL NAT IPs can change after WSL restarts. Re-run the portproxy commands when the WSL IP changes. The firewall rule only needs to exist once for the selected local port; if it already exists, keep the existing rule or remove it before creating a replacement.

3. Install WaitAgent inside WSL:

```bash
curl -fsSL https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh | bash
```

4. Start a public listener workspace inside WSL:

```bash
waitagent --public <reachable-wsl-or-windows-ip>:7474
```

Use the listener/connect endpoint shown in the WaitAgent footer when connecting remote hosts. In WSL, the listener must be started with `--public` so remote machines receive an address they can dial back. If the remote machine cannot connect back, check Windows Defender Firewall and allow inbound TCP for the selected port, for example `7474`; in NAT mode, also confirm the Windows portproxy still points at the current WSL IP.

If your WSL or Windows environment uses a corporate proxy, keep `autoProxy=true` and configure the WaitAgent remote install proxy from **Ctrl-W -> Proxy Configuration** when the remote host also needs that proxy to reach GitHub releases.

---

## Usage

```bash
# Start a workspace
waitagent

# List sessions
waitagent ls

# Attach to an existing session
waitagent attach <target>

# Detach
waitagent detach
```

Inside the workspace, create sessions, launch agents, and switch between them — all from one terminal.

---

## Why This Exists

Existing tools each solve part of the problem:

- **tmux / Zellij** — terminal multiplexing infrastructure, not interaction scheduling
- **Claude Code / Codex CLI** — single-agent CLI execution, not multi-session management
- **Warp / Cursor / Codex App** — vendor-owned agentic IDEs requiring accounts and cloud services

WaitAgent targets the missing layer:

> A terminal-native, vendor-neutral session manager that lets you run multiple agents across machines from one place — no account, no IDE, no platform lock-in.

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

*Add these topics on the [repo settings page](https://github.com/kikakkz/wait-agent/settings) → "Topics" for better discoverability on GitHub.*
