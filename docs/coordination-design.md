# Coordination service design (issue #224, slice 1-3a)

The coordination service replaces the relay-era control plane: presence and
discovery, enrollment issuance, and the dashboard backend, plus an embedded
iroh relay (slice 1-3c). Sessions are iroh QUIC streams (slice 2). This
document covers the serve mode and the enrollment core.

## Identity model

- A node is an ed25519 keypair. The public key (z-base-32, the iroh node
  id) is the identity; registering it in the coordination registry makes the
  node authorized. No certificates.
- An operator is a separate ed25519 keypair with an administrative role.
  Authorized operator public keys are listed in `coordination.toml`; every
  admin request is signed by the operator key and verified against that
  list. The mapping from operator keys to the legacy
  `authorized_operators` concept is one authorized key per file-free entry
  in the config.

## Enrollment flow (two steps, admin in the loop)

1. `coordination enroll-request`: the node submits its public key over the
   node ALPN. The coordination stores a pending request and mails a magic
   link to the administrator mailbox. The link carries the request id and a
   one-time token (TTL 15 minutes by default, configurable).
2. The operator approves with `coordination approve <id>` over the admin
   ALPN (signed). Approval activates the token and prints a
   `waitagent://join?token=…` link for the operator to forward to the
   applicant out of band.
3. `coordination join --link <url-or-token>`: the node presents its public
   key and the token. The coordination verifies the request is approved,
   the token matches, is unexpired, unused, and bound to the presented key,
   then registers the key in the registry and marks the request joined.
   The token redeems exactly once.

A request lifecycle: `pending -> approved -> joined`, with `expired`
(time-based) and `revoked` terminal states. `coordination revoke <node-id>`
removes a node from the registry.

## ALPN layout

| ALPN | Surface | Frames |
|---|---|---|
| `waitagent/coordination/0` | node | one JSON request line, one JSON response line per bidi stream |
| `waitagent/coordination-admin/0` | operator admin | envelope `{body, operator, signature}`; `body` is a signed JSON string |

Admin commands: `requests`, `approve`, `revoke`. Node commands:
`enroll-request`, `join`. Presence is a reserved seam on the node ALPN and
is not implemented in this slice.

## Storage layout

Everything lives under `~/.waitagent/coordination/` (new files only):

- `coordination.toml` — `listen`, `admin_email`, `mail_auth_code`,
  `operators` (z-base-32 public keys), `token_ttl_secs`. Unknown keys are
  rejected.
- `requests/<id>.json` — one pending/active request per file:
  `{id, node_id, created_at, expires_at, token, state, joined_at}`.
- `registry/<node-id>.json` — one authorized node per file; presence is
  authorization, removal is revocation.

Directories are 0o700 and files 0o600 on unix, following the node key file
convention.

## SMTP

`coordination.toml` carries only the admin mailbox and an auth code. The
SMTP endpoint resolves from the mailbox domain through a data-driven preset
table (host/port/TLS per provider); adding a provider is adding a table row.
Send failures surface as explicit errors on the enroll-request path. For
evaluation without a mailbox, `coordination serve --admin-stdout` prints the
magic link to the terminal instead of sending mail.

## Client addressing

`--coordination` accepts the `node_addr` JSON line printed by the serve
mode, or `<node-id>@<host:port>`. The node remembers the coordination
address from its enroll-request (`coordination-server.json`) so
`coordination join --link …` needs no further addressing; `--coordination`
overrides it.

## TUI page (slice 1-3b, issue #246)

The Ctrl+W menu grows a fourth sidebar section, Coordination, mirroring
the relay page's layout: saved server profiles, a "+ New Coordination"
draft row, and a detail page. Design decisions:

- **Persistence.** Saved servers live in
  `~/.waitagent/coordination-servers.json` (0o600 via
  `write_private_file`), separate from the node's remembered
  enroll-target `coordination-server.json`: the profile list is
  operator-managed state, the remembered address is node-side flow
  state. Adding a server validates the address with
  `parse_coordination_addr` and refuses invalid input with an inline
  `Status::Error`; the profile name is the server node id's first 8
  chars.
