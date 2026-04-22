# WaitAgent Tmux-First Runtime Architecture

Version: `v1.0`  
Status: `Accepted`  
Date: `2026-04-21`

## 1. Purpose

This document defines the target code architecture for the tmux-first migration.

The product direction is already accepted in [tmux-first-workspace-plan.md](tmux-first-workspace-plan.md).
This document answers a different question:

- how the codebase should be reorganized so the tmux-first direction lands on a cleaner architecture instead of becoming another layer inside the current monolith

## 2. Why A New Architecture Is Needed

The current structure is not a good long-term base for the tmux-first migration.

Visible signals:

- `src/app.rs` is over 11k lines and mixes command dispatch, workspace runtime, fullscreen logic, sidebar behavior, rendering, PTY ownership, and tests
- `src/lifecycle.rs` mixes daemon lifecycle, attach protocol, child-process management, and rendering bootstrap
- entry logic is split between `main`, `app`, and `lifecycle` in a way that is functional but not cleanly layered
- the old fullscreen path left a large amount of UI, resize, passthrough, and restore behavior coupled into one runtime file

If tmux-first work is added directly on top of that structure, the local architecture will improve at the product level but remain hard to evolve at the code level.

## 3. Architecture Goals

The new runtime architecture must be:

- more modular
- more uniform in coding style
- more explicit about ownership boundaries
- more testable in isolation
- clearer about which layer owns policy versus shell or tmux side effects
- centered around one unified program entry

## 4. Design Principles

The tmux-first architecture follows these principles:

1. One composition root
   `main` should delegate to one bootstrap path that wires the selected command mode.
2. Thin entrypoints
   CLI handlers should route to application services, not directly perform tmux, PTY, or rendering work.
3. Explicit layering
   Domain state, application orchestration, infrastructure adapters, and UI processes should be separate modules.
4. Adapter isolation
   tmux, terminal, PTY, IPC, and transport code should be treated as replaceable adapters behind narrow interfaces.
   For tmux specifically, the adapter is a vendored submodule plus a Rust glue layer, not a shell-command wrapper around a user-installed binary.
5. Strangler migration
   old code may coexist temporarily, but new tmux-first work should land in new modules and pull responsibility out of `app.rs`, not add to it.

## 5. Target Layering

The target crate structure should converge toward:

```text
src/
  main.rs
  bootstrap.rs
  cli/
  command/
  domain/
  application/
  runtime/
  infra/
  ui/
  legacy/
```

### 5.1 `bootstrap/`

Responsibilities:

- initialize config
- parse CLI input
- build the dependency graph
- dispatch to the requested command mode

Rule:

- `main.rs` should become a thin wrapper over `bootstrap::run()`

### 5.2 `cli/`

Responsibilities:

- parse flags and subcommands
- define typed command values
- keep argument parsing out of runtime orchestration

### 5.3 `domain/`

Responsibilities:

- session identity and metadata
- workspace instance identity
- layout identity
- focus targets
- state transitions that do not require direct I/O

Typical modules:

- `domain/session.rs`
- `domain/workspace.rs`
- `domain/layout.rs`
- `domain/focus.rs`

### 5.4 `application/`

Responsibilities:

- orchestrate use cases
- translate user intent into domain changes plus adapter calls
- own cross-module policy

Typical services:

- `WorkspaceService`
- `SessionService`
- `LayoutService`
- `FocusService`
- `FullscreenService`

Rule:

- application services may depend on domain abstractions and adapter traits
- they should not write raw tmux command strings inline

### 5.5 `runtime/`

Responsibilities:

- host long-lived runtime loops
- manage workspace instance lifecycle
- host daemon state if daemon remains part of the design
- coordinate background readers, event fanout, and shutdown

Typical modules:

- `runtime/workspace_runtime.rs`
- `runtime/daemon_runtime.rs`
- `runtime/attach_runtime.rs`
- `runtime/ui_runtime.rs`

