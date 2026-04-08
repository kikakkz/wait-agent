# WaitAgent

WaitAgent is a terminal-native interaction scheduler for multi-agent workflows.

It does not try to replace agents, IDEs, or orchestration platforms. It focuses on a narrower problem:

> Let multiple AI agent sessions share one terminal, instead of forcing the user to switch between many terminals.

The target UX is workspace-first:

- On a single machine, the user starts one `waitagent`
- Inside that WaitAgent workspace, the user creates and manages multiple background sessions
- In multi-machine mode, those same local workspaces connect to one `waitagent server`
- Local and server-side interaction stay synchronized and follow the same single-focus model

## Current Positioning

The core goals of WaitAgent are:

- Provide one workspace shell entrypoint per machine
- Run multiple independent agent sessions behind a single terminal experience
- Expose only one active session for interaction at a time within each attached console
- Detect sessions that are likely waiting for user input
- Allow at most one automatic switch after the user submits input
- Preserve raw TTY behavior without semantic parsing or agent-specific behavior changes
- Keep local mode and network mode behaviorally identical from the user’s perspective

## Deployment Modes

### Local Mode

The user starts one `waitagent` workspace on the machine and creates multiple managed sessions inside it.

### Network Mode

The user configures one access point for the same `waitagent` workspace:

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

This repository contains product documentation and an active Rust implementation.

Current implementation status:

- Local workspace-first interaction is now the primary local UX: one `waitagent` can create and manage multiple shell-backed sessions inside the same terminal
- Local PTY runtime, scheduler, Peek, renderer, and validation coverage exist
- Terminal fidelity has been hardened for Codex-like TUIs, including terminal capability replies, application cursor keys, managed viewport sizing, UTF-8 handling, cursor visibility, and wide-character rendering
- Network transport, server runtime, client runtime, node registration, and remote session publication baselines exist
- The remaining local MVP work is acceptance and stabilization, with auto-switch behavior still requiring more real-workflow validation
- Mirrored multi-console network UX is still the next major product milestone after the local workspace path is accepted

Current documents:

- [Product PRD](docs/wait-agent-prd.md)
- [Architecture](docs/architecture.md)
- [Functional Design](docs/functional-design.md)
- [Module Design](docs/module-design.md)
- [UI Design](docs/ui-design.md)
- [Interaction Flows](docs/interaction-flows.md)
- [Protocol](docs/protocol.md)
- [MVP Plan](docs/mvp-plan.md)
- [Local Acceptance Checklist](docs/local-acceptance-checklist.md)
- [Execution Status Board](docs/execution-status-board.md)

## Recommended Next Step

- Finish local workspace acceptance and refine auto-switch behavior in real agent workflows, then resume network aggregation work on top of that entry model

## Why This Exists

Existing tools solve adjacent but different problems:

- `tmux / Zellij` solve terminal multiplexing
- `Claude Code / Codex CLI` solve single-agent CLI execution
- `Codex App / Cursor / Warp` solve vendor-owned multi-agent management

What is still missing is a terminal-native, vendor-neutral, low-intrusion interaction layer for human-in-the-loop multi-agent CLI workflows.