- **New page vs saved page.** The New page offers Add (save only — the
  operator flow must not spam a self-enroll request) and Enroll (save,
  then enroll-request from the node key). The saved page offers Enroll,
  Approve (only while the local operator key is registered and a
  pending request is selected), Copy (only while a join link is minted),
  Join, and Refresh. Tab cycles the available actions; Left/Right button
  walking is not implemented on this page (Tab/Shift-Tab suffice).
- **Status polling.** A background thread ticks every 5 s while the
  popup is open and polls the *selected saved server* only: liveness is
  "any well-formed JSON reply" to a `ping` frame on the node ALPN (the
  server answers `unknown command`, which still proves a coordination
  server is there; the poll uses an ephemeral key so merely opening the
  page never writes key material). When the server is online, the poll
  also fetches `requests` on the admin ALPN with the operator key when
  one exists; `operator is not authorized` maps to operator=registered:
  false. Snapshots are tagged with the server address and applied only
  while that server is still selected, so a selection change mid-poll
  never paints another server's state. The serve side logs one error
  line per poll when the client closes without a stream — accepted
  noise for a connect-only probe.
- **New-request notification.** Applying a snapshot compares pending
  request ids against the previous list; a new id raises
  `Status::Hint("New join request arrived.")`. The first snapshot after
  selecting a server therefore surfaces all outstanding pending
  requests; repeats stay silent.
- **Join link.** Approve mints the link via the admin round trip and
  shows it on the page; Copy writes it to the clipboard (arboard). Join
  redeems a pasted magic link or bare 64-hex token from the Join Token
  field via the node ALPN `join` command. Approve/Enroll/Join/Refresh
  run inline on the UI thread behind the existing Working modal, like
  the relay join, reusing `round_trip_blocking` with its self-built
  runtime; the 5 s poll is the only background activity.

## Embedded relay, presence, and address distribution (slice 1-3c, issue #253)

- **Relay server shape.** iroh 1.4.1 ships the relay as the `iroh-relay` crate
  (`server` feature): `iroh_relay::server::Server::spawn` with a
  `RelayConfig::new(bind_addr)` runs a plaintext-HTTP relay (WebSocket relay
  protocol v2). TLS terminates on the same HTTP server via `RelayConfig.tls`
  and is deliberately deferred to the 1-3d dashboard work; local evaluation
  uses `http://` relay URLs, which iroh clients accept (scheme selects `ws://`
  vs `wss://`). The QUIC server component (`ServerConfig.quic`) only adds QUIC
  address discovery and requires TLS certificates, so it stays disabled.
- **Port convention.** The embedded relay binds TCP on the same port number as
  the QUIC endpoint (default 7479), satisfying the single-port requirement. A
  bind/spawn failure is logged and downgraded to "no relay" — coordination
  stays usable, the relay URL is simply not announced. `coordination.toml`
  `relay = false` disables it; `relay_url = "…"` overrides the announced URL
  (needed behind NAT or with a DNS name).
- **Announcement.** The serve process prints `relay_url:` and every node-ALPN
  response (enroll-request, join, heartbeat) carries `relay_url`. The node CLI
  persists it to `~/.waitagent/coordination-relay.json` so `coordination
  heartbeat` and the future agent runtime can include it in their published
  address without re-enrolling.
- **Presence.** The `PresenceStore` is in-memory on purpose: presence is
  ephemeral and rebuilds from heartbeats after a server restart. A heartbeat is
  accepted only when the claimed `node_id` equals the connection-authenticated
  identity (`conn.remote_id()`) and the node is in the registry; the
  connection-observed direct address is merged into the reported NodeAddr.
  `presence_timeout_secs` (default 45 s, three missed 15 s heartbeats) bounds
  the online list; `revoke` drops the presence entry with the registry entry.
  `coordination heartbeat` sends one shot (for timers) or loops with `--every`.
