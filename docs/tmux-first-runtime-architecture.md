# WaitAgent Tmux-First Runtime Architecture

Version: `v1.3`  
Status: `Accepted`  
Date: `2026-04-24`

## 1. Purpose

This document defines the target code architecture for the tmux-first migration.

The product direction is already accepted in [tmux-first-workspace-plan.md](tmux-first-workspace-plan.md).
The event-driven counterpart for the new local runtime path is documented in [event-driven-local-runtime-architecture.md](event-driven-local-runtime-architecture.md).
This document answers a different question:

- how the codebase should be reorganized so the tmux-first direction lands on a cleaner architecture instead of becoming another layer inside the current monolith

## 2. Why A New Architecture Is Needed

The historical structure that this migration replaces was not a good long-term
base for the tmux-first direction.

Visible signals:

- a retired monolithic local-runtime surface mixed command dispatch,
  workspace runtime, fullscreen logic, sidebar behavior, rendering, PTY
  ownership, and tests in one place
- legacy lifecycle code mixed daemon lifecycle, attach protocol,
  child-process management, and rendering bootstrap
- entry logic was split across multiple styles instead of one explicit
  bootstrap and dispatcher boundary
- the old fullscreen path left a large amount of UI, resize, passthrough,
  and restore behavior coupled into one runtime surface

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
   old code may coexist temporarily, but new tmux-first work should land in
   explicit runtime modules and pull responsibility out of retired monolithic
   compatibility surfaces instead of adding to them.
6. Command compatibility over code compatibility
   preserve the user-facing command surface where needed, but do not preserve old module structure or runtime plumbing when a clean replacement is clearer.
7. Trait-oriented boundaries
   prefer trait-backed adapters, explicit service objects, and runtime objects over growing monolithic function clusters.
8. Document-and-source-driven integration
   when Rust standard behavior, Cargo build behavior, or tmux behavior is uncertain, check the official docs and relevant source before freezing the abstraction.
9. Branded tmux over custom runtime
   waitagent should behave like a branded tmux distribution wherever possible.
   Except for waitagent-specific sidebar, footer/menu, and management surfaces, local runtime behavior should reuse tmux native concepts and semantics rather than rebuilding them in a custom daemon or socket protocol.
10. Tmux owns fullscreen
   fullscreen correctness is defined by tmux-native pane zoom, history, and restore behavior.
   If an artifact reproduces in raw tmux, do not reintroduce waitagent-owned redraw, replay, or fullscreen composition to hide it.
   A TUI that was started in a narrow pane may legitimately keep unused columns after zoom until it redraws itself; this is an accepted consequence of tmux-native ownership, not a waitagent bug.
11. Remote sessions stay host-owned
   future remote sessions must connect to the server over long-lived transport links, keep PTY ownership on the remote host, and expose server-side interaction through waitagent control-plane abstractions rather than by pretending the server owns a remote PTY locally.
12. Chrome navigation uses pane identity
   waitagent global chrome-navigation keys such as `Right`, `Left`, and sidebar-hide actions must be driven by tmux pane identity and layout ownership, not by probing pane contents or inferring shell prompt state.
13. Target activation keeps chrome fixed
   sidebar or footer activation must rebind the main presentation slot inside the current workspace.
   In-workspace target switching must not detach the client, reveal the shell, or launch a fresh `waitagent attach <target>` path.

## 4.1 Native Reuse Rules

The intended product is not a new terminal multiplexer that merely borrows tmux pieces.
It is a waitagent-branded tmux-like tool with a narrow amount of product-specific chrome.

Effective interpretation:

- keep the executable and product surface branded as `waitagent`
- preserve convenience entry by workspace directory where that improves UX
- after entry, runtime identity should be tmux-native
  `server/socket -> session -> window -> pane`
- `workspace` is a bootstrap alias, not the primary long-lived runtime identity
- `attach`, `ls`, `detach`, fullscreen, scrollback, copy-mode, pane switching, and related control flow should default to tmux-native behavior
- `waitagent` or `workspace` may create or bootstrap a backend when no attach target is specified; `attach` is only for joining an existing backend
- do not invent a dedicated waitagent-only `status` command unless tmux-native semantics prove insufficient and the extra command is justified as a waitagent-specific management surface
- waitagent-owned code should focus on:
  - sidebar pane
  - footer/menu pane
  - target discovery and management affordances layered on top of tmux
  - product-specific defaults and branding

Anti-goal:

