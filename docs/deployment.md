# Relay Deployment and Connection Operations Manual

Operational manual for public relay deployment → node enrollment → day-to-day
management. Design decisions and protocol details are in
[relay-design.md](relay-design.md); WebUI security configuration, the
magic-link authentication mechanism, and reverse-proxy deployment are in
[webui.md](webui.md). This document covers only operational steps and behavior
that has landed; it does not repeat design rationale.

## A. Public relay host

### Starting the relay

```console
$ waitagent relay serve --listen 0.0.0.0:7475
```

- `--listen` defaults to `0.0.0.0:7475`; `--authorized-nodes <dir>` overrides
  the whitelist directory (default `~/.waitagent/authorized_nodes/`).
- Firewall: only the relay listen port needs to be open (default 7475/TCP).
  All nodes connect to the relay via outbound connections; no inbound port is
  needed on the node side.
- The relay's on-disk state is only `relay.toml`, the whitelist directory, and
  the token store; it persists no session or traffic data.

### Generating an invite token (run on the relay host)

```console
$ waitagent relay invite                 # one-time token, default TTL 15 minutes
$ waitagent relay invite --deploy        # reusable for mass provisioning, default TTL 30 days
$ waitagent relay invite --ttl 3600      # TTL override in seconds
```

- After generation the token is copied to the clipboard on a best-effort basis
  (on hosts without a clipboard, a message is printed asking you to copy it
  manually).
- A one-time token is consumed by any redemption attempt — including rejected
  ones (fail-closed: a leaked token cannot be replayed); a deploy token can be
  reused until it expires.
- Token state is persisted on the relay side; restarting the relay does not
  lose unexpired tokens.

### Running the WebUI in the same process (optional, recommended)

```console
$ waitagent relay serve --web --web-listen 0.0.0.0:8788
```

- `--web` brings up the web surface in the same process (listening on
  `0.0.0.0:8788` by default); single fail-stop Lifetime: if either half exits,
  the whole process exits.
- Prerequisites: this host has already run `relay join` (so
  `~/.waitagent/relay.toml` exists) and `~/.waitagent/webui.toml` is
  configured — if either is missing, startup fails immediately rather than
  running a headless relay.

Fill in `webui.toml` once at deployment time (full details in
[webui.md](webui.md)):

```toml
admin_email = "you@example.com"              # the sole admin email
mail_auth_code = "<auth code / app password>"
public_base_url = "https://dash.example.com" # external address used in magic links
# trusted_proxies = "127.0.0.1"              # only for reverse-proxy setups; empty default = direct connections only
```

- SMTP host/port/TLS are selected automatically from a built-in provider table
  based on the email domain; the user only enters the email address + auth
  code. How to obtain the auth code: QQ/163 mail generate a separate "auth
  code" in the web settings (it is not the login password);
  Outlook(Hotmail)/Gmail require enabling 2FA first, then generating an app
  password.
- `trusted_proxies`: in reverse-proxy or tunnel deployments, add the proxy
  address here so device fingerprinting and rate limiting can see the real
  client IP; empty by default = direct connections only, and
  `X-Forwarded-For` is ignored.

## B. Node enrollment

```console
$ waitagent relay join <relay-address> <token>   # address is host[:port], default port 7475
```

A single join completes bidirectional binding (identity authentication and
enrollment in [relay-design.md](relay-design.md)):

1. The token authenticates the enrollment session — a party without a token
   cannot establish a session, so there is no MitM window;
2. Once the session is established, the relay sends its own certificate
   fingerprint over this authenticated channel, and the node writes it to
   `~/.waitagent/relay.toml` (`address` + `relay_fingerprint` — this is the
   "pinning");
3. At the same time, the relay automatically adds the node's certificate
   fingerprint to the whitelist. No fingerprint is ever copied by hand.

