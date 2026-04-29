# WaitAgent Execution Status Board

Version: `v1.22`  
Status: `Active`  
Date: `2026-04-28`

## 1. Purpose

This document is the human-facing project status snapshot for WaitAgent.

It is intentionally no longer the place for exhaustive machine execution state.
Detailed task routing, blockers, verification history, and reusable assistant procedures now live in `.agents/`.

Use this document for:

- the current phase and why it matters
- the current human decision point
- milestone and track-level progress
- the next queue after the current gate closes

Use `.agents/` for:

- exact current task state
- task backlog ordering
- the complete machine task inventory
- blocker records
- verification records
- reusable assistant procedures

## 2. Current Phase

Current phase:

- `Phase 2: Network Aggregation MVP`

Current gate:

- `task.t5-07` remote control-plane routing and publication ownership on the shared target catalog

Why this is the current gate:

- the shared transport-agnostic target registry is already in place and is now the fixed boundary for remote work
- remote open, input, output fanout, and viewport-versus-PTY resize semantics are already routed through that control-plane boundary
- remote publication now reaches the shared catalog through a socket-scoped publication server plus long-lived publication agent rather than per-target helper mutations
- the next blocking gap before server-console work is replacing the still-local hook-triggered publication lifecycle with actual node-owned publication ownership and richer live metadata pushes

## 3. Current Snapshot

Project status at a glance:

