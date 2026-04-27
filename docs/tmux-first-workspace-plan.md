# WaitAgent Tmux-First Workspace Plan

Version: `v1.3`  
Status: `Accepted`  
Date: `2026-04-24`

## 1. Purpose

This document records the accepted replacement for the old custom `native fullscreen` and `live surface` direction.

The code-architecture counterpart to this plan is documented in [tmux-first-runtime-architecture.md](tmux-first-runtime-architecture.md).

It exists to answer four questions clearly:

- what architecture is now accepted for local multi-session workspace UX
- why the old fullscreen path is no longer the baseline
- which product behaviors must remain true
- how implementation work is split into bounded delivery slices

## 2. Decision Summary

WaitAgent will use a `tmux-first` local workspace architecture with one persistent chrome surface.

The accepted model is:

- the main interactive view is a fixed `main slot` inside a persistent workspace chrome
- the right sidebar is a dedicated tmux pane
- the bottom menu or footer is a dedicated tmux pane
- sidebar or footer selection rebinds only the `main slot` target
- in-workspace switching must not detach the client, reveal the shell, or relaunch WaitAgent
- fullscreen is implemented as tmux pane zoom, not as local replay or repaint
- fullscreen keeps waitagent menu visibility by projecting footer chrome onto the tmux status line while the footer pane is hidden by zoom
- fullscreen history and scrolling use tmux-native pane history and copy-mode behavior
- tmux is vendored into the repository as a pinned submodule and exposed upward through a Rust glue layer rather than through runtime shell commands or a system tmux dependency

This intentionally replaces both:

- the old direction where WaitAgent tried to keep the focused session in a reduced PTY and then promote that live session into a custom fullscreen path
- the later intermediate direction where in-workspace switching detached the client and launched `waitagent attach <target>`

## 3. Root Cause For The Architecture Change

The old fullscreen architecture is no longer accepted because its core rendering assumption is invalid for Codex-like live TUI workloads.

The detached switching architecture is no longer accepted because it cannot satisfy the workspace contract even if chrome refresh is event-driven.

The rejected switching path worked like this:

- sidebar or footer interaction selected another target
- WaitAgent detached the current client
- WaitAgent launched `waitagent attach <target>`
- the terminal briefly returned to the shell before the new attach path re-entered WaitAgent

That design is structurally wrong for the accepted product because:

- sidebar and footer do not stay fixed
- the shell becomes visible during target changes
- flicker is a direct consequence of leaving and rebuilding the client
- future local and remote targets cannot share one stable presentation slot under that model

The problem is therefore not a redraw bug or a missing debounce. It is an ownership problem.

## 4. Accepted Product Constraints

The new architecture must preserve all of the following:

- sidebar remains a first-class, always-visible concept in normal workspace mode
- bottom menu or footer remains visible in normal workspace mode
- only the main view changes when switching targets in normal mode
- fullscreen still exists
- fullscreen scrollback is supported
- target switching remains fast and persistent
- Codex and shell sessions keep native terminal behavior
- the design may support multiple backend instances, similar to tmux multi-server or multi-socket usage

WaitAgent is no longer constrained to a single backend instance if multiple isolated workspace instances are simpler and safer.

## 5. Accepted Runtime Topology

The default local topology is:

```text
waitagent workspace instance
  -> vendored tmux runtime
    -> one persistent workspace chrome
      -> sidebar pane
      -> main slot pane
      -> footer or menu pane
    -> target host surfaces in the same backend
      -> local target A
      -> local target B
      -> local target N
      -> future remote bridge target M
```

Layout in normal mode:

```text
+---------------------------+-----------+
|                           |           |
|         main slot         |  sidebar  |
|                           |           |
+---------------------------------------+
|             footer or menu            |
+---------------------------------------+
```

Layout in fullscreen mode:

- zoom the `main slot`
- keep the workspace and target identity unchanged
- let tmux hide the sidebar and footer panes as part of native zoom
- mirror the waitagent footer or menu line into the tmux status line so fullscreen retains menu visibility without reopening local overlay rendering
- return from zoom back to the same three-pane layout

## 6. Interaction Model

The interaction contract is intentionally close to tmux, but target activation is stricter than raw client reattach.

Normal mode:

- `Right` is a waitagent global chrome-navigation key that selects the sidebar pane from the main slot
- `Left` is a waitagent global chrome-navigation key that returns from the sidebar pane to the main slot
- `h` hides the sidebar when the sidebar pane is focused and returns focus to the main slot
- pressing `Enter` on a sidebar item activates that target inside the existing workspace and returns focus to the main slot
- the footer pane is the primary non-prefixed waitagent session-management surface
- waitagent chrome navigation decisions are based on tmux pane identity and layout only; they must not depend on shell-prompt detection, pane-text probing, `capture-pane`, or content-aware heuristics

Fullscreen mode:

- `Ctrl-O` toggles zoom on the main slot
- the tmux status line becomes the fullscreen owner of the waitagent menu line
- fullscreen-safe waitagent session actions use prefixed tmux bindings such as `Ctrl-B s`
- fullscreen scrollback uses tmux pane history
- WaitAgent may wrap tmux copy-mode behind simpler keys, but the underlying mechanism remains tmux-native

The critical rule is that WaitAgent no longer tries to co-own the same framebuffer as the live session, and it no longer treats `attach` as the normal in-workspace switching primitive.
If the same fullscreen artifact can be reproduced with the same pane layout in raw tmux, treat it as tmux-native behavior rather than a waitagent layout bug.
If a TUI session was started in a pane narrowed by sidebar or footer chrome, zoom may reveal unused columns on the right until that application chooses to redraw the current view; this is accepted product behavior for the tmux-first path.

