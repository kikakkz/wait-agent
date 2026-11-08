# WebUI Deployment and Security Configuration (issue #131)

`waitagent web serve` is the relay's WebUI process: a special node holding
its own node certificate that enrolls via the standard node↔relay protocol;
all management operations (status, invite, remove) reach the relay over its
authenticated long-lived connection — no in-process special case even on
the same machine (docs/relay-design.md management channel). The browser
side is server-rendered (askama), with zero JS framework/build chain.

## Startup Modes

- Unified startup (issue #142): `waitagent relay serve --web [--web-listen <addr>]`
  launches the web surface in the same process — fail-stop single Lifetime:
  if the relay exits the web exits, and if the web fails to start or crashes
  the relay exits too (when the web lacks deployment config it fails at
  startup instead of running a headless relay). The web side still enrolls
  via the loopback standard node↔relay protocol, with no in-process special
  case.
- Single-port dashboard (issue #209): with `relay serve --web`, the
  dashboard is also served on the relay's own public port (TLS + ALPN
  `h2`/`http/1.1`), i.e. `https://<relay>:7475/`. The relay uses a
  self-signed certificate, so browsers show a certificate warning (expected,
  accepted in #131); session cookies issued via logins on that port carry
  the `Secure` attribute. The standalone `--web-listen` HTTP listener is
  kept (with a deprecated notice) so the `public_base_url` in magic links
  and existing reverse proxies keep working.
- Self-service enrollment (issue #208): when `relay serve --web` is running
  with `webui.toml` configured, any device can initiate enrollment with
  `waitagent relay join-request <relay:port>` — the relay emails the admin
  a one-time magic link bound to that device's fingerprint; the admin
  forwards it to approve, and the applicant completes enrollment by running
  `waitagent relay join <relay> --link <link>`. `relay invite` remains
  reserved for the deployment flow (SSH auto-minting of tokens in Install
  as relay).
- Standalone startup: `waitagent web serve [--listen <addr>]` (default
  `0.0.0.0:8788`, the public-internet deployment mode; it prints an
  exposure warning at startup).
- Local-only use: `--listen 127.0.0.1:8788` loopback binding, no warning.
- Prerequisites: this machine has already done `relay join`
  (`~/.waitagent/relay.toml` exists), and `~/.waitagent/webui.toml` is
  configured (startup reports an error and guidance if missing).

## webui.toml (filled in once at deployment)

```toml
admin_email = "you@example.com"        # the sole admin email
mail_auth_code = "<auth code / app password>"
public_base_url = "https://dash.example.com"   # external address in magic links
# trusted_proxies = "127.0.0.1"        # only set behind a reverse proxy; empty default = direct connections only
# escape hatch for self-hosted mail servers on domains outside the table (all four keys appear as a group):
# mail_custom_host = "smtp.corp.example"
# mail_custom_port = 465
# mail_custom_tls = "ssl"              # ssl | starttls | none
# mail_custom_user = "you@corp.example"
```

The SMTP host/port/TLS are chosen from the built-in provider table based on
the email domain; the user only fills in the email + auth code (decided in
v4):

| Domain | SMTP host | Port/TLS |
|---|---|---|
| qq.com | smtp.qq.com | 465 SSL |
| 163.com | smtp.163.com | 465 SSL |
| hotmail.com / outlook.com | smtp.office365.com | 587 STARTTLS |
| gmail.com | smtp.gmail.com | 465 SSL |

Auth code instructions: QQ/163 mailboxes require generating a separate
"auth code" in the web settings (not the login password); Outlook/Gmail
require enabling 2FA first, then generating an app password. Known risk:
Microsoft is tightening basic auth; hotmail app passwords depend on tenant
policy. If they stop working, use `mail_custom_*` or wait for OAuth (no
OAuth in Phase 1).

## Login (magic link + device fingerprint)

1. The browser opens the dashboard → no session → enters an email on the
   login page. **Mail is sent only when the input == admin_email** (all
   other addresses get the same neutral response, to prevent
   spamming/enumeration); rate limit 3 attempts/10 min/IP.
2. The email contains `public_base_url + /auth/magic?token=...`. The token
   is an Ed25519-signed JWT (**the server key is an in-memory ephemeral
   Ed25519 keypair**, regenerated on every startup and never written to
   disk — issue #143; restarts naturally rotate it, and an old token that
   fails verification is simply treated as unauthenticated, so just go
   through the magic link again; there is no key file to back up, which
   dissolves the original request for a key-rotation command. Note the
   difference from relay node credentials: the node certificate fingerprint
   is bound to whitelist identity and must be persisted). Magic TTL is 10
   minutes, one-time (any redemption attempt consumes it).
3. Redemption validates the device fingerprint: SHA-256(client IP [real
   value taken via trusted_proxies] + User-Agent + JS probe
   platform/timezone/language/screen). **The same-machine constraint is
   literal**: the machine that requested the link == the machine that
   clicks it == the machine/browser/network for the entire session; any
   change is rejected (just do the magic link again). Heartbeat every 30s;
   a session is void after 90s without a heartbeat; absolute session
   lifetime is 12h. Restarting the process invalidates all sessions (stated
   explicitly in v3).

## Dashboard and Write Operations

- Read-only pages: relay listen/uptime, connection table
  (node_id/idle/online), usage/capacity; polled with a 10s `<meta refresh>`.
- **invite**: one-time (default) or deploy (checkbox, reusable until
  expiry), with an optional TTL; the resulting token is shown only once.
  The generated token reaches the relay over the node channel and is
  persisted to disk; the invited node enrolls as usual with
  `waitagent relay join <addr> <token>`.
- **remove**: requires entering a fingerprint (or unique prefix) to
  confirm; the relay deletes the whitelist entry and drops its online
  connection. Prefix resolution matches uniquely against the current
  connection table on the web side; any ambiguity is rejected.
- **CSRF**: all write POSTs carry a session-level CSRF token (planted into
  a template hidden field at login, verified in constant time); one more
  layer of cross-site defense on top of the fingerprint middleware.
- **Audit**: invite/remove are written to error_log (with built-in
  millisecond timestamps); the operator is recorded as the first 8
  characters of the session jti (view with `waitagent __error-log`).

## Reverse Proxy and Public Deployment

- When deploying behind a reverse proxy/tunnel, configure the proxy address
  in `trusted_proxies` so the fingerprinting and rate limiting see the real
  client IP; empty by default = direct connections only (X-Forwarded-For
  is ignored).
- Session cookies are `HttpOnly; SameSite=Strict`; there is no persisted
  session table (in-memory only).
- The emailed link requires the clicker's network to match the requester's
  (same NAT egress); tapping a link requested from a PC on mobile data is
  rejected by the fingerprint — this is an explicit, by-design trade-off.

## Emergency Channel Retained

The local admin socket (`relay status/invite/remove/shutdown --listen ...`)
is retained as the bootstrap and emergency channel, available alongside all
WebUI capabilities; **shutdown only goes through the local admin socket**
and is permanently refused on the node channel (a network-reachable
channel must not be able to stop the relay). When the CLI cannot reach a
running relay, it reports a clear error and guidance instead of implicitly
starting one.