- product, architecture, functional, module, UI, interaction-flow, protocol, and MVP planning docs exist
- the Rust implementation workspace and core local runtime are in place
- the tmux-first local path already owns the visible workspace chrome
- the accepted new direction is now stricter than the earlier tmux-window switching model: sidebar and footer stay fixed while only the main view changes
- `task.event-r2` is complete: chrome updates, session-catalog refresh, pane refresh, and shell-exit cleanup now use explicit events rather than pane-local polling loops on the accepted path
- `task.event-r2a` is now accepted for the local product goal: same-socket switching uses tmux-native pane rebinding, target hosts are modeled separately from the visible workspace chrome session, active-target projection comes from workspace state instead of the visible chrome session id, workspace lifecycle hooks refresh only the affected workspace chrome, startup materializes the initial target identity before attach, and real-terminal sidebar or footer switching keeps the fixed chrome mounted
- local acceptance is no longer blocked on deleted legacy interaction features, because they are not part of the accepted current product scope
- `task.event-r3` is now closed for the accepted local scope: attach and resize validation passed, explicit runtime events now own the accepted control path, and stale auto-switch wording has been retired because auto-switch is not part of the current product contract
- `task.event-r4` is now closed: the event-driven local route is accepted as the default baseline after user-reported shell and Codex visible-behavior validation
- event-r4 cleanup aligned `.agents` entrypoints, project context, and current architecture docs on the real default path `bootstrap -> CommandDispatcher -> WorkspaceCommandRuntime`
- `task.t5-06` is now split into `task.t5-06a -> task.t5-06b -> task.t5-06c` so the remote foundation lands as separate documentation, model, and registry-boundary slices
- `task.t5-06a` is now closed in substance: the remote foundation doc and bounded task split are landed
- `task.t5-06b` is now closed in substance: the transport-agnostic target model and protocol identity contract are explicit in code and docs
- `task.t5-06c` is now closed in substance: local tmux now sits behind an explicit target-registry boundary and current consumers read unified target records through it
- `task.t5-07` is now the active gate: route remote target open and input through the server control plane, and refine remote resize semantics so attachment viewport changes stay local while PTY resize remains a distinct control-plane path
- the first `task.t5-07` slice is now refined in code: protocol envelope types exist, remote opens create attachments plus PTY-resize authority state, multi-console input is serialized by the server control plane, and opening or fullscreen viewport changes no longer auto-propagate fake PTY resize traffic before deeper runtime integration
- a dedicated `remote_main_slot_runtime` boundary now exists: the main-slot remote branch can derive console identity plus viewport size and turn remote activation into routed control-plane messages against an explicit transport sink, while remote render-path work remains the next gap
- remote control-plane fanout is now resolved to concrete per-node deliveries before the sink boundary, so future transport code can send node-bound messages directly instead of reinterpreting internal broadcast destinations
- the default workspace runtime now uses a concrete connection-registry sink for remote activation, so the remaining transport gap is registering live node connections and forwarding remote output rather than defining yet another sink abstraction
- the local workspace console now auto-registers a loopback observer connection for remote activation, which means observer-side open-target delivery already works in-process and the remaining network gap is primarily authority-side connection ownership plus remote output/render wiring
- the same remote main-slot boundary now exposes explicit open, input, and PTY-resize send paths, and authority-side loopback registration can receive those messages in tests, so the remaining product gap is no longer control-plane shape but live connection sources plus output/render wiring
- authority-side `target_output` fanout now also flows through the same boundary and reaches observer loopback mailboxes in tests, which means the biggest remaining gap is not control-plane routing but hooking live connections and real terminal rendering onto the delivered output stream
- a dedicated remote observer runtime now consumes delivered observer envelopes, decodes `target_output`, and rebuilds local terminal state for remote targets, so the next clean step is binding that observer state into main-slot presentation rather than extending transport or server-side PTY ownership
- remote targets can now respawn a dedicated workspace main-pane process that opens the target, consumes observer-side mailbox growth, and renders remote terminal state locally, so the active gap is no longer “remote render path missing” but “live authority transport and connection registration are still not plugged into that pane runtime”
- a first cross-process authority registration path now exists through a local socket transport and a no-dependency control-plane envelope codec, so remote panes no longer rely only on in-process loopback registration; the active gap is now full authority-side ownership and sustained bidirectional delivery rather than the absence of any live registration bridge
- a dedicated authority transport runtime now exists on the other side of that socket boundary, so a PTY-owning node can connect or register, receive routed `target_input` plus `apply_resize`, and send `target_output` back into the same observer render path; the next gap is binding a real authority-side PTY host onto that runtime instead of relying on transport-only tests
- that next PTY-host binding slice now exists as a hidden bounded runtime: a real tmux target pane can be selected on the authority node, consume routed input plus PTY resize, and stream pane output back through the same control-plane path via a pipe-pane output pump
- remote targets now also support an optional selector that can resolve the concrete PTY host on the authority node, and the remote main-slot pane uses it to auto-spawn that hidden authority-host bridge when the selector is locally resolvable in the current loopback-compatible process model
- a published remote-target store now acts as a second shared-catalog producer beside local tmux, workspace-visible target projection includes those remote peers alongside same-authority local target hosts, and the old one-shot hidden publication commands are retired
- a socket-backed remote publication listener now owns published-target updates for one workspace tmux socket, so authority-side publication transports send `target_published` and `target_exited` envelopes over a local control-plane transport instead of mutating the shared catalog by spawning per-event helper commands
- a socket-scoped remote publication agent now owns reconciliation for one tmux socket, keeps persistent per-authority publication transports alive across reconciles, and uses source-socket metadata in the published-target store to clean stale remote targets without one bridge process or lockfile per target
- remote publication metadata can now be bound directly onto target-host tmux sessions, workspace startup auto-discovers those bindings, ensures the publication server and publication agent path only when the current socket actually needs publication recovery, and uses tmux hooks only to signal socket reconciliation on that agent
- workspace startup publication recovery is now itself socket-scoped, so one workspace boot no longer scans every waitagent tmux socket or eagerly starts publication sidecars just to clear remote publication state; stale records still reconcile on startup when they belong to the current socket
- repeated reconcile publishes that do not actually change remote-target metadata are now treated as no-ops at the publication listener, so the current hook-triggered publication path no longer rewrites the shared store or refreshes every workspace chrome row on unchanged metadata
- when remote publication metadata does change, the publication listener now refreshes only workspace chrome sessions on the same tmux socket instead of fanout-refreshing every workspace across every socket
- a live target-host session can now explicitly withdraw its remote publication metadata through the same socket-scoped control path, so `target_exited` no longer depends on killing the tmux session when the product only wants to stop publishing that target
- the published-target store now tracks source-socket membership per remote target instead of a single source socket, so one workspace/socket withdrawing a target no longer incorrectly removes a still-published copy of that same remote target from other local workspaces
- socket-global lifecycle hook ownership is now split away from workspace layout: publication runtime itself registers and maintains the `session-created`, `session-closed`, `client-attached`, and `client-detached` hooks per tmux socket, and those hooks drive socket-scoped reconcile plus socket-scoped chrome refresh through dedicated hidden commands
- those socket-global lifecycle hooks now enter through one dedicated socket-lifecycle hidden command instead of a shell-composed pair of commands, and `CommandDispatcher` is the composition boundary that runs publication reconcile plus socket-scoped chrome refresh without making publication runtime reach back into layout runtime
- publication server itself no longer instantiates layout runtime when remote-target metadata changes; it now applies catalog mutations locally and then re-enters the hidden command boundary for socket-scoped chrome refresh, so runtime composition stays aligned on the command dispatcher boundary
- publication agent now coalesces bursts of queued reconcile signals before scanning tmux state, which reduces redundant full-socket reconcile passes under tmux hook storms without changing the socket-scoped ownership model
- explicit local publication bind and unbind now push `target_published` and `target_exited` immediately for the concrete target-host session they already know about, so full-socket reconcile is narrowed further toward startup recovery and hook-driven metadata refresh instead of being the default publication lifecycle path
- socket-lifecycle publication updates are now hook-aware too: the hidden lifecycle hook command carries tmux hook name plus hook session name, and attach, detach, or session-create churn now prefers a direct republish of just that one bound target-host session instead of forcing a full-socket publication scan every time
- publication emission itself is now re-centered on the long-lived socket-scoped publication agent: bind, unbind, and targeted hook updates enqueue explicit agent commands instead of opening one-off publication transports directly, so transport cache ownership and concrete publish or exit delivery live in one runtime again
- published-target membership is now source-binding scoped rather than only source-socket scoped, so the catalog can distinguish multiple publishers on the same tmux socket and keep or remove each remote target by the exact `(socket, target-session)` owner
- `session-closed` publication handling now uses that source-binding identity to prefer targeted `exit_target` fanout for the closed publisher, leaving full-socket reconcile as fallback recovery instead of the normal close path
- explicit target-host close ownership now emits publication exit before tmux session teardown as well, so locally-owned target shutdown is no longer primarily waiting for the later tmux hook to withdraw the published remote target
- explicit target-host detach on the command path now emits a publication refresh after client detachment too, so attached-client count changes are not only waiting for the later `client-detached` hook before remote metadata catches up
- explicit current-client detach inside tmux now also opportunistically refreshes publication metadata when the detached session resolves to a local target-host, so the common `waitagent detach` path no longer waits only on the later hook either
- explicit local target-host publication ownership is now gathered behind `TargetHostRuntime`, so main-slot close and workspace detach paths no longer orchestrate owner-side publication refresh or exit by calling publication runtime directly
- each bound local target-host now also auto-spawns a dedicated publication owner runtime that watches its own session metadata and triggers publish refreshes on change, so live attached-client and command or path metadata no longer rely only on tmux socket hooks for steady-state updates
- that publication owner runtime now sends steady-state `target_published` and `target_exited` envelopes directly over the publication transport instead of only nudging the socket agent, so bound target unbind and session-disappearance exit are beginning to follow an actual owner-driven publication channel
- live binding bootstrap is now narrowing too: bind and startup owner bring-up no longer force a full-socket reconcile just because live bindings exist, so the socket agent is moving toward stale-record recovery and fallback cleanup instead of being the normal source of first publish for healthy owners
- targeted publish hooks and explicit local refresh now also prefer owner bring-up over agent publish: `client-attached`, `client-detached`, `session-created`, and local detach-driven refreshes first ensure the bound publication owner is running, making hook-triggered publish increasingly a restart fallback rather than the steady-state sender
- the owner socket now exposes a minimal control plane too: explicit refresh and close paths can ask the bound publication owner to `refresh` or `stop`, and `session-closed` fallback exit now stands down when that owner is still reachable, so even more targeted publish or exit traffic stays on the owner-driven path before hook or agent fallback engages
- explicit unbind now uses that owner control plane too: it first asks the bound publication owner to `stop` and only falls back to legacy source-session exit signaling when no owner is reachable, so withdraw is no longer mainly waiting on owner polling or hook cleanup
- the hidden authority target-host runtime now also participates in publication owner bring-up: when a loopback-resolved authority PTY host starts, it ensures the bound publication owner exists and immediately asks it to refresh, so owner lifecycle entry is no longer only coming from workspace startup or publication-side command paths
- the remaining gap is no longer “how does the pane find a PTY host?” or “can the UI see a remote target at all?” but “how do real remote nodes replace this still-local hook-triggered publication path with an actual node-owned publication lifecycle and richer live metadata pushes?”

