# Relay Feature Design

Interconnecting hosts across locations. A site-1 node actively makes an
outbound connection to a publicly reachable relay; a site-2 node likewise
connects to the relay and establishes a session with the site-1 node through
the relay, with an experience fully identical to today's direct connection.
The direct-connection path is kept; when no relay is configured, behavior is
fully identical to the status quo.

## Goals and Non-Goals

Goals:

- Positioning: self-hosted. The name is simply relay; the public-cloud
  version gets its own naming and design (covered separately alongside
  billing).
- Phase 1 (this document): client-server star topology. Every node keeps one
  long-lived TLS connection to the relay, and cross-node traffic is
  multiplexed on that connection as streams.
- Data policy: the relay only forwards; it persists no session or traffic
  data; on-disk state is limited to relay.toml, the whitelist directory, and
  token-generation parameters. Traffic is routed in memory by frame headers
  only.
- Single-machine capacity model: calibratable, configurable, observable
  admission control, serving as input for a future cluster scheduler.
- Management plane: the WebUI is the everyday management plane (connection
  table, usage, invite, online status, in-browser coding); the CLI is reduced
  to a minimal bootstrap set (serve, join, one-line install command).
- Reuse the existing node↔node protocol: the TLS on top of it,
  AuthorityTransport, ControlPlane, and the session machinery
  (OpenMirror/RawPty/Resize/bootstrap) are all left untouched.
- Mutual authentication: the relay only accepts its own nodes; a node only
  connects to its own relay.
- Reserve extension points for future cloud billing (API key + subscription
  tiers); Phase 1 does not implement billing.

Non-goals:

- NAT traversal / P2P (Phase 2; only the transport leg is replaced, the mux
  and protocol layers are reused).
- Cross-account access authorization. Nodes of different accounts are fully
  independent through the relay: there is no mutual trust and no pairing
  mechanism is needed. Cross-account concerns for public relay clusters are
  designed separately alongside billing.
- Session-data persistence / recording / audit to disk (the relay does none
  of these; see data policy).
- Browser-side E2E for the WebUI (in the self-hosted form the web service
  sees traffic, equivalent to a person sitting in front of that machine; the
  public-cloud form upgrades this further — see the billing extension
  points).

## Architecture

```
site1 node ══TLS long conn══╗                 ╔══TLS long conn══ site 2 node
site3 node ══TLS long conn══╣═══ relay hub ═══╣══TLS long conn══ site 4 node
                            ╚════════════════╝
```

- After startup, each node establishes one persistent connection to the
  relay: authentication + registering node_id, heartbeats, and carrying all
  cross-node traffic. Disconnects are handled by the node side reconnecting
  and re-registering (reusing the existing reconnect worker pattern).
- site2 connecting to site1 = opening one virtual stream over the two
  long-lived connections: site2 sends
  `open_stream(target_node_id)`, the relay routes it, and both ends get a
  pair of full-duplex, ordered, reliable byte streams — the same semantics
  as a TCP connection.
- Integration seam: abstract the existing "node dials node" TCP
  establishment path into `PeerConnection` (an ordered, reliable,
  full-duplex byte stream). Direct TCP is one implementation; a relay stream
  is another. The existing protocol code runs on top of the seam and is
  unaware of the transport.

## Protocol Layers

```
Existing session protocol (AuthorityTransport / ControlPlane / ClientHello...)   unchanged
node↔node TLS (existing rustls mutual self-signed certs, E2E encryption)         unchanged
────────────────────────────────────────────────────────────
mux frame layer: stream open / close / data / window (per-stream backpressure)
relay control frames: register / unregister / open_stream / close_stream /
             error (structured reason code) / heartbeat
node↔relay outer connection: TLS + heartbeat
────────────────────────────────────────────────────────────
relay hub: connection table (node_id → connection), stream routing, fair scheduling, per-stream windows
```

The relay only terminates the outer TLS and parses mux frame headers for
routing; the content between nodes remains the inner E2E TLS ciphertext, so
the design holds even with an untrusted relay.

## Authentication and Enrollment (decided: invite-based)