### 5.6 `infra/`

Responsibilities:

- perform side effects
- implement adapter traits

Typical adapters:

- `infra/tmux.rs`
- `infra/terminal.rs`
- `infra/pty.rs`
- `infra/ipc.rs`
- `infra/transport.rs`
- `infra/fs_state.rs`

Rule:

- infrastructure code owns sockets, file descriptors, build integration, and the Rust glue boundary into vendored tmux
- business policy should not live here

Vendored tmux rule:

- tmux source lives in `third_party/tmux` as a pinned git submodule
- build-system integration is owned in the glue layer
- upper Rust layers consume typed capabilities, not tmux CLI strings
- waitagent must not require the user to preinstall tmux as a separate runtime dependency

### 5.7 `ui/`

Responsibilities:

- render sidebar pane
- render footer pane
- expose small self-contained pane UIs

Typical modules:

- `ui/sidebar.rs`
- `ui/footer.rs`
- `ui/shared.rs`

Rule:

- these are tmux pane programs or pane-oriented renderers, not overlay composers for the main pane

### 5.8 `legacy/`

Responsibilities:

- temporarily contain the old custom local runtime while migration is in progress

Rule:

- no new feature work should be added here
- only compatibility, read-only reference, or deletion work should touch it

## 6. Unified Entry Model

The program should converge toward one command bootstrap path:

```text
main
  -> bootstrap::run
    -> cli::parse
      -> command dispatch
        -> application service
          -> runtime or infra adapter
```

Representative command families:

- `workspace`
- `attach`
- `daemon`
- `status`
- `detach`
- `ui-sidebar`
- `ui-footer`

This is better than the current split where command ownership is spread across `app.rs` and `lifecycle.rs` with different runtime styles.

## 7. Coding Rules For The Migration

Effective immediately:

- new tmux-first logic should not be added to `src/app.rs` unless the change is strictly bridging or deletion-related
- new runtime boundaries should be introduced in new files first
- old modules should lose responsibility over time instead of gaining new tmux-specific branches
- command handling should move toward one style and naming convention
- new modules should prefer explicit service types over giant free-function clusters or giant `impl App`
- no Rust source file may exceed 1000 lines; once a file approaches that limit, split it into narrower modules before adding more logic
- vendored tmux must compile in the default `cargo build` path; opt-in environment-variable gates are not an acceptable steady-state build model
- `build.rs` should stay thin and delegate vendored tmux build orchestration to tmux glue modules rather than embedding that logic inline

## 8. Suggested First Concrete Module Split

Before large feature migration, establish these files:

- `src/bootstrap.rs`
- `src/infra/tmux.rs`
- `src/domain/workspace.rs`
- `src/application/workspace_service.rs`
- `src/runtime/workspace_runtime.rs`
- `src/ui/sidebar.rs`
- `src/ui/footer.rs`

These are the minimum pieces needed to stop the tmux migration from collapsing back into the old monolith.

## 9. Migration Sequence

The code migration should happen in this order:

1. create the new architecture skeleton and module boundaries
2. add vendored tmux backend adapter and build integration inside the new `infra` layer
3. move workspace entry and lifecycle wiring onto the new bootstrap and runtime path
4. move sidebar and footer onto dedicated UI modules
5. move fullscreen and focus logic onto tmux-native services
6. delete or isolate the old custom fullscreen runtime inside `legacy`

## 10. Acceptance Criteria

The architecture migration is successful only if:

- `src/app.rs` stops being the default landing zone for new local-runtime behavior
- new tmux-first work enters through unified bootstrap and explicit runtime or application modules
- at least the tmux backend, workspace runtime, and pane UI entrypoints live outside the old monolithic files
- the resulting entry structure is simpler to explain than the current `main -> app + lifecycle + legacy fullscreen` split
- the module split is reflected in tasks before implementation continues