- do not keep building a parallel waitagent-owned daemon protocol for features that tmux already provides natively
- do not rebuild a waitagent-owned fullscreen redraw or replay path to compensate for tmux-native resize behavior
- do not add app-specific redraw hints or view-reset tricks for Codex-like sessions in order to hide accepted narrow-start zoom behavior
- do not let the local tmux adapter become the only session model; future remote sessions will use server-routed interaction rather than a local tmux pane backed by a server-owned PTY
- do not use pane-text inspection, shell-prompt heuristics, or nested shell probes inside tmux key bindings to decide whether sidebar or menu navigation should activate
- do not implement in-workspace target switching by detaching the client and launching `waitagent attach <target>`

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

- target identity and metadata
- workspace instance identity
- layout identity
- focus targets
- state transitions that do not require direct I/O

Typical modules:

- `domain/session.rs`
- `domain/workspace.rs`
- `domain/layout.rs`
- `domain/focus.rs`
- `domain/session_registry.rs`

Rule:

- target identity used by sidebar, footer, menus, and future network-session management should be transport-agnostic
- local tmux-backed targets and future remote targets should project into one shared domain model rather than separate ad hoc side registries
- target identity should converge on `transport + authority/node id + session id`, not on local tmux socket semantics alone
- the domain model should distinguish `workspace chrome`, `main slot`, and `target realization` so switching policy does not collapse back into attach semantics

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
- `SessionRegistryService`

Rule:

- application services may depend on domain abstractions and adapter traits
- they should not write raw tmux command strings inline
- target activation should flow through transport-agnostic capabilities so a future server-side `interact` runtime can route input, resize, focus, and attach to either a local tmux host or a remote connected node
- sidebar or footer selection must resolve to `activate target in main slot`, not to `detach current client and run attach`

### 5.5 `runtime/`

Responsibilities:

- host the minimum waitagent-owned runtime needed around tmux-native control
- manage workspace bootstrap and waitagent-specific pane programs
- coordinate startup and product-specific surfaces that tmux itself does not provide
- own layout restore rules for persistent sidebar and footer panes plus main-slot zoom restore
- own the stable main-slot binding or rebinding path used by sidebar and footer activation

Representative modules on the accepted current path are:

- `runtime/workspace_command_runtime.rs`
- `runtime/workspace_entry_runtime.rs`
- `runtime/workspace_layout_runtime.rs`
- `runtime/main_slot_runtime.rs`
- `runtime/event_driven_pane_runtime.rs`
- `runtime/event_driven_tmux_pane_runtime.rs`
- `runtime/event_driven_ui_pane_runtime.rs`

Compatibility rule for future remote sessions:

- local session runtime may stay tmux-native
- remote session runtime on the server will not be a server-owned PTY pane; it will be a waitagent `interact` surface that talks to the server control plane
- runtime boundaries should therefore separate `session presentation` from `session host ownership`

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
- prefer direct typed tmux capabilities over custom compatibility protocols when tmux already exposes the needed control surface
- future network-session transport adapters should enter here behind the same session-registry abstractions consumed by application and UI layers
- the vendored tmux adapter is a local-session host adapter only; it must not become the implicit authority for future remote-session control or metadata inference

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
- `ls`
- `detach`
- `ui-sidebar`
- `ui-footer`

Command model rule:

- `workspace` is the friendly entry alias that maps the current directory to a tmux target
- `attach`, `ls`, and `detach` should converge toward tmux-native target semantics rather than waitagent-specific daemon identity
- `attach` is an explicit client-entry command for an existing backend; it is not the hidden mechanism for sidebar or footer target switching inside an already-running workspace
- waitagent-specific management commands should only exist where tmux has no native equivalent and where the command is part of sidebar, footer, or multi-session management product value

## 6.1 Persistent Layout Rule

The waitagent local UI should be designed around one persistent tmux layout:

- a persistent main slot for shell, agent, or remote interaction
- a persistent right sidebar pane
- a persistent bottom footer or menu pane

Main-slot rule:

- sidebar or footer activation rebinds only the main slot target
- the current workspace client stays attached during in-workspace switching
- local implementations may use parked panes, parked windows, `swap-pane`, `join-pane`, `move-pane`, or equivalent tmux-native rebinding primitives as long as the visible chrome remains fixed
- future remote targets must bind into the same main slot through a dedicated bridge runtime rather than by replacing the entire workspace client

Fullscreen rule:

- the main pane may zoom fullscreen using tmux-native zoom behavior
- sidebar and footer are still architectural first-class panes and must restore cleanly after zoom exit
- history viewing during fullscreen should rely on tmux scrollback and copy-mode rather than custom replay layers