Device identity = the SHA-256 SPKI fingerprint of the node's self-signed
certificate (`~/.waitagent/node.crt`), reusing the existing mechanism in
`src/infra/node_credentials.rs`. No CA.

- node → relay: the relay holds a separate self-signed certificate; the node
  pins the relay fingerprint in `~/.waitagent/relay.toml`, and TLS verifies
  "peer fingerprint == configured value", dropping the connection on
  mismatch.
- relay → node: the relay maintains the `authorized_nodes/` whitelist
  directory (following the `authorized_operators/` pattern); the mTLS
  handshake requires a client certificate, and a fingerprint not in the
  whitelist is rejected — the rejection happens at the transport layer.

Enrollment flow (`waitagent relay invite` / `waitagent relay join`):

1. On the relay side (or any already-trusted node), run
   `waitagent relay invite`, which generates a one-time, short-lived
   invitation token and copies it to the clipboard automatically; for mass
   provisioning, a long-term deployment token can be used.
2. On the new node, run `waitagent relay join <relay-address> <token>`:
   - The token authenticates the enrollment session; without a token no
     session can be established (no MitM window).
   - Once the session is established, the relay delivers its own certificate
     fingerprint over that authenticated channel, and the node writes it to
     `relay.toml` automatically.
   - The relay also automatically adds the node's certificate fingerprint to
     the whitelist.
   - Mutual binding completes in one pass; no fingerprint needs to be copied
     by hand at any point.

Self-service enrollment (issue #208, same mechanism as the dashboard magic
link):

1. The requester runs `waitagent relay join-request <relay:port>` on the
   initiating device: without any credential, the CLI posts the node
   credential fingerprint (SPKI SHA-256, stably recomputable on the machine)
   to the relay's dashboard public route `POST /auth/join-request`.
2. The relay emails the administrator using exactly the same SMTP/JWT
   facilities as dashboard login (the subject distinguishes the two flows;
   `sub = "enroll-magic"`, valid for 10 minutes, single-use jti, AuthStores
   rate limit of 3 per 10-minute window); when mail is not configured, it
   gives an actionable error and points to the `relay invite` path.
3. The administrator forwarding the email to the requester constitutes
   approval (no separate approval UI).
4. The requester runs on the same device
   `waitagent relay join <relay> --link <URL from the email or bare token>`:
   the CLI recomputes the fingerprint and calls `POST /auth/enroll`; the
   server validates sub/exp, compares the device fingerprint, and consumes
   the jti, then mints a one-time enroll token via the same admin-socket
   Invite channel as the dashboard and returns it; the CLI then completes
   enrollment and pinning via the existing Enroll protocol (#209 dual-path
   negotiation) — zero changes to the Enroll wire protocol. Both HTTPS legs,
   consistent with join, go through the activated proxy profile.

### Single-Port ALPN Demultiplexing (issue #209)

The relay serves three traffic classes on a single public port,
demultiplexing by the ClientHello ALPN before the TLS handshake
(`LazyConfigAcceptor` peeks the ClientHello → selects the ServerConfig and
handler):

| ALPN | Traffic | Client auth | Handler |
|---|---|---|---|
| none / other | node link (backward compatible) | whitelist mTLS | existing register/heartbeat lifecycle |
| `waitagent-enroll` | enrollment | any client cert (the token is the authorization) | existing Enroll frame protocol |
| `h2` / `http/1.1` | dashboard Web | no client cert | axum app (served by `relay serve --web`) |

- link clients send no ALPN; "no ALPN → link" guarantees zero-change
  compatibility with old nodes.
- join dual-path negotiation: on a new relay the first hop succeeds
  directly; on an old relay (whose main port whitelist-rejects unregistered
  certificates) the first hop fails at TLS and automatically falls back to
  the legacy enroll listener on `port + 1`. Fallback happens only on
  TLS-layer failure; protocol-layer errors after a successful handshake
  (token rejection, etc.) are reported as-is. The error message states that
  both paths were tried.
