# WaitAgent

WaitAgent is a terminal-native interaction scheduler for multi-agent workflows.

It does not try to replace agents, IDEs, or orchestration platforms. It focuses on a narrower problem:

> Let multiple AI agent sessions share one terminal, instead of forcing the user to switch between many terminals.

## Current Positioning

The core goals of WaitAgent are:

- Run multiple independent agent sessions behind a single terminal experience
- Expose only one active session for interaction at a time within each attached console
- Detect sessions that are likely waiting for user input
- Allow at most one automatic switch after the user submits input
- Preserve raw TTY behavior without semantic parsing or agent-specific behavior changes

## Deployment Modes

### Local Mode

Multiple sessions run on the same machine and are aggregated by a local WaitAgent instance.

### Network Mode

The user only needs to configure an access point for a WaitAgent instance:

- Local sessions become visible on the server side automatically
- The local CLI remains fully interactive
- The server side can also interact with those sessions
- Terminal results and state changes are synchronized automatically across both sides

## Core Experience

- Single focus: each attached console interacts with exactly one session at a time
- Automatic but controlled: an input submission creates at most one auto-switch opportunity
- Continuous interaction protection: do not switch away while the current session is still in the same interaction flow
- Peek: inspect a background session in read-only mode without taking over input
- Minimal UI: no multi-panel dashboard, no card layout, no summary-first interface

## Current State

This repository currently contains product documentation only. Implementation has not started yet.

Current documents:

- [Product PRD](docs/wait-agent-prd.md)
- [Architecture](docs/architecture.md)
- [Functional Design](docs/functional-design.md)
- [Module Design](docs/module-design.md)
- [UI Design](docs/ui-design.md)
- [Interaction Flows](docs/interaction-flows.md)
- [Protocol](docs/protocol.md)
- [MVP Plan](docs/mvp-plan.md)
- [Execution Status Board](docs/execution-status-board.md)

## Recommended Next Step

- Start implementation from the local-first plan in [docs/mvp-plan.md](docs/mvp-plan.md)

## Why This Exists

Existing tools solve adjacent but different problems:

- `tmux / Zellij` solve terminal multiplexing
- `Claude Code / Codex CLI` solve single-agent CLI execution
- `Codex App / Cursor / Warp` solve vendor-owned multi-agent management

What is still missing is a terminal-native, vendor-neutral, low-intrusion interaction layer for human-in-the-loop multi-agent CLI workflows.
