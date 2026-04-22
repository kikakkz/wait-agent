# WaitAgent Tmux-First Workspace Plan

Version: `v1.1`  
Status: `Accepted`  
Date: `2026-04-22`

## 1. Purpose

This document records the accepted replacement for the old custom `native fullscreen` and `live surface` direction.

The code-architecture counterpart to this plan is documented in [tmux-first-runtime-architecture.md](tmux-first-runtime-architecture.md).

It exists to answer four questions clearly:

- What architecture is now accepted for local multi-session workspace UX
- Why the old fullscreen path is no longer the baseline
- Which product behaviors must remain true
- How implementation work is split into bounded delivery slices

## 2. Decision Summary

WaitAgent will move to a `tmux-first` local workspace architecture.

The accepted model is:

- the main interactive session runs in a real tmux pane
- the right sidebar is a dedicated tmux pane
- the bottom menu/footer is a dedicated tmux pane
- one user-visible session maps to one tmux window
- fullscreen is implemented as tmux pane zoom, not as local replay or repaint
- fullscreen history and scrolling use tmux-native pane history and copy-mode behavior
- tmux is vendored into the repository as a pinned submodule and exposed upward through a Rust glue layer rather than through runtime shell commands or a system tmux dependency

This intentionally replaces the old direction where WaitAgent tried to keep the focused session in a reduced PTY and then promote that live session into a custom fullscreen path.

## 3. Root Cause For The Architecture Change

The old architecture is no longer accepted because its core rendering assumption is invalid for Codex-like live TUI workloads.

The rejected path worked like this:

- normal dashboard mode ran the focused session in a PTY reduced by sidebar and footer layout
- fullscreen tried to resize that already-running session to full terminal size
- WaitAgent then relied on local transcript or snapshot-based recovery to make the fullscreen view appear correct

That design is structurally unstable because live TUIs do not guarantee a full repaint after resize.

In practice this created recurring failures:

- right-side blank regions after fullscreen entry
- stale narrow-layout content after widening
- redraw gaps during live output
- sidebar contamination or main-pane truncation when trying to compensate
- input, focus, and restore bugs caused by mixed ownership of one display surface

The problem is therefore not a single resize bug. It is the result of making WaitAgent behave like a terminal multiplexer without using terminal-multiplexer ownership boundaries.

## 4. Accepted Product Constraints

The new architecture must preserve all of the following:

- sidebar remains a first-class, always-visible concept in normal workspace mode
- bottom menu/footer remains visible in normal workspace mode
- fullscreen still exists
- fullscreen scrollback is supported
- session switching remains fast and persistent
- Codex and shell sessions keep native terminal behavior
- the design may support multiple daemon instances, similar to tmux multi-server or multi-socket usage

WaitAgent is no longer constrained to a single daemon instance if multiple isolated workspace instances are simpler and safer.

## 5. Accepted Runtime Topology

The default local topology is:

```text
waitagent workspace instance
  -> vendored tmux runtime
    -> one tmux session per workspace instance
      -> one tmux window per managed session
        -> main pane   = real shell / codex / agent process
        -> sidebar pane = waitagent sidebar UI process
        -> footer pane  = waitagent footer/menu UI process
```

Layout in normal mode:

```text
+---------------------------+-----------+
|                           |           |
|         main pane         |  sidebar  |
|                           |           |
+---------------------------------------+
|              footer/menu pane         |
+---------------------------------------+
```

Layout in fullscreen mode:

- zoom the `main pane`
- keep session/window identity unchanged
- return from zoom back to the same three-pane layout

## 6. Interaction Model

The interaction contract is intentionally close to tmux.

Normal mode:

- session switching selects another tmux window
- moving focus right selects the sidebar pane
- moving focus left returns to the main pane
- entering the sidebar and pressing `Enter` switches to the selected session window and returns focus to the main pane

Fullscreen mode:

- `Ctrl-O` toggles zoom on the main pane
- fullscreen scrollback uses tmux pane history
- WaitAgent may wrap tmux copy-mode behind simpler keys, but the underlying mechanism remains tmux-native