- The dashboard uses a self-signed certificate, so browsers show a
  certificate warning on `https://relay:7475/` (accepted in #131); session
  cookies served over TLS carry the `Secure` attribute, and the legacy HTTP
  listener does not add it.
- **Deprecation window**: the `port + 1` enroll listener stays on by default
  and can be disabled via `legacy_enroll_listener = false` in relay.toml or
  the CLI `--no-legacy-enroll`; the standalone HTTP web listener (8788) stays
  as-is, with a deprecation notice in the startup log. Both are removed after
  the window.

## Node Lifecycle

- Enrollment: after join, the fingerprint is written to the
  `authorized_nodes/` whitelist; the node registers and establishes the
  long-lived connection, and the relay's connection table records
  `node_id → connection`.
- Heartbeat: the node sends one frame every 10 seconds; if the relay misses
  3 in a row (30s), it deems the node offline, cleans up the connection
  table, and releases capacity. A disconnected node reconnects and
  re-registers from its own side (reusing the existing reconnect worker
  pattern). Intervals and thresholds can be tuned in relay.toml; the
  defaults are as above.
- Voluntary offline: the node sends an unregister control frame for a
  graceful shutdown.
- Revocation: `relay remove <fingerprint>` deletes the node from the
  whitelist and drops its online connection; a revoked node's subsequent
  register fails at the transport layer (the mTLS fingerprint check finds it
  not in the whitelist).
- Online status: register / unregister / heartbeat-timeout events are pushed
  over a structured channel to interested parties' consoles — the sidebar
  status of the remote-hosts entry and the status line show the peer node
  online / offline (same channel as the error semantics; interest is
  established when register declares remote-hosts or open_stream targets
  that node).

## Management Channel (WebUI / CLI ↔ relay)

- The relay daemon is started by `waitagent relay serve` (the hub:
  listening, connection table, routing, heartbeats, capacity admission).
- **The WebUI is the everyday management plane.** The web service is a
  special node: it holds its own certificate and enrolls via the standard
  register; management operations (connection table, usage/capacity, invite,
  remove, online status) reach the relay as remote control streams over its
  authenticated node↔relay long-lived connection; the in-browser terminal
  likewise reuses the existing ControlPlane (OpenMirror/RawPty/Resize) to
  join each node's session as one more observer participant.
- Deployment form: the web service runs on the same machine as the relay and
  starts as one (`relay serve --web` brings up the web plane; fail-stop
  single Lifetime: if either half exits, the whole process exits), same
  repo, same binary; the two processes talk over loopback using the standard
  node↔relay protocol, with no special-case "in-process over memory" path
  (the local hop is microseconds, negligible against the WAN; the single
  protocol path is dogfooded by all nodes; the failure domain stays decoupled
  from the deployment topology, and an in-memory shortcut is reconsidered
  only once load tests provide measured evidence).
- **The CLI is reduced to a minimal bootstrap set**: `relay serve`,
  `relay join`, plus a one-line install command for mass provisioning (the
  WebUI generates a token → paste and run it on the node).
- The relay-role bootstrap (Install as relay) idempotency probe gains a
  **version gate** in addition to the admin socket (issue #215): after the
  probe passes, it reads the running relay's version via `relay status`; if
  the version is missing (an old relay's status has no version field) or
  does not match the current binary, it first runs `relay shutdown`, waits
  for the socket to disappear, and then restarts with `relay serve` from the
  current binary; only a matching version is kept as-is. An old process lacks
  single-port ALPN demultiplexing, and its link handler would cut Enroll
  frames short ("truncated frame header or payload"), so it must not be left
  in place by mistake.
- The local admin socket (following the owner-control socket pattern of
  `remote_node_ingress_owner_socket_path`; Windows uses an equivalent named
  pipe / `.port` marker) is kept as a bootstrap and emergency channel;
  everyday management does not go through it. When the CLI cannot connect to
  a running relay, it reports a clear, guiding error and does not start one
  implicitly.
- Permission boundary: self-hosted single-administrator form — if the WebUI
  is reachable, it can be managed (equivalent to owning the machine);
  revocation (remove) is performed in the WebUI, not via a remote node
  control stream. Multi-account / fine-grained authorization is designed
  separately with the public cloud.

## node_id Strategy

- In the protocol, node_id remains an opaque String; the protocol is not
  changed.
- Direct mode: keep the existing `host#port`.
- Relay mode: node_id is the certificate fingerprint directly (or a short
  form), self-proving globally unique, no registry needed.

## Billing Extension Points (Phase 1 leaves the seam only; not implemented)

- The auth block of register carries a version number; the authentication
  backend is pluggable: private mode = whitelist (local Authenticator),
  cloud mode = API key (Cloud Authenticator calling the control plane).
- `open_stream` passes through a Policy hook: private mode = allowlist;
  cloud mode = quota check (concurrent peer count per account) + a
  structured reason code on rejection.
- The relay cluster does not take a synchronous dependency on the billing
  service: quota credentials with short TTLs signed via key rotation
  (JWT-style) are validated entirely locally afterwards; metering is
  reported asynchronously in batches.
- Public relay form: the relay uses a real domain name + a public-CA
  certificate (WebPKI validates the domain); fingerprint pinning is kept only
  for self-owned deployments.

## Capacity Modeling and Admission Control

Positioning: a Phase 1 single-machine relay must already have a
**calibratable, configurable, observable** capacity model. It is the input
for future cluster scheduling — the scheduler assigns nodes to relays by
`admission decision = capacity (capability estimate) − usage (current use)`.

Capacity dimensions (single machine, all overridable in relay.toml):

- `max_nodes`: maximum number of concurrent node long-lived connections.
- `max_streams`: maximum number of concurrent streams.
- Forwarding throughput guard threshold (bytes/s): when exceeded, new
  open_streams are rejected (conservative policy; reevaluate after load-test
  calibration whether to change to window-tightening degradation).
- Resource baseline: per-connection / per-stream memory and CPU costs are
  published as measured values with each release, and deployers override the
  defaults with measurements from their own machines.

Metering and admission:

- The relay hub's connection table / stream table is itself real-time usage
  (active connection count, active stream count, per-stream window
  occupancy, forwarding byte rate); metering happens on the routing path
  with no separate metrics system.
- register over `max_nodes` → structured rejection; open_stream over
  `max_streams` → structured rejection (hooked on the decided Policy hook).
- A connection reclaimed on heartbeat timeout releases capacity immediately;
  a node reconnects and re-registers for fresh admission.

Calibration method: e2e load tests (docker network isolation; see the
task-breakdown test items) scan the knee points of connection count / stream
count / throughput and produce recommended defaults; defaults are published
conservatively and revised after measurement.

Observability: a relay subcommand shows capacity and usage; when a node's
register / open_stream is rejected, it receives a structured reason code,
shown in the console via the decided error channel.

## Error Semantics (quota / key / device unbinding, etc.)

The error channel relay → node → console gets its structure defined now:
error code + human-readable message, displayable on the sidebar item status
and the status line, no raw strings.

## Configuration Surface

- Node side `~/.waitagent/relay.toml`: `address`, `relay_fingerprint`
  (written automatically after join), heartbeat parameters.
- Relay side `~/.waitagent/relay.toml`: listen address, admin socket path,
  whitelist directory path, token-generation parameters, heartbeat parameters
  (interval default 10s, offline threshold default 3 misses), capacity limits
  (`max_nodes`, `max_streams`, throughput guard threshold).
- remote-hosts entries gain `via = "auto" | "relay" | "direct"`: auto is the
  default (old entries default from an implicit direct upgrade; explicit old
  values are preserved); auto = direct first, relay as fallback — connect
  probes direct first (TCP + TLS pin handshake, short timeout bound) and
  dials via relay on failure; explicit direct/relay does not fall back
  (guidance is given in the error message). The effective path is recorded in
  `last_via_used` for UI display; user choice is not silently rewritten
  (issue #156, superseding this file's earlier "explicit specification, no
  implicit fallback" decision entry).
- The `tls_pin_sha256` of a remote-hosts entry may be omitted: when the pin
  is missing and via is auto/relay, connect queries the peer's registered
  fingerprint by host label via the node↔relay management channel's
  `resolve-node` (adopted only on a unique hit; multiple hits / no hit fall
  back to the legacy SSH bootstrap path), and the discovered pin is persisted
  with the profile and noted in the response (issue #156 slice 3). After
  registering, a node announces its own labels over the same channel via
  `announce` (hostname + egress IP toward the relay's literal address,
  ≤8 entries × ≤64 bytes), writing only its own connection-table entry on
  this link.

## Compatibility Commitments

- The relay is only a new transport branch on the dial path; when no relay
  is configured, the code path is fully identical to today.
- The existing direct TCP and socks5 proxy paths are untouched.
- All existing session protocols are unchanged.

## Phase 1 Task Breakdown

Status legend (issue #137 reconciliation, 2026-10-05): ✅ landed /
🔶 partial / ⬜ not implemented / — out of scope this phase; anchor =
landing PR or code location. Status is updated with each landing PR.

1. ✅ Landed — mux frame layer + `PeerConnection` seam (the existing TCP
   dial is moved into the seam, behavior unchanged). Anchors: #66 (dial
   moved into the seam), #69 (mux frame codec and stream state machine);
   `src/infra/peer_connection.rs`, `src/infra/relay_mux/`. The mux module
   only does stream multiplexing and window frames; relay control frames
   live in `src/infra/relay_link.rs` / `src/infra/relay_routing.rs`.
2. ✅ Landed (inbound path completed 2026-10-05) — relay daemon and
   subcommands: `relay serve`, local admin socket and local admin protocol
   (#81), remote control streams (node-side management channel, #135 read /
   #140 write), connection table (#75), open/close routing (#78), heartbeats
   (10s / 3-miss timeout, #75), whitelist authentication (#72, including
   `relay remove` revocation; process-level e2e for revocation in the #145
   revoke scenario). Inbound relay streams are wired into node ingress
   (#129's #130/#132, 2026-10-05), making both directions usable. Anchors:
   `src/infra/relay_server.rs`, `relay_admin.rs`, `relay_remote_admin.rs`,
   `relay_connection_table.rs`, `relay_routing.rs`, `relay_ingress.rs`,
   `src/cli/relay.rs`.
3. ✅ Landed — `relay invite` / `relay join` enrollment flow (mutual
   automatic pinning). Anchors: #87; `src/infra/relay_enrollment.rs`,
   `relay_join.rs`, `src/command/relay_enroll.rs`.
4. ✅ Landed (inbound completed 2026-10-05) — node relay client: long-lived
   connection keepalive (#90), on-demand stream open (#93, `via = "relay"`
   dial), re-register on disconnect (#90), presence events received into the
   console (#96). The inbound accept queue is wired into ingress
   (#130/#132). Anchors: `src/infra/relay_client.rs`, `relay_presence.rs`,
   `src/ratatui_node/client_runtime.rs` (status line rendering).
5. ✅ Landed — per-stream backpressure (window/ack) and fair scheduling.
   Anchors: #101 (backpressure + DRR fair scheduling), #110 (control frame
   bypass), #123/#125 (fairness assertions anchored);
   `src/infra/relay_scheduler.rs`, `src/infra/relay_mux/stream.rs`.
6. 🔶 Partial — usage metering, max_nodes / max_streams / throughput-threshold
   admission landed (#84; `src/infra/relay_capacity.rs`, structured
   rejections at both register and open_stream; process-level e2e for the
   max_nodes admission rejection in the #145 capacity scenario). Load-test
   calibration — out of scope this phase (defaults published conservatively,
   revised after measurement).
7. ✅ Landed — configuration surface: relay.toml (#104; node side
   `src/infra/relay_toml_store.rs`, relay side `relay_serve_toml_store.rs`),
   remote-hosts `via` (#107 landed direct/relay; #156 slice 2 extended to
   the auto|direct|relay three-way choice with `last_via_used`
   effective-path annotation; `src/host/ssh/remote_host_history_store.rs`),
   Ctrl-W connection path integration (the via three-way choice and the
   "via auto → …" marker in
   `src/host/ssh/connect_remote_host_pane_runtime.rs`; dial goes through
   `remote_host_connect_runtime.rs`; auto probing and fallback in
   `src/infra/remote_grpc_transport.rs`).
   ✅ Landed (#156 slice 3) — fingerprint auto-discovery: the remote-hosts
   `tls_pin_sha256` may be omitted; connect uniquely hits the peer's
   fingerprint by host label via the node↔relay management channel's
   `resolve-node` (`announce` advertises the node's own hostname + egress IP
   after register, writing only its own connection-table entry; anchors
   `src/infra/relay_connection_table.rs`, `relay_remote_admin.rs`,
   `relay_client.rs`, `remote_host_connect_runtime.rs`). Process-level e2e:
   the e2e-relay.sh pinautodiscovery scenario (both the pin-less relay leg
   and the auto leg, with the on-disk pin == the peer's enrolled
   fingerprint).
8. ✅ Landed — structured error semantics (relay → node → console). Anchors:
   #111 (error codes finalized), #112 (propagation within a node), #113
   (console rendering); `src/infra/relay_routing/error_code.rs`,
   `src/ratatui_node/client_runtime.rs`. The console rendering surface
   (footer status line + inline relay-error marker) has no process-level e2e
   anchor — relay_error only renders in TUI snapshots, and the
   `__node-command` control plane skips push messages; an exemption is
   registered under the #136 mechanism (#145; the error-frame transport
   channel is covered by the #145 revoke scenario).
9. ✅ Landed — docker network-isolated e2e with two nodes + relay (#121
   harness; scenarios smoke / reconnect / reregister / streams / direct /
   pastefile, see `scripts/e2e/relay/e2e-relay.sh`), disconnect/reconnect
   (#124), multiple concurrent streams (#126), direct regression (#127),
   paste-file over the relay link (#128→#133); #137 reconciliation
   gap-filling (#145): revoke (remove revocation + transport-layer reconnect
   rejection + token unaffected), capacity (max_nodes admission rejection),
   presence (peer liveness → sidebar availability online/offline round
   trip); Windows CI (`ci.yaml` windows-check / windows-test run the full
   relay unit suite; docker scenarios are Linux-only by nature).
10. 🔶 Partial — WebUI: the web service enrolling as a special node (#134),
    read-only dashboard over the node channel (#135), magic-link
    authentication (#138), invite / remove write operations + CSRF + audit
    (#140), all landed 2026-10-05; e2e in `scripts/e2e/web/e2e-web.sh` (full
    invite + remove flow, including the process-level assertion that the
    removed node's relay link dies, #145). ✅ Landed (2026-10-06) —
    unified-start deployment form: `relay serve --web [--web-listen <addr>]`
    brings up the web plane in the same process (#142), fail-stop single
    Lifetime (if either half exits, the whole process exits; when the web
    half lacks deployment configuration, startup fails instead of running a
    headless relay); the web half still enrolls over loopback using the
    standard node↔relay protocol. Process-level e2e in
    `scripts/e2e/web/e2e-web.sh` phase 2. ⬜ Not implemented: in-browser
    terminal reusing OpenMirror/RawPty/Resize; one-line install command.

Compatibility commitments (issue #137 verification, 2026-10-05): when no
relay is configured, the relay client is not installed
(`src/ratatui_node/runtime.rs` establishes the long-lived connection only
when relay.toml exists); the direct dial moved into the seam is
byte-for-byte preserved in behavior (`src/infra/peer_connection.rs`), the
socks5 proxy path is untouched; all session protocols run on top of the seam
unchanged. Direct regression has docker e2e (the direct scenario in
`scripts/e2e/relay/e2e-relay.sh`).

## Phase 2 Outlook

- `PeerConnection` gains a P2P implementation: addresses are exchanged via
  the relay (rendezvous) and NAT traversal hole punching is performed; once
  successful, traffic no longer flows through the relay, which degrades to
  registry + rendezvous.
- relay cluster + admission scheduler: the scheduler allocates join /
  reconnect requests by `capacity − usage` based on each relay's capacity
  declarations and usage reports; scheduling decisions have a short TTL, and
  relay failures trigger rescheduling via node reconnects; the capacity view
  tolerates second-level staleness — the same trade-off as locally
  validating short-TTL billing credentials — and the cluster is not strongly
  consistent.
- Device identity becomes mandatory (the certificate fingerprint system is
  ready); the control plane implements billing.