- **Resolve.** `resolve <node-id>` requires both the requester and the target
  to be registered, and returns the target's last-known NodeAddr in the
  mesh-probe wire format (`node_id`, `relay_url`, `direct_addresses`) plus
  `last_seen`; callers decide whether a stale address is worth dialing. This is
  the slice-2 data-plane addressing seam.
- **mesh-probe relay mode.** `--relay <url>` switches both listen and dial to
  `RelayMode::Custom` with exactly one relay; without the flag behavior is
  unchanged. `listen` waits for the relay URL to appear in its published
  address, and `dial` echoes the active relay URL in its JSON report. Relay
  URLs are compared normalized (`url::Url` semantics), so a trailing slash is
  insignificant.
- **Usage note for slice 2 (hard-won in loopback testing):** an iroh 1.4
  endpoint answers relayed QUIC handshakes only while `accept()` is actively
  polled — a listener that has not entered its accept loop silently drops the
  dial. Accept-first is mandatory, not cosmetic.

## Dashboard backend (slice 1-3d, issue #260; frontend is 1-3e/#261)

- **Single-port dispatch.** `coordination serve` no longer uses
  `iroh_relay::server::Server::spawn` (which owns its listener and cannot
  share a port). One hyper server (`hyper-util` auto connection builder with
  upgrades enabled, mandatory for the relay WebSocket) listens on the same
  TCP port as the QUIC UDP socket; a `tower` dispatch service routes the
  exact `GET /relay` path to an embedded `RelayServiceWithNotify` and
  everything else to an axum 0.8 router served directly over
  `Request<Incoming>`. `relay = false` drops only the relay arm — the
  dashboard keeps serving. `/ping` is answered by axum (any 2xx) because
  net_report measures relay latency with a plain HTTPS GET.
- **RelayService embedding.** `RelayService::new(Handlers::default(), empty
  headers, no rate limit, KeyCache::new(DEFAULT_KEY_CACHE_CAPACITY),
  AllowAll, Metrics::default())` wrapped in `RelayServiceWithNotify`; the
  relay upgrade is hardcoded on the exact `/relay` path, so an empty handler
  map loses nothing (the built-in `/healthz` is superseded by dashboard
  endpoints). No private iroh-relay APIs are used.
- **No web-auth.key on disk.** Sessions are opaque 256-bit tokens (the same
  `mint_token` CSPRNG the enrollment tokens use) with every claim kept in a
  server-side in-memory store; a restart invalidates everything and a fresh
  magic link is the recovery path. There is nothing to rotate because nothing
  is persisted.
- **Magic-link login.** `login-request` accepts only the configured
  `admin_email` (a non-admin mailbox gets the same response and no link, so
  nothing is enumerable), enforces a 60 s per-IP cooldown, and mails the
  15-minute one-time link through the 1-3a SMTP preset flow; `--admin-stdout`
  prints `login_link:` like the enroll link. Tokens are bound at request time
  to request IP + User-Agent + a client-collected fingerprint, and redeeming
  fails closed (403) on any mismatch, expiry, prior use, or unknown token.
  Success marks the link used, creates a session (absolute TTL 12 h), sets an
  HttpOnly SameSite=Strict cookie, and redirects to `/dashboard/`.
- **Heartbeat and expiry.** Clients heartbeat every 30 s; a session idle for
  more than 90 s is rejected at the auth gate and reaped by a 30 s sweep,
  alongside the absolute 12 h TTL.
- **CSRF.** Mutations (`heartbeat`, `approve`, `revoke`) require an
  `X-CSRF-Token` header matching the per-session token, handed out in the
  `X-CSRF-Token` response header of authenticated responses.
- **Admin semantics equivalence.** Approve/revoke/requests go through the same
  `RequestStore`/`RegistryStore`/`PresenceStore` methods the admin-ALPN
  handlers call in the same process, so dashboard, TUI page, and CLI agree by
  construction; approve returns the same `waitagent://join?token=…` format as
  the CLI. The serve process acts under its own operator key: an empty
  `operators` list auto-registers it (logged, in memory only); a non-empty
  list without the key makes dashboard admin endpoints answer 409 with the
  listing instruction until the admin adds it.
