# WaitAgent Event-Driven Local Runtime Architecture

Version: `v1.2`  
Status: `Accepted`  
Date: `2026-04-27`

## 1. Purpose

This document defines the accepted event-driven replacement for the remaining mixed polling loops on the local runtime path.

It complements:

- [tmux-first-workspace-plan.md](tmux-first-workspace-plan.md)
- [tmux-first-runtime-architecture.md](tmux-first-runtime-architecture.md)

Those documents define the tmux-first product and layer boundaries.
This document answers a narrower question:

- which runtime modules produce events
- which runtime modules consume them
- which polling loops are now considered historical
- how the next implementation slices map onto that contract

## 2. Non-Negotiable Rule

All new local-runtime implementation work must be event-driven.

That means:

- no fixed UI refresh ticks for sidebar or footer state
- no timeout-driven scheduler recomputation
- no sleep-based attach or resize coordination on the new path
- no bootstrap readiness loops based on repeated connect attempts for the accepted path

Historical polling code may remain temporarily while the new path is landing, but it is not an acceptable foundation for new implementation slices.

## 3. Accepted Runtime Modules

The accepted local runtime is now anchored on the modules that actually own the
default route in code:

1. `bootstrap::run`
   Parses CLI input and hands every command to one dispatcher boundary.
2. `command::dispatch::CommandDispatcher`
   Routes `workspace`, `attach`, and other user-facing commands into the
   accepted runtime owners rather than historical mixed loops.
3. `runtime::WorkspaceCommandRuntime`
   Owns the default local command path for workspace bootstrap, attach,
   target activation, fullscreen, detach, and list behavior.
4. `runtime::WorkspaceEntryRuntime`
   Bootstraps or resolves the local workspace, applies the fixed chrome layout,
   and returns the accepted tmux-backed workspace handle.
5. `runtime::MainSlotRuntime`
   Owns target activation inside the persistent workspace chrome and rebinds
   the main slot without leaving the current client.
6. `runtime::WorkspaceLayoutRuntime`
   Owns layout reconciliation, chrome refresh signaling, main-pane output
   bridging, and fullscreen-aware chrome updates.
7. `runtime::EventDrivenPaneRuntime`
   Owns hidden sidebar and footer pane processes, consumes explicit refresh and
   resize signals, and emits activation intent.
8. `runtime::EventDrivenTmuxPaneRuntime`
   Projects tmux pane state and workspace session snapshots into the pane-side
   event-driven chrome runtime.
9. `runtime::EventDrivenUiPaneRuntime`
   Owns sidebar and footer render-state transitions once a pane snapshot or
   input event arrives.

The critical event-r4 rule is:

- `workspace` and `attach` must continue to enter through
  `bootstrap::run -> CommandDispatcher -> WorkspaceCommandRuntime`
- hidden pane rendering must continue to enter through
  `CommandDispatcher -> EventDrivenPaneRuntime`
- no historical runtime path is allowed to own the default local route

## 4. Accepted Event Classes

The new event contract is represented in code by:

- [local_runtime.rs](/opt/data/workspace/wait-agent/src/domain/local_runtime.rs)
- [local_runtime_event_service.rs](/opt/data/workspace/wait-agent/src/application/local_runtime_event_service.rs)
- [event_driven_pane_runtime.rs](/opt/data/workspace/wait-agent/src/runtime/event_driven_pane_runtime.rs)
- [workspace_command_runtime.rs](/opt/data/workspace/wait-agent/src/runtime/workspace_command_runtime.rs)

The accepted top-level event classes are:

1. `TmuxHook`
   Source: `TmuxHookBridge`
   Examples: `client-attached`, `client-detached`, `client-resized`, `client-session-changed`
   Consumers: `WorkspaceController`, `SessionCatalogProjector`
2. `SessionCatalog`
   Source: session-service and pane-state projection on the accepted tmux path
   Examples: snapshot published, selected target changed, active target changed
   Consumers: `SidebarPaneRuntime`, `FooterPaneRuntime`
3. `Chrome`
   Source: pane runtimes
   Examples: sidebar selection changed, footer render requested, status-line projection changed
   Consumers: `WorkspaceController`, `MainSlotRuntime`, sibling chrome runtime when needed
4. `TargetActivation`
   Source: `WorkspaceCommandRuntime`, `MainSlotRuntime`
   Examples: target activation requested, target rebound into main slot, target activation committed
   Consumers: `SidebarPaneRuntime`, `FooterPaneRuntime`
5. `Attach`
   Source: `WorkspaceCommandRuntime`
   Examples: workspace attached, target attached, current client detached
   Consumers: tmux backend and workspace layout runtime ownership