Rejected interaction approaches:

- do not implement conditional sidebar navigation that probes the current pane contents and only steals `Right` when a shell prompt appears to be visible
- do not embed nested shell probes inside tmux key bindings in order to guess whether waitagent chrome navigation should activate
- do not implement in-workspace target switching by detaching the client and launching `waitagent attach <target>`

## 7. Process Model

### 7.1 Workspace Instance

Each workspace instance owns:

- one workspace key
- one tmux runtime namespace
- one persistent visible chrome layout
- the mapping between transport-agnostic target identity and the concrete host surface bound into the main slot

### 7.2 Target

Each target owns:

- one transport-agnostic identity in the shared catalog
- one concrete realization, either:
  - a local tmux-hosted surface in the current backend, or
  - a future remote bridge renderer bound into the same main slot
- stable metadata in the shared registry

The accepted local implementation may use parked panes, parked windows, `swap-pane`, `join-pane`, `move-pane`, or equivalent tmux-native rebinding primitives as long as the visible chrome remains fixed.

### 7.3 Sidebar And Footer

Sidebar and footer are not overlays on the main slot.

They are independent UI processes running in their own tmux panes and consuming WaitAgent state through a narrow interface.

This can be implemented through:

- direct local IPC to a workspace controller
- local status files
- lightweight `waitagent ui-*` subcommands that rerender from current state

The exact Rust glue interface can evolve later. The fixed-chrome ownership model and vendored-backend rule are the accepted parts.

## 8. What Is Explicitly Rejected

The following are no longer accepted as the primary architecture:

- custom native fullscreen based on replaying a previously narrow PTY into a widened terminal
- local overlay composition where sidebar or footer are painted over or beside live session output on the same screen buffer
- fullscreen correctness that depends on probing the child app for redraw side effects
- dashboard-local scroll reconstruction as the main solution for fullscreen history
- in-workspace target switching that detaches the client and launches `waitagent attach <target>`

Historical code and task records may remain for migration purposes, but they are not the target baseline.

## 9. Migration Rules

The migration must follow these rules:

- do not mix the old display pipeline and new tmux display pipeline on the same live path
- do not keep sidebar or footer as overlay rendering once tmux panes exist for that workspace path
- do not keep fullscreen dependent on transcript replay once tmux zoom owns fullscreen
- do not depend on a user-installed tmux binary or shell-command orchestration as the steady-state backend
- do not treat `waitagent attach` as the hidden target-switching primitive inside an already-running workspace
- remove or formally retire obsolete display tasks as the tmux path becomes authoritative
- defer network expansion until the local tmux-first display base is stable enough not to multiply later debugging cost

## 10. Implementation Slices

The accepted event-driven delivery queue is:

1. `task.event-r1`
   Establish the new event-driven local runtime architecture and event contract.
2. `task.event-r2`
   Make tmux chrome and session-catalog projection event-driven.
3. `task.event-r2a`
   Replace cross-session attach switching with fixed-chrome main-slot target activation.
4. `task.event-r3`
   Move attach, resize, and scheduler behavior onto explicit runtime events.
5. `task.event-r4`
   Route the default local path through the new event-driven stack and isolate polling history.

Priority rule:

- `task.event-r2a -> task.event-r3 -> task.event-r4` is the locked local-priority batch for the current phase
- resumed network or remote work stays deferred until that batch is complete or explicitly re-planned

Earlier `tmux-*` slices remain valid historical implementation record, but they are not the current planning surface.

Current event-r4 refinement:

- the default local route is now explicitly anchored on `bootstrap::run -> CommandDispatcher -> WorkspaceCommandRuntime`
- hidden pane ownership is explicitly anchored on `EventDrivenPaneRuntime`
- stale references to deleted files such as `src/app.rs` or non-existent placeholders such as `event_driven_runtime.rs` should be retired wherever they still appear in architecture or agent-control docs

## 11. Acceptance Criteria

The tmux-first migration is accepted only if all of the following become true:

- Codex runs in a real tmux-hosted main slot rather than a custom replayed surface
- normal mode keeps sidebar and footer visible without overwriting main session output
- switching targets keeps the current workspace chrome mounted and changes only the main view
- switching targets never reveals the shell or detaches the current client
- fullscreen no longer leaves waitagent-owned fake sidebar chrome or custom replay artifacts over the main pane; a narrow-start TUI may still preserve unused columns after zoom until the application redraws
- fullscreen history is complete and scrollable through tmux pane history
- switching between main slot and sidebar is responsive and deterministic
- switching targets never restarts the target process or loses its history
- the old custom fullscreen path is either deleted or explicitly marked as compatibility-only and inactive for the default workspace route

## 12. Status Of Prior Work

The previous custom fullscreen tasks are retained only as historical implementation record.

They are no longer the active accepted architecture baseline.

That means:

- old `display-*` fullscreen slices may be marked `superseded` even if code landed during that period
- network tasks that assumed the old local display baseline should be deferred until the tmux-first path is stable
- sidebar tasks remain product-relevant, but their implementation home moves from custom screen composition to tmux panes and fixed-chrome main-slot activation
- the original `task.tmux-1` through `task.tmux-6` queue is also retained only as historical planning after the rewrite review found that the bridge-heavy route would preserve too much of the legacy lifecycle structure