The critical rule is that WaitAgent no longer tries to co-own the same framebuffer as the live session.

## 7. Process Model

### 7.1 Workspace Instance

Each workspace instance owns:

- one workspace key
- one tmux runtime namespace
- one tmux session
- the mapping between WaitAgent session identity and tmux window identity

### 7.2 Managed Session

Each managed session owns:

- one tmux window
- one real main pane PTY
- stable metadata in `SessionRegistry`
- optional sidebar/footer projections derived from registry state

### 7.3 Sidebar And Footer

Sidebar and footer are no longer overlays on the main pane.

They are independent UI processes running in their own tmux panes and consuming WaitAgent state through a narrow interface.

This can be implemented through:

- direct local IPC to a workspace daemon
- local status files
- lightweight `waitagent ui-*` subcommands that rerender from current daemon state

The exact Rust glue interface can evolve later. The pane ownership model and vendored-backend rule are the accepted parts.

## 8. What Is Explicitly Rejected

The following are no longer accepted as the primary architecture:

- custom native fullscreen based on replaying a previously narrow PTY into a widened terminal
- local overlay composition where sidebar/footer are painted over or beside live session output on the same screen buffer
- fullscreen correctness that depends on probing the child app for redraw side effects
- dashboard-local scroll reconstruction as the main solution for fullscreen history

Historical code and task records may remain for migration purposes, but they are not the target baseline.

## 9. Migration Rules

The migration must follow these rules:

- do not mix the old display pipeline and new tmux display pipeline on the same live path
- do not keep sidebar/footer as overlay rendering once tmux panes exist for that workspace path
- do not keep fullscreen dependent on transcript replay once tmux zoom owns fullscreen
- do not depend on a user-installed tmux binary or shell-command orchestration as the steady-state backend
- remove or formally retire obsolete display tasks as the tmux path becomes authoritative
- defer network expansion until the local tmux-first display base is stable enough not to multiply later debugging cost

## 10. Implementation Slices

The accepted implementation queue after the rewrite review is:

1. `task.tmux-0`
   Establish the new modular runtime architecture, unified entry model, and migration skeleton.
2. `task.tmux-r1`
   Re-baseline the migration around a clean rewrite boundary, freezing the invalid bridge path and clarifying what existing tmux work is retained.
3. `task.tmux-r2`
   Implement the real vendored tmux control adapter behind the new runtime interfaces.
4. `task.tmux-r3`
   Build a new workspace lifecycle stack outside the legacy lifecycle module.
5. `task.tmux-r4`
   Route workspace-facing commands through the new tmux-first lifecycle stack.
6. `task.tmux-r5`
   Implement persistent sidebar and footer panes as tmux-owned UI surfaces.
7. `task.tmux-r6`
   Implement tmux-native fullscreen zoom and fullscreen-only scrollback.
8. `task.tmux-r7`
   Move session switching and sidebar focus semantics onto tmux-native control.
9. `task.tmux-r8`
   Remove the obsolete custom local display path and complete shell-plus-codex acceptance.

## 11. Acceptance Criteria

The tmux-first migration is accepted only if all of the following become true:

- Codex runs in a real tmux main pane rather than a custom replayed surface
- normal mode keeps sidebar and footer visible without overwriting main session output
- fullscreen no longer leaves a fake blank sidebar region
- fullscreen history is complete and scrollable through tmux pane history
- switching between main pane and sidebar is responsive and deterministic
- switching sessions never restarts the target process or loses its history
- the old custom fullscreen path is either deleted or explicitly marked as compatibility-only and inactive for the default workspace route

## 12. Status Of Prior Work

The previous custom fullscreen tasks are retained only as historical implementation record.

They are no longer the active accepted architecture baseline.

That means:

- old `display-*` fullscreen slices may be marked `superseded` even if code landed during that period
- network tasks that assumed the old local display baseline should be deferred until the tmux-first path is stable
- sidebar tasks remain product-relevant, but their implementation home moves from custom screen composition to tmux panes
- the original `task.tmux-1` through `task.tmux-6` queue is also retained only as historical planning after the rewrite review found that the bridge-heavy route would preserve too much of the legacy lifecycle structure