## 5. Explicit Producer And Consumer Contract

The accepted producer or consumer map is:

```text
tmux hooks
  -> WorkspaceController
  -> SessionCatalogProjector

session catalog updates
  -> SidebarPaneRuntime
  -> FooterPaneRuntime

sidebar selection intent
  -> WorkspaceCommandRuntime
  -> MainSlotRuntime
  -> FooterPaneRuntime

target activation
  -> MainSlotRuntime
  -> SidebarPaneRuntime
  -> FooterPaneRuntime

attach command / attach target resolution
  -> WorkspaceCommandRuntime
  -> SessionService

main-pane output bridge signal
  -> WorkspaceLayoutRuntime
  -> SidebarPaneRuntime
  -> FooterPaneRuntime
```

The critical constraint is that `SidebarPaneRuntime` and `FooterPaneRuntime` consume explicit state-change events.
They must not be responsible for rediscovering state by periodic `list_sessions()` and redraw polling.
The other critical constraint is that `MainSlotRuntime` keeps target activation inside the current client.
It must not translate sidebar or footer selection into `detach-client -E "waitagent attach <target>"`.

## 6. Historical Polling Paths

The following paths are now historical and have explicit replacement owners in the new contract:

1. `src/runtime/ui_pane_runtime.rs::run_sidebar`
   Current mechanism: fixed `200ms` session refresh plus `recv_timeout`
   Replacement owner: `SessionCatalogProjector`
2. `src/runtime/ui_pane_runtime.rs::run_footer`
   Current mechanism: fixed `200ms` sleep and unconditional redraw
   Replacement owner: `FooterPaneRuntime`
3. `src/runtime/workspace_attach_runtime.rs::run`
   Historical mechanism: fixed `50ms` client tick for resize and startup-refresh logic
   Replacement owner: `WorkspaceCommandRuntime` plus `WorkspaceLayoutRuntime`
4. `src/runtime/workspace_bootstrap_runtime.rs::wait_for_existing_daemon_ready`
   Historical mechanism: sleep-and-retry readiness loop
   Replacement owner: `WorkspaceEntryRuntime` plus `WorkspaceLayoutRuntime`
5. `src/app.rs` managed console and passthrough loops
   Historical mechanism: fixed `50ms` event-loop tick for scheduler and resize checks
   Replacement owner: `WorkspaceCommandRuntime` plus `EventDrivenPaneRuntime`

These paths may remain while migration is incomplete, but they are not allowed to absorb new feature work.

Additional rejected path:

6. `src/runtime/event_driven_pane_runtime.rs` sidebar switch handoff
   Current mechanism: `detach-client -E "waitagent attach <target>"`
   Replacement owner: `MainSlotRuntime`

## 7. Default Route Ownership

The accepted default local route is now:

```text
main
  -> bootstrap::run
  -> CommandDispatcher
  -> WorkspaceCommandRuntime
     -> WorkspaceEntryRuntime
     -> WorkspaceLayoutRuntime
     -> MainSlotRuntime
     -> SessionService / tmux backend
```

The accepted hidden-pane route is:

```text
tmux pane program
  -> CommandDispatcher
  -> EventDrivenPaneRuntime
  -> EventDrivenTmuxPaneRuntime
  -> EventDrivenUiPaneRuntime
```

If a local behavior does not enter through one of those two routes, it is not
on the accepted default path and must be treated as historical, inactive, or
explicitly transitional.

## 8. Slice Mapping

The accepted follow-on queue is:

1. `task.event-r1`
   Define the event contract and ownership map.
2. `task.event-r2`
   Make tmux chrome and session-catalog projection event-driven.
3. `task.event-r2a`
   Replace cross-session attach switching with fixed-chrome main-slot target activation.
4. `task.event-r3`
   Move attach, resize, and scheduler behavior onto explicit runtime events.
5. `task.event-r4`
   Route the default local path through the new event-driven stack and isolate polling history.

## 9. Migration Rule

Do not patch historical polling loops to make them slightly cleaner and call that event-driven.

Acceptable work:

- adding explicit typed events
- adding dedicated event-driven runtimes
- routing state transitions through named producers and consumers
- isolating the old polling path behind legacy boundaries

Rejected work:

- keeping `recv_timeout(...tick...)` as the primary coordinator and only renaming helpers
- keeping pane runtimes on periodic `list_sessions()` refresh and calling that “reactive”
- keeping scheduler recomputation in a timeout branch and calling that “event assisted”
- keeping in-workspace target switching on `detach-client -E "waitagent attach <target>"` and calling that “tmux-native” or “event-driven”
