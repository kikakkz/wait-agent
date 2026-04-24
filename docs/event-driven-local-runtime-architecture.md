# WaitAgent Event-Driven Local Runtime Architecture

Version: `v1.0`  
Status: `Accepted`  
Date: `2026-04-23`

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

The new local runtime is split into these event-owning modules:

1. `TmuxHookBridge`
   Converts tmux hooks and pane-geometry changes into typed runtime events.
2. `WorkspaceController`
   Owns command-side effects, layout changes, focus changes, and session-switch commits.
3. `SessionCatalogProjector`
   Produces authoritative session-catalog updates from tmux and workspace state.
4. `SidebarPaneRuntime`
   Renders sidebar chrome and emits explicit selection intent.
5. `FooterPaneRuntime`
   Renders footer or status-line chrome and emits explicit menu intent.
6. `AttachClientRuntime`
   Translates client stdin, daemon output, attach lifecycle, and terminal resize into runtime events.
7. `SchedulerRuntime`
   Produces focus and autoswitch decisions only when upstream runtime state changes.

## 4. Accepted Event Classes

The new event contract is represented in code by:

- [local_runtime.rs](/opt/data/workspace/wait-agent/src/domain/local_runtime.rs)
- [local_runtime_event_service.rs](/opt/data/workspace/wait-agent/src/application/local_runtime_event_service.rs)
- [event_driven_runtime.rs](/opt/data/workspace/wait-agent/src/runtime/event_driven_runtime.rs)

The accepted top-level event classes are:

1. `TmuxHook`
   Source: `TmuxHookBridge`
   Examples: `client-attached`, `client-detached`, `client-resized`, `client-session-changed`
   Consumers: `WorkspaceController`, `SessionCatalogProjector`
2. `SessionCatalog`
   Source: `SessionCatalogProjector`
   Examples: snapshot published, selection changed, active session changed
   Consumers: `SidebarPaneRuntime`, `FooterPaneRuntime`, `SchedulerRuntime`
3. `Chrome`
   Source: pane runtimes
   Examples: sidebar selection changed, footer render requested, status-line projection changed
   Consumers: `WorkspaceController`, sibling chrome runtime when needed
4. `Attach`
   Source: `AttachClientRuntime`
   Examples: client attached, input read, client resized, daemon output received
   Consumers: `WorkspaceController`, `SchedulerRuntime`
5. `Scheduler`
   Source: `SchedulerRuntime`
   Examples: focus changed, autoswitch requested, autoswitch committed
   Consumers: `WorkspaceController`, chrome runtimes

## 5. Explicit Producer And Consumer Contract

The accepted producer or consumer map is:

```text
tmux hooks
  -> WorkspaceController
  -> SessionCatalogProjector

session catalog updates
  -> SidebarPaneRuntime
  -> FooterPaneRuntime
  -> SchedulerRuntime

sidebar selection intent
  -> WorkspaceController
  -> FooterPaneRuntime

attach input / attach resize / daemon output
  -> WorkspaceController
  -> SchedulerRuntime

scheduler focus / autoswitch decisions
  -> WorkspaceController
  -> SidebarPaneRuntime
  -> FooterPaneRuntime
```

The critical constraint is that `SidebarPaneRuntime` and `FooterPaneRuntime` consume explicit state-change events.
They must not be responsible for rediscovering state by periodic `list_sessions()` and redraw polling.

## 6. Historical Polling Paths

The following paths are now historical and have explicit replacement owners in the new contract:

1. `src/runtime/ui_pane_runtime.rs::run_sidebar`
   Current mechanism: fixed `200ms` session refresh plus `recv_timeout`
   Replacement owner: `SessionCatalogProjector`
2. `src/runtime/ui_pane_runtime.rs::run_footer`
   Current mechanism: fixed `200ms` sleep and unconditional redraw
   Replacement owner: `FooterPaneRuntime`
3. `src/runtime/workspace_attach_runtime.rs::run`
   Current mechanism: fixed `50ms` client tick for resize and startup-refresh logic
   Replacement owner: `AttachClientRuntime`
4. `src/runtime/workspace_bootstrap_runtime.rs::wait_for_existing_daemon_ready`
   Current mechanism: sleep-and-retry readiness loop
   Replacement owner: `WorkspaceController`
5. `src/app.rs` managed console and passthrough loops
   Current mechanism: fixed `50ms` event-loop tick for scheduler and resize checks
   Replacement owner: `SchedulerRuntime`

These paths may remain while migration is incomplete, but they are not allowed to absorb new feature work.

## 7. Slice Mapping

The accepted follow-on queue is:

1. `task.event-r1`
   Define the event contract and ownership map.
2. `task.event-r2`
   Make tmux chrome and session-catalog projection event-driven.
3. `task.event-r3`
   Move attach, resize, and scheduler behavior onto explicit runtime events.
4. `task.event-r4`
   Route the default local path through the new event-driven stack and isolate polling history.

## 8. Migration Rule

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
