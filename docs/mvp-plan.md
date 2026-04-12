# WaitAgent MVP Plan

Version: `v1.1`  
Status: `Active`  
Date: `2026-04-12`

## 1. Purpose

This document explains the human delivery strategy for the WaitAgent MVP.

It is no longer the place for exact machine execution ordering.
Detailed task sequencing now lives in `.agents/tasks/` and related runbooks.

## 2. MVP Strategy

The MVP remains split into two stages:

- `Stage A`: Local single-machine MVP
- `Stage B`: Network extension MVP

The core strategic rule is unchanged:

> Prove the local workspace interaction model first. Resume network work only after that model is trusted in real use.

## 3. Stage A: Local Single-Machine MVP

Goal:

- make one `waitagent` entrypoint usable as the default local workspace

Included scope:

- local workspace bootstrap
- shell-backed session hosting
- console focus management
- waiting detection and one-enter one-switch scheduling
- Peek
- terminal-native rendering
- reusable local session context

Explicitly excluded:

- remote transport as a requirement for local usability
- authentication hardening
- reconnect recovery
- mirrored multi-console interaction

Human exit criteria:

- one local `waitagent` process can manage multiple sessions in one terminal
- the local shell and agent workflows still feel natural
- the local acceptance checklist passes in real use

## 4. Stage B: Network Extension MVP

Goal:

- extend the accepted local workspace so remote sessions can appear on a server-side console without changing the local-first model

Included scope:

- transport protocol implementation
- server and client runtime baselines
- node registration and remote session publication
- aggregate server-side session visibility
- remote input and resize routing
- mirrored output and server-side console interaction

Explicitly excluded:

- full reconnect identity recovery
- full authentication hardening
- rich diagnostics and operational tooling beyond MVP usability

Human entry gate:

- Stage A must already be accepted
- local scheduler behavior must feel stable
- local renderer behavior must feel stable
- no unresolved local UX issue should be likely to contaminate network debugging

## 5. Main Risks

Primary risk:

- building distributed abstractions too early and destabilizing the accepted local interaction model

Secondary risk:

- incorrect switching behavior that is technically functional but not trustworthy in daily use

Mitigation:

- keep PTY authority local to the PTY-owning node
- keep local mode runnable without remote prerequisites
- validate terminal and scheduler behavior against real workflows, not only unit tests

## 6. Immediate Human Focus

The immediate focus is still:

- close `T4-10` through final local acceptance sign-off

After that:

- resume `T5-06` as the first bounded post-acceptance network slice

For exact execution order, use `.agents/tasks/current.yaml`, `.agents/tasks/backlog.yaml`, and `.agents/runbooks/`.