**What per-fingerprint accounting for tokens means**: device identity = the
SHA-256 SPKI fingerprint of the node's self-signed certificate
(`~/.waitagent/node.crt`); the whitelist creates one entry per fingerprint.
Moving to a new machine or reinstalling generates a new certificate and thus a
new fingerprint — the new fingerprint is not in the whitelist, so the register
call is rejected at the transport layer (mTLS fingerprint check), and an old
token does not cover it. Therefore, after a machine change or reinstall you
must run `relay invite` again for a new token and then `relay join`; clean up
the fingerprint entry of a retired device with
`waitagent relay remove <fingerprint>`.

## C. TUI zero-command-line path

The full flow without typing commands (Ctrl-W popup, a combination of three
landed slices):

1. **Add relay** (issue #156 slice 1, PR #157): in the Relay section of the
   Ctrl-W connection popup, fill in the address + invite token; it goes
   through exactly the same join engine as `relay join`. If the new relay
   fingerprint does not match the old pin being replaced, the operation is
   rejected by default and the old pin is restored; after confirming the
   relay change, click "Switch anyway" to resend with FORCE. Remove clears
   `relay.toml`, stops the relay long connection, and resets relay state.
2. **Add host with via auto** (issue #156 slice 2, PR #158): the Via field on
   the Connection card offers Auto/Direct/Relay, with auto as the default.
   auto = direct first, relay as fallback: connect first probes direct with a
   short timeout cap (TCP + pin handshake), and dials via the relay on
   failure. The path that took effect is recorded in `last_via_used`, and the
   host header is annotated "via auto → relay"; the user's choice is never
   silently rewritten.
3. **Automatic fingerprint discovery** (issue #156 slice 3, PR #159): the
   `tls_pin_sha256` field of a remote-hosts entry can be omitted. On connect,
   the `resolve-node` call over the node↔relay management channel matches the
   peer's registered fingerprint uniquely by host label (the hostname +
   egress IP the node announces automatically after registering); a match is
   persisted with the profile and reused next time. On no match or multiple
   matches, the flow falls back to the legacy SSH bootstrap path; entries with
   explicit direct do not query the relay.

## D. Management surface

- **Dashboard** ([webui.md](webui.md)): open `public_base_url` in a browser →
  enter `admin_email` on the login page → receive a magic link by email
  (TTL 10 minutes, one-time; redemption checks the device fingerprint — the
  request, the click, and the session must all be from the same machine on the
  same network). The signing key is an in-memory ephemeral Ed25519 keypair;
  restarting the process invalidates all sessions, after which you simply go
  through the magic link again. After login: a read-only dashboard (relay
  listen/uptime, connection table, usage/capacity, polled every 10s), invite
  (one-time or deploy, optional TTL, token shown only once), remove (enter
  the fingerprint to confirm; deletes the whitelist entry and disconnects the
  online link).
- **CLI** (local admin socket, the bootstrap and emergency channel):
  `waitagent relay status`, `waitagent relay invite [--deploy] [--ttl]`,
  `waitagent relay remove <fingerprint>`. `relay shutdown` only goes through
  the local admin socket — a network-reachable channel must not be able to
  stop the relay. When the CLI cannot reach a running relay, it reports an
  explicit error with guidance and does not start anything implicitly.

## E. Known limitations

- **Browser terminal not implemented**: a browser terminal reusing
  OpenMirror/RawPty/Resize inside the WebUI is an explicit non-goal of relay
  Phase 1 (relay-design.md task 10 ⬜); session access currently goes through
  the TUI only.
- **First public operation**: all process-level relay e2e tests run inside a
  docker-isolated bridge topology (`scripts/e2e/relay/e2e-relay.sh`,
  `scripts/e2e/web/e2e-web.sh`); there is no record yet of a first
  real-machine deployment of a public relay. Capacity defaults were released
  conservatively and will be revised from measurements on your own host after
  the first public deployment.
- **Windows host e2e not covered**: Windows CI runs the full relay unit test
  suite, while the docker e2e scenarios are inherently Linux-only; the relay
  deployment path on Windows hosts has no process-level e2e anchor.
- **Microsoft basic-auth risk**: hotmail/outlook app passwords depend on
  tenant policy, and Microsoft is tightening basic auth; when the auth code
  stops working, the escape hatch is the `mail_custom_*` four-key
  bring-your-own mail server in `webui.toml` (no OAuth in Phase 1).