Future-proofing rule:

- sidebar and footer should consume a shared session registry and layout model that can represent both local tmux sessions and future network-backed sessions
- do not hardwire sidebar or footer state to one local-only daemon model
- a future remote session selected on the server may render through a waitagent `interact` runtime in the main slot while sidebar and footer continue to consume the same transport-agnostic session catalog
- local fullscreen, copy-mode, and tmux-native focus semantics remain the baseline for local sessions only; remote sessions will require separate server-routed interaction policy without changing the shared catalog shape

This is better than the historical split where command ownership was spread
across monolithic local-runtime and lifecycle surfaces with different runtime
styles.

## 7. Coding Rules For The Migration

Effective immediately:

- new tmux-first logic should not be added to retired monolithic compatibility surfaces unless the change is strictly bridging or deletion-related
- new runtime boundaries should be introduced in new files first
- old modules should lose responsibility over time instead of gaining new tmux-specific branches
- command handling should move toward one style and naming convention
- new modules should prefer explicit service types over giant free-function clusters or giant `impl App`
- preserve command compatibility where product behavior depends on it, but do not preserve old lifecycle code structure as a design constraint
- maximize reuse of tmux-native behavior instead of recreating tmux features in waitagent-owned runtime code
- if tmux already has a stable primitive for a user-visible feature, prefer exposing or wrapping that primitive over inventing a parallel waitagent protocol
- prefer trait-oriented adapter or service boundaries and explicit runtime objects so new modules converge on one style
- consult relevant Rust docs, Cargo docs, tmux docs, and source code when an integration detail is not already obvious from the local codebase
- from now on, prefer mature, widely used, and actively maintained Rust components across the board rather than homegrown infrastructure or niche dependencies
- do not reinvent abstractions that established maintained Rust components already solve well
- do not choose temporary or weakly maintained components for core architecture, even if they appear convenient in the short term
- no Rust source file may exceed 1000 lines; once a file approaches that limit, split it into narrower modules before adding more logic
- vendored tmux must compile in the default `cargo build` path; opt-in environment-variable gates are not an acceptable steady-state build model
- `build.rs` should stay thin and delegate vendored tmux build orchestration to tmux glue modules rather than embedding that logic inline
- local metadata inference based on tmux pane inspection is acceptable only for local sessions; future remote sessions must publish authoritative metadata through the transport layer instead of forcing the server to re-infer it from a local pane adapter
- tmux key bindings for waitagent chrome navigation must stay declarative and pane-targeted; avoid shell-based runtime probes that make navigation depend on the live contents of the pane
- if a user-visible target switch would leave the current client and re-enter through `attach`, the architecture is still wrong and the implementation should be reworked at the ownership boundary rather than patched with redraw suppression

## 8. Suggested First Concrete Module Split

That seed split is now materially in place. The current equivalents that define
the accepted route are:

- `src/bootstrap.rs`
- `src/infra/tmux.rs`
- `src/domain/workspace.rs`
- `src/application/workspace_service.rs`
- `src/runtime/workspace_command_runtime.rs`
- `src/runtime/workspace_entry_runtime.rs`
- `src/runtime/workspace_layout_runtime.rs`
- `src/runtime/main_slot_runtime.rs`
- `src/runtime/event_driven_pane_runtime.rs`

These are the minimum pieces that keep the tmux-first migration from
collapsing back into retired monolithic runtime ownership.

## 9. Migration Sequence

The code migration should happen in this order:

1. create the new architecture skeleton and module boundaries
2. add vendored tmux backend adapter and build integration inside the new `infra` layer
3. move workspace entry and lifecycle wiring onto the new bootstrap and runtime path
4. move sidebar and footer onto dedicated UI modules
5. add a stable main-slot activation path that keeps chrome fixed while rebinding local or future remote targets
6. move fullscreen and focus logic onto tmux-native services
7. delete or isolate the old custom fullscreen runtime inside `legacy`

## 10. Acceptance Criteria

The architecture migration is successful only if:

- retired monolithic compatibility surfaces stop being the default landing zone for new local-runtime behavior
- new tmux-first work enters through unified bootstrap and explicit runtime or application modules
- at least the tmux backend, workspace runtime, main-slot activation path, and pane UI entrypoints live outside the old monolithic files
- the resulting entry structure is simpler to explain than the current `main -> app + lifecycle + legacy fullscreen` split
- in-workspace target switching is expressed as `activate target in main slot` rather than as `detach current client and attach elsewhere`
- the module split is reflected in tasks before implementation continues
