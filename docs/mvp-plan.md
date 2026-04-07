# WaitAgent MVP Plan

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

## 1. Purpose

This document converts the execution board into a concrete near-term build plan.

It is intentionally narrower than the full board.

Primary rule:

> Deliver a usable local MVP first. Add network capabilities only after the local interaction model is proven.

It complements:

- [execution-status-board.md](execution-status-board.md)
- [architecture.md](architecture.md)
- [protocol.md](protocol.md)

## 2. MVP Strategy

The MVP is split into two implementation stages:

- `Stage A`: Local single-machine MVP
- `Stage B`: Network extension MVP

Stage B must not begin until Stage A is usable end to end.

## 3. Stage A: Local Single-Machine MVP

## 3.1 Goal

Prove that WaitAgent can solve the core interaction problem locally:

- Multiple sessions
- Single-focus console
- Waiting heuristic
- One-enter one-switch scheduling
- Peek
- Minimal terminal-native UI

## 3.2 Included Scope

- CLI bootstrap
- Session registry
- PTY manager
- Terminal raw mode and resize
- Console runtime
- Typing-state protection
- Waiting heuristic
- Waiting queue
- Auto-switch scheduler
- Peek
- Focus renderer and minimal status lines

## 3.3 Explicitly Excluded from Stage A

- Remote transport
- Server runtime
- Client runtime
- Authentication
- Reconnect logic
- Multi-console mirrored interaction

## 3.4 Stage A Task Set

Use this exact order:

1. `T1-01` Initialize implementation workspace and crate structure
2. `T1-02` Implement base config loading and app bootstrap
3. `T1-03` Implement session registry core types
4. `T1-04` Implement PTY manager spawn and ownership model
5. `T1-06` Implement terminal raw mode and resize capture
6. `T2-01` Implement console runtime state
7. `T2-03` Implement typing-state protection
8. `T2-04` Implement waiting heuristic engine
9. `T2-05` Implement waiting queue management
10. `T2-02` Implement manual focus switching
11. `T2-06` Implement auto-switch state machine
12. `T2-07` Implement continuation protection
13. `T2-08` Implement Peek mode
14. `T3-01` Integrate VT screen state engine
15. `T3-02` Implement session screen snapshot storage
16. `T3-03` Implement focused session renderer
17. `T3-04` Implement minimal top and bottom status lines
18. `T3-05` Implement focus restore on switch
19. `T3-06` Implement Peek rendering path
20. `T4-01` Build end-to-end local attach command
21. `T4-02` Add scheduler unit tests
22. `T4-03` Add PTY integration tests
23. `T4-04` Add renderer snapshot tests
24. `T4-05` Validate three-session local workflow manually
25. `T4-06` Fix local MVP defects and stabilize

## 3.5 Stage A Exit Criteria

Stage A is complete only if all of the following are true:

- A user can run at least three local sessions
- One console can switch among them
- Partial input blocks switching
- One `Enter` creates at most one automatic switch
- Peek works without changing input ownership
- Focus restoration works visually
- The local flow passes unit, integration, and manual validation

## 4. Stage B: Network Extension MVP

## 4.1 Goal

Extend the local MVP so sessions from remote nodes can appear on a server-side console while preserving local CLI interaction.

## 4.2 Included Scope

- Protocol implementation subset
- Server runtime skeleton
- Client runtime skeleton
- Node registration
- Remote session publication
- Server-side aggregate session registry
- Remote input and resize routing
- Mirrored output broadcast
- Mirrored input propagation
- Server-side console attach

## 4.3 Explicitly Excluded from Stage B

- Full reconnect identity recovery
- Full authentication hardening
- Rich audit and diagnostics surfaces
- Performance optimization beyond basic usability

## 4.4 Stage B Entry Gate

Do not start Stage B until:

- Stage A exit criteria are met
- Local scheduler behavior is stable
- Local renderer behavior is stable
- Core session and PTY ownership model has not been changing for at least one iteration

## 4.5 Stage B Task Set

Use this order:

1. `T0-07` Define wire protocol document
2. `T5-01` Define protocol schema and versioning
3. `T5-02` Implement server runtime skeleton
4. `T5-03` Implement client runtime skeleton
5. `T5-04` Implement node registration and liveness
6. `T5-05` Implement remote session publication
7. `T5-06` Implement aggregate server session registry
8. `T5-07` Implement remote resize and input routing
9. `T6-01` Implement server-side console attach
10. `T6-02` Implement mirrored output broadcast
11. `T6-03` Implement mirrored input propagation
12. `T6-04` Implement server-side waiting queue
13. `T6-05` Implement multi-console attach awareness UI
14. `T6-06` Validate mirrored local/server workflow end to end

## 4.6 Stage B Exit Criteria

Stage B is complete only if:

- A client node can connect to a server
- Local sessions appear on the server
- The same session can be attached locally and from the server
- Input from either side reaches the same PTY
- Output from that PTY is mirrored to all attached consoles
- Server-side focus and waiting queue behavior work for remote sessions

## 5. Deferred Work

These items are intentionally deferred until after the MVP stages:

- `T7-01` Reconnect and session identity recovery
- `T7-02` Offline handling hardening
- `T7-03` Authentication and enrollment hardening
- `T7-04` Structured diagnostics
- `T7-05` Debug status views
- `T7-06` Replay and reconnect test expansion

## 6. Build Rules

The implementation must obey these rules:

- No real network dependency for Stage A
- No semantic parsing
- No multi-panel UI
- No remote-first abstractions that block local usability
- PTY ownership remains local to the PTY host

## 7. Risk Controls

## 7.1 Main Risk

The biggest execution risk is building distributed abstractions too early.

Mitigation:

- Finish local console, local scheduler, and local renderer first
- Treat remote transport as an extension layer
- Keep session authority local to the PTY owner

## 7.2 Scheduler Risk

The second major risk is incorrect switching behavior.

Mitigation:

- Write scheduler tests before remote transport work
- Lock down the one-enter one-switch rule locally first
- Validate continuation protection locally first

## 8. Recommended Immediate Next Tasks

If implementation starts now, begin with:

1. `T1-01` Initialize implementation workspace and crate structure
2. `T1-03` Implement session registry core types
3. `T1-04` Implement PTY manager spawn and ownership model
4. `T1-06` Implement terminal raw mode and resize capture

These are the minimum foundational tasks that unlock the rest of Stage A.

