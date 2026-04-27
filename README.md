# WaitAgent

WaitAgent is a terminal-native interaction scheduler for multi-agent workflows.

It does not try to replace agents, IDEs, or orchestration platforms. It focuses on a narrower problem:

> Let multiple AI agent sessions share one terminal, instead of forcing the user to switch between many terminals.

The target UX is workspace-first:

- On a single machine, the user starts one `waitagent`
- Inside that WaitAgent workspace, the user creates and manages multiple background sessions
- Remote session aggregation is a future product area and is not part of the current command surface

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

- Local workspace-first interaction is now the primary local UX: one `waitagent` can create and manage multiple shell-backed sessions inside the same terminal
- Terminal fidelity has been hardened for Codex-like TUIs, including terminal capability replies, application cursor keys, managed viewport sizing, UTF-8 handling, cursor visibility, and wide-character rendering
- The current local phase is centered on stabilization and cleanup of the tmux-native workspace path
- Remote session connection and management will be redesigned on top of the tmux-native local architecture rather than carried forward from the deleted legacy runtime

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

## Recommended Next Step

- Continue refining the local tmux-native workspace path, then design remote session connection and management on top of that unified architecture

## Why This Exists

Existing tools solve adjacent but different problems:

- `tmux / Zellij` solve terminal multiplexing
- `Claude Code / Codex CLI` solve single-agent CLI execution
- `Codex App / Cursor / Warp` solve vendor-owned multi-agent management

What is still missing is a terminal-native, vendor-neutral, low-intrusion interaction layer for human-in-the-loop multi-agent CLI workflows.
