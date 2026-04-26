# WaitAgent Tmux-Native Realignment Review

Version: `v1.0`
Status: `Accepted`
Date: `2026-04-22`

## 1. Purpose

This review re-evaluates the implemented and planned tmux migration work under the accepted rule:

- waitagent should be a branded tmux-like tool
- except for waitagent-owned sidebar, footer/menu, and management affordances, implementation should maximize reuse of tmux native behavior and targets

## 2. Review Result

### 2.1 Retain As Foundation

These slices remain aligned and should be preserved:

- `task.tmux-0`
  architecture skeleton, unified bootstrap entry, and non-monolithic module layout remain valid
- `task.tmux-r1`
  rewrite boundary reset remains valid
- `task.tmux-r2`
  vendored tmux build integration and typed tmux adapter remain core foundations
- `task.sidebar-1`
  the sidebar product contract remains useful as a UI requirement, independent of the old composed display implementation

### 2.2 Transitional Work That Must Be Rewritten Or Removed

These implemented slices are no longer acceptable as the target local runtime architecture:

- `task.lifecycle-1` through `task.lifecycle-5`
  they intentionally built a waitagent-owned daemon, socket, attach, and PTY envelope
- `task.sidebar-2` through `task.sidebar-4`
  they implemented sidebar behavior inside the old waitagent-composed display surface rather than tmux-owned panes
- daemon-centered parts of `task.tmux-r3`
  they extracted the custom lifecycle stack cleanly, but the extracted stack is still centered on a waitagent daemon protocol rather than tmux-native runtime identity
- the first partial cut of `task.tmux-r4`
  it cut command entrypoints over to the new modules, but those modules still route through a custom daemon/socket/attach/status stack

### 2.3 Unstarted Work That Remains Valid After Narrowing

These future slices are still structurally correct after the product clarification, but must be interpreted narrowly:

- `task.tmux-r5`
  build waitagent sidebar and footer as tmux-owned panes
- `task.tmux-r6`
  fullscreen, scrollback, and copy-mode should use tmux native behavior
- `task.tmux-r7`
  session management affordances should layer on top of tmux-native switching
- `task.tmux-r8`
  remove transitional custom runtime paths and leave one clear local runtime model

## 3. Code-Level Review

### 3.1 Foundations To Keep

- [src/infra/tmux.rs](/opt/data/workspace/wait-agent/src/infra/tmux.rs)
  retain; this is the real typed tmux control foundation
- [src/infra/tmux_glue.rs](/opt/data/workspace/wait-agent/src/infra/tmux_glue.rs)
  retain; this is the vendored build and runtime glue foundation
- [src/domain/workspace.rs](/opt/data/workspace/wait-agent/src/domain/workspace.rs)
  retain; workspace-derived identity remains useful as a bootstrap alias
- [src/application/workspace_service.rs](/opt/data/workspace/wait-agent/src/application/workspace_service.rs)
  retain; workspace bootstrap through typed tmux capabilities remains valid
- [src/runtime/workspace_entry_runtime.rs](/opt/data/workspace/wait-agent/src/runtime/workspace_entry_runtime.rs)
  retain in narrowed form; this is close to the desired bootstrap alias boundary
- [src/bootstrap.rs](/opt/data/workspace/wait-agent/src/bootstrap.rs)
  retain; unified entry remains valid
- [src/command/dispatch.rs](/opt/data/workspace/wait-agent/src/command/dispatch.rs)
  retain, but continue simplifying until commands directly express branded tmux semantics

### 3.2 Transitional Modules To Demote And Remove

These modules represent a parallel waitagent-owned multiplexer layer and are not the accepted endpoint:

- [src/runtime/workspace_daemon_runtime.rs](/opt/data/workspace/wait-agent/src/runtime/workspace_daemon_runtime.rs)
- [src/runtime/workspace_attach_runtime.rs](/opt/data/workspace/wait-agent/src/runtime/workspace_attach_runtime.rs)
- [src/runtime/workspace_daemon_client_runtime.rs](/opt/data/workspace/wait-agent/src/runtime/workspace_daemon_client_runtime.rs)
- [src/runtime/workspace_daemon_protocol.rs](/opt/data/workspace/wait-agent/src/runtime/workspace_daemon_protocol.rs)
- [src/application/workspace_daemon_service.rs](/opt/data/workspace/wait-agent/src/application/workspace_daemon_service.rs)
- [src/domain/workspace_paths.rs](/opt/data/workspace/wait-agent/src/domain/workspace_paths.rs)
- [src/domain/workspace_status.rs](/opt/data/workspace/wait-agent/src/domain/workspace_status.rs)
- [src/runtime/workspace_readiness.rs](/opt/data/workspace/wait-agent/src/runtime/workspace_readiness.rs)
- daemon-driven control parts of [src/runtime/workspace_command_runtime.rs](/opt/data/workspace/wait-agent/src/runtime/workspace_command_runtime.rs)

These may remain temporarily while the cutover is in flight, but they should be treated as transitional compatibility code rather than the intended architecture.

## 4. Required Architectural Adjustment

The primary local runtime model should now be:

```text
waitagent command surface
  -> tmux-backed bootstrap alias
  -> tmux-native target identity
  -> waitagent-owned sidebar/footer/menu panes where required
```

That means:

- `workspace` stays as convenience entry
- `attach`, `ls`, and `detach` should converge on tmux-native semantics
- fullscreen, scrollback, copy-mode, and switching should reuse tmux directly
- waitagent-owned runtime logic should shrink, not expand
- sidebar and footer should be modeled as persistent tmux panes rather than optional overlays
- future network-backed sessions should fit the same shared session-registry model consumed by sidebar, footer, and management menus

## 5. Task Reclassification

Accepted reclassification:

- keep `task.tmux-0`, `task.tmux-r1`, `task.tmux-r2`, and `task.sidebar-1`
- mark `task.lifecycle-1` through `task.lifecycle-5` as superseded
- mark `task.sidebar-2` through `task.sidebar-4` as superseded
- keep `task.tmux-r3` as historical done work, but treat its daemon-centered outputs as transitional
- continue `task.tmux-r4`, but redefine its real objective as removing the transitional daemon-centered command path in favor of tmux-native command semantics

## 6. Immediate Execution Rule

Until the tmux-native cutover is complete:

- do not add new user-facing features to the transitional daemon/socket stack
- only touch that stack to support deletion, demotion, or bounded compatibility while tmux-native replacements land
- new local-runtime feature work should target tmux-native behavior first

## 7. Refined Execution Order

The execution queue should now be interpreted as:

1. `task.tmux-r4`
   replace daemon-centered workspace commands with tmux-native target semantics while keeping `workspace` as a bootstrap alias
2. `task.tmux-r5`
   move waitagent-specific sidebar and footer UI into tmux-owned panes and stable tmux layouts
3. `task.tmux-r6`
   align fullscreen, scrollback, copy-mode, and detach or control keybindings with tmux-native behavior
4. `task.tmux-r7`
   layer waitagent-specific management affordances on top of tmux-native session and pane switching
5. `task.tmux-r8`
   remove transitional daemon and composed-display code so one clean local runtime path remains

Refinement rule:

- each task should reduce waitagent-owned runtime surface area
- no future task should introduce new user-facing behavior through the transitional daemon/socket model unless it is strictly temporary deletion support
