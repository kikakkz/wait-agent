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
