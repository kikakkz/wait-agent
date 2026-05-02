# WaitAgent Execution Status Board

Version: `v1.35`  
Status: `Active`  
Date: `2026-05-02`

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

- `task.t5-08c4d3b` add explicit session-scoped remote live-mirror control so opened remote sessions stop falling back to placeholder-only state

Why this is the current gate:

- the shared transport-agnostic target registry, control-plane routing, and manual-only server-console model are already in place
- the dedicated remote node connection architecture is now explicit, including the accepted node-scoped long-connection, bounded backpressure, and reconnect ownership model
- the node-session proto and RPC contract are now explicit in the protocol doc, including the accepted gRPC service shape, envelope, versioning, status mapping, and reconnect baseline
- the production trust model, dialing direction, duplicate-session collapse, and canonical ownership policy are now explicit in the remote-node architecture doc
- the CLI-first network entry contract is now explicit too: remote networking must move off environment-variable startup knobs and onto public `--port` plus `--connect` arguments, with default listener bind `0.0.0.0` and default port `7474`
- the render bootstrap, replay, and observer catch-up policy is now explicit too, so the remaining blocker is no longer transport or ownership design drift but the final visible render binding and end-to-end product validation
- the first production cross-host ingress path is now landed through the repo-owned gRPC transport and ingress boundary
- shared live node-session ownership, disconnect-to-offline projection, and reconnect ownership are now centralized behind the node-session owner runtime
- the file-backed remote sidebar source has now been removed from the accepted visible-catalog path
- backend-scoped export and detach or reattach continuity work are now closed enough in substance to stop being the product blocker
- the latest cross-host review exposed the remaining product gap instead: remote session rows and remote activation exist, but the opened remote surface still lacks an explicit session-scoped live-mirror lifecycle, so real cross-host opens can fall back to placeholder-only state instead of showing the client session's actual screen
- that makes the current gate the missing live-mirror contract itself: explicit mirror open or close protocol, server-side per-session mirror ownership, PTY-owner mirror lifecycle, and visible first-screen parity on the accepted `--port` plus `--connect` path

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
- `task.t5-07` is now closed in substance: remote target open, input, output fanout, viewport-versus-PTY resize, publication ownership, shared node-session framing, and authority-host ownership convergence are all in code and validated
- `task.t6-01` remains the umbrella gate for server-console activation on the shared catalog and accepted routing boundary
- that umbrella is now execution-split as `task.t6-01a -> task.t6-01b -> task.t6-01c -> task.t6-01e` because the former auto-switch slice was cancelled and replaced by a manual-only policy-lock cleanup
- `task.t6-01a` is now closed in substance: local attach and remote interact both report through one explicit server-console interaction seam plus shared trace model instead of forcing later runtime work to branch directly on transport
- `task.t6-01b` is now closed in substance: remote server-console interaction now emits real submit and manual-switch signals through that unified seam, and server-console runtime state records those signals with console-local ownership instead of depending only on passive catalog refresh
- `task.t6-01c` is now historical context only: the later product decision removed the need to surface or preserve FIFO waiting order in the active UX
- `task.t6-01d` is now superseded: auto-switch was explicitly cancelled because automatic focus jumps are considered a poor user experience for this product
- `task.t6-01e` is now closed in substance: active docs and task state are aligned on a manual-only attention model where waiting state is surfaced in sidebar, picker, badges, counts, and related chrome without changing focus automatically
- `task.t5-08a1` is now closed in substance: the accepted protocol doc freezes `waitagent.remote.v1`, `NodeSessionService.OpenNodeSession`, typed protobuf envelopes, gRPC error mapping, versioning rules, reconnect baseline, and app-agnostic terminal semantics in place of the old JSON frame contract
- `task.t5-08a2` is now closed in substance: the remote-node architecture now fixes hub-and-spoke dialing, rustls mTLS admission, SPIFFE-style node identity binding, canonical session arbitration, duplicate-session containment, and reconnect ownership between node dialers and the server manager
- `task.t5-08a3` is now closed in substance: the server-owned bounded replay window, in-order observer bootstrap, reopen-based reconnect recovery, and explicit truncation policy now fix the remote render bootstrap contract without inventing a second rendering protocol
- the original phase-2 remote queue `task.t5-08a -> task.t5-08b -> task.t5-08c` is now through the ingress and ownership slices, leaving visible render binding plus end-to-end validation as the remaining product work
- `task.t5-08a` is now closed in substance: repo-owned protobuf generation, tonic-facing transport wrappers, a dedicated `remote_node_ingress_runtime`, and real gRPC-backed authority ingress now replace the old local-only assumption on the accepted production registration path
- `task.t5-08b` is now closed in substance: `RemoteNodeSessionOwnerRuntime` now owns shared authority-session reuse per authority node, steady-state publication transport ownership, disconnect-to-offline projection, and reconnect-plus-replay behavior without dropping the local authority bridge
- `task.t5-08c` now has stronger proof too: remote main-slot render coverage explicitly includes server-console observer scope, there is now a higher-level `RemoteMainSlotIngressRuntime` grpc-ingress-to-observer render-path combination test, and the acceptance checklist carries a dedicated phase-2 cross-host validation appendix instead of leaving end-to-end verification implicit
- `task.t5-08c` no longer lacks a public listener or dial contract in code: every process now starts the listener on the accepted path, `--port` owns listener configuration, `--connect` owns outbound dialing, and the remaining phase-2 gap is real cross-host validation plus any last visible render-path defects found there
- the earlier `task.t5-08c1` discovery batch landed and tested cleanly, but that publication-centric discovered-target model is now explicitly treated as superseded by the accepted `node -> sessions -> attachments` product semantics
- `task.t5-08c2` is now closed in substance: connected nodes synchronize their current remote sessions directly into the shared catalog on the `--connect` path instead of waiting on the old publication-centric discovered-target flow
- `task.t5-08c3` is now closed in substance: remote authority and observer traffic carry explicit `session_id` end to end, server fanout state keys off session identity, and `attachment_id` remains only the session-local observer handle
- the earlier `task.t5-08c4` delivery also surfaced a correction: while stable remote labels, task-state projection, and pane-local ingress ownership are in place, the remaining remote catalog path still merges file-backed discovered-session state into the visible sidebar and is therefore not an accepted end state
- a dedicated remote runtime-owner design is now locked too: one backend-scoped sidecar must outlive attached UI clients, own live node and remote-session state in memory, and expose that state through a clean local IPC boundary instead of `/tmp` caches
- `task.t5-08c4d2` is now closed in substance: runtime-only detach or reattach continuity and owner-restart semantics now preserve one live remote row while the backend owner is alive and clear rows correctly when it is gone
- the latest cross-host validation attempt also exposed that `task.t5-08c4d3` was too coarse: it mixed protocol gap, PTY-owner lifecycle gap, visible-bootstrap gap, and final acceptance into one umbrella even though the opened remote session still lacked a complete live-mirror design
- that umbrella is now split as `task.t5-08c4d3a -> task.t5-08c4d3d`, with the design now explicit in `docs/remote-live-mirror-design.md`
- `task.t5-08c4d3a` is now closed in substance: the accepted session-scoped live-mirror design, protocol additions, bootstrap rule, and bounded implementation split are explicit
- `task.t5-08c4d3b` is now the active gate: the protocol and server runtime must gain explicit mirror open or close control plus per-session mirror-route ownership before further cross-host UI validation can be trusted
- the dedicated server-console runtime now carries explicit focus and selection state while waiting attention stays visible through per-session state only
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
- a published remote-target store plus a node-scoped discovered-target store now act as additional shared-catalog producers beside local tmux, workspace-visible target projection includes those remote peers alongside same-authority local target hosts, and the old one-shot hidden publication commands are retired
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
- the hidden authority target-host runtime now resolves its own bound publication mapping, opens a persistent publication transport for the lifetime of the hosted PTY, and emits `target_published` on startup plus `target_exited` on shutdown, so authority-side PTY hosting is now participating as a real publication owner rather than only waking a separate helper
- authority-host publication fallback is narrower too: if no publication binding exists it does nothing, and if direct publication-session startup fails it only falls back to owner bring-up plus `refresh`, which keeps the old helper path as recovery rather than mixing it back into the normal steady-state sender
- the remote main-slot pane now models authority transport state explicitly as waiting for remote authority, waiting for a local bridge, connected, or failed, and authority registration mismatches surface as visible transport failures instead of silent background no-ops
- authority transport registration now also has its own runtime boundary: a dedicated authority-connection runtime owns the current local socket listener, registration validation, registry wiring, and disconnect or failure events, so future real remote connection sources can plug in without rewriting pane event flow again
- that authority-connection runtime now also accepts an explicit connection-source boundary rather than hardwiring the local socket listener as its only construction path, and the remote pane consumes it through that abstraction, so the next cross-node step can inject a real remote authority source without another pane-runtime structure change
- that connection-source boundary now has a second concrete implementation too: besides the local Unix-socket listener, authority-connection runtime can also consume externally injected authority streams through a queued source, which means a future real remote registration path can hand off accepted streams into the same runtime without masquerading as a pane-local listener
- the default pane-side authority path now actually uses that bridge shape too: a dedicated local-socket bridge starter accepts authority streams and feeds them into the queued source before authority registration continues, so the production path is already aligned with the external-producer contract rather than leaving it only in test scaffolding
- external producer wiring now also has a first-class starter API: `QueuedAuthorityStreamStarter::channel()` exposes a ready-to-inject authority starter plus stream sink pair, so the next real remote node registration slice has a concrete construction boundary instead of needing to assemble internal runtime pieces by hand
- external authority-stream ownership is now explicit at real lifecycle boundaries: `RemoteMainSlotPaneRuntime` keeps the queued authority sink under its own ownership, and `CommandDispatcher` exposes a top-level `submit_external_authority_stream` path instead of leaking the sink as constructor output
- the remote main-slot process now owns a real authority-ingress boundary too: `RemoteMainSlotIngressRuntime` binds the scoped authority transport socket at the process edge and hands each accepted stream into the in-process pane runtime through that top-level submit path, so authority registration is no longer only a constructor capability or a pane-internal listener shape
- that public authority transport is now also explicit as a node-facing bridge boundary instead of only a raw registration socket: `RemoteAuthorityTransportRuntime` performs a minimal `client_hello` / `server_hello` handshake on the pane-scoped transport socket, and ingress bridges that outer transport stream into the existing pane authority-connection runtime by creating an internal registered stream pair
- publication transport now uses the same outer node-facing handshake boundary too: a shared remote node-transport helper owns `client_hello` / `server_hello`, authority transport and publication transport both use it, and the publication server no longer begins on a raw registration frame before consuming `target_published` or `target_exited`
- live remote attach now also has one real shared outer node-session boundary: the authority target-host runtime performs one hello-authenticated connect and multiplexes authority input or resize, PTY output, and live `target_published` or `target_exited` metadata over that single stream instead of opening a second publication transport beside the authority transport
- remote main-slot ingress now demultiplexes that shared outer node session explicitly too: authority traffic still bridges into the existing authority-connection runtime, while live publication frames update a node-scoped discovered-target catalog and trigger global workspace-chrome refresh without routing through tmux publication bindings first
- the fallback publication path now uses that same node-session protocol boundary too: socket-scoped publication owner or agent processes send `publication` channel envelopes through `RemoteNodeSessionRuntime`, and the publication server consumes them on a hello-authenticated node session instead of a separate publication-only wire shape
- fallback publication transport ownership is now centralized as well: one socket-scoped publication sender runtime owns the cached outbound publication sessions, while publication owner and publication agent sidecars only submit local publish or exit commands into that sender instead of each keeping their own remote transport cache
- live authority-host attach now also exposes a local publication relay boundary and becomes the preferred publication owner while it is alive: publication owner or agent refresh and withdraw for that target first reuse the already-open live authority-host node session, and only fall back to the socket-scoped sender when no live relay is reachable
- that sender-side fallback owner is now explicit at the runtime boundary too: a dedicated `RemoteNodeSessionOwnerRuntime` owns the publication-sender listener and its outbound node-session cache, instead of leaving that cache nested inside publication runtime implementation details
- publication owner routing policy now lives with that owner runtime boundary too: publish and exit dispatch now flows through `remote_node_session_owner_runtime`, which owns the decision `live authority-host relay first, dedicated node-session owner second`, leaving publication runtime focused on tmux-bound publication semantics and shared-catalog updates
- authority-host publication orchestration is now explicit as well: the authority-host runtime no longer owns steady-state publication bootstrap or fallback logic directly, but instead consumes an injected publication gateway while the dedicated publication runtime remains the production implementation of that boundary
- the first `task.t6-01` slice now exists too: a hidden `remote_server_console_runtime` reuses the same remote observer, authority-ingress, and live publication path as the local remote main-slot instead of inventing a second server-console remote stack
- the current `task.t6-01` slice now exists as well: the dedicated server-console surface now supports a long-lived `picker -> target -> picker` lifecycle, resolves activation targets from the same shared catalog boundary, routes local targets through the existing local attach path, and routes remote targets through the shared remote interact surface with `Ctrl-]` returning to the picker
- the remaining `task.t6-01` gap is now narrower again: target discovery, activation routing, explicit focus, and real submit or manual-switch signal ownership are already in code, so any future T6 work should stay limited to manual-only attention-cue polish rather than focus automation
- the current remote review also fixed an important concept boundary for the last phase-2 slice: `--connect` is a node connection, not a session; one connection may publish many backend-owned local sessions; those sessions route by `session_id`; and remote projections are locally consumable but must never be republished

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
- `T6` Mirrored multi-console interaction: active, with the first server-console activation and scheduling-state slices now landed
- `T7` Reliability, security, and diagnostics: not started

## 6. Current Focus And Next Queue

Current focus:

- add explicit session-scoped live-mirror control on the accepted remote path so opening a remote session yields the client session's real visible screen rather than placeholder-only transport state

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

Remaining remote queue for phase completion:

1. `task.t5-08c4d3b` Implement explicit remote mirror open or close protocol messages and server-side session-route ownership
2. `task.t5-08c4d3c` Implement PTY-owner session mirror lifecycle and reuse on the connected client node
3. `task.t5-08c4d3d` Bind bootstrap replay plus live output into visible parity and close cross-host acceptance
4. `T3-07` Implement narrow-terminal compaction rules for the fixed-chrome workspace layout only if acceptance evidence proves compact layout is blocking

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