## 4. Milestone Summary

| Milestone | Goal | Status |
| --- | --- | --- |
| `M0` | Product and design baseline completed | `done` |
| `M1` | Local single-machine workspace UX usable end to end | `done` |
| `M2` | Network aggregation MVP usable end to end | `in_progress` |
| `M3` | Hardening, observability, and developer usability | `not_started` |

## 5. Track Summary

Execution tracks at human-summary level:

- `T0` Documentation and planning: active and aligned with the refined fixed-chrome architecture
- `T1` Local runtime foundation: complete enough for the current architecture correction
- `T2` Event-driven control path: complete enough for the accepted local scope
- `T3` Terminal UI and rendering: the old custom fullscreen and shared-surface path remains retired
- `T4` Local workspace UX and validation: complete enough for the current local scope
- `T5` Network transport and registration: active again, with the remote foundation now split into documentation, model, registry-boundary, and later routing slices
- `T6` Mirrored multi-console interaction: not started
- `T7` Reliability, security, and diagnostics: not started

## 6. Current Focus And Next Queue

Current focus:

- execute `task.t5-07` now so resumed remote work gains real server-controlled open and input routing, plus a clean split between local attachment viewport changes and PTY-resize control, before server-console work begins

Accepted local architecture direction:

- one persistent workspace chrome with fixed sidebar, fixed main slot, and fixed footer or menu
- selecting a sidebar or footer item rebinds only the main slot target
- in-workspace switching must not detach the current client, reveal the shell, or rebuild the workspace chrome
- local targets live inside one tmux backend and switch through tmux-native rebinding primitives rather than by launching a fresh attach command
- future remote targets must fit the same transport-agnostic target catalog and render into the same main slot through a bridge runtime
- `waitagent` or `workspace` may bootstrap a backend; `waitagent attach` joins an existing backend only

Accepted event-driven delivery queue:

1. `event-r1` Establish the new event-driven local runtime architecture and event contract
2. `event-r2` Implement event-driven tmux chrome, session catalog, and pane update flows
3. `event-r2a` Replace cross-session attach switching with fixed-chrome main-slot target activation
4. `event-r3` Move remaining attach, resize, and lifecycle control onto explicit runtime events
5. `event-r4` Route the default local path through the new event-driven stack and isolate polling history only if future remote design still needs that split

Priority rule:

- no deleted legacy surface should be revived during remote planning
- remote and local session management should be redesigned on top of the cleaned tmux-native workspace baseline

Refined remote queue after the current documentation slice:

1. `T5-07` Implement remote target input routing and clean remote resize boundaries through the server control plane
2. `T6-01` Implement the server-side workspace console as a target-activation surface
3. `T3-07` Implement narrow-terminal compaction rules for the fixed-chrome workspace layout if acceptance evidence makes it necessary

The exact machine ordering for that queue lives in `.agents/tasks/backlog.yaml`.

## 7. Human Sign-Off Notes

The local product contract that must survive the migration is:

- shell-backed sessions still feel like real reusable shell contexts
- Codex-like TUI behavior remains trustworthy inside WaitAgent
- sidebar and menu remain first-class workspace controls
- sidebar and menu stay mounted while switching targets in normal mode
- fullscreen still exists and behaves like a real terminal view
- UTF-8 and Chinese input remain readable in practical use
- the local display architecture should stop generating chrome-switch artifacts that would distort later network debugging

## 8. Maintenance Rule

Update this board when:

- the project phase changes
- the current human gate changes
- milestone-level progress changes
- the next queue changes in a way humans need to understand

Do not re-expand this file into a machine task database.
That role belongs to `.agents/`.
Any task that becomes real work must be represented in `.agents/tasks/`; do not keep orphan tasks only in docs or chat.
