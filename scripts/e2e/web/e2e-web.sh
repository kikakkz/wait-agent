#!/bin/bash
# WebUI e2e (issue #131 slice 4; AGENTS.md hard constraint #6: new surfaces
# ship with process-level acceptance).
#
# Topology: all real processes on loopback, no docker — a real relay serve,
# a real web serve (special-node enrollment over the standard node<->relay
# protocol), and a scripted stub SMTP server that captures the magic link.
# The scenario walks the whole operator surface: unauthenticated redirect,
# magic-link login, CSRF-gated dashboard invite, and asserts the relay
# whitelist actually grows when a node redeems the minted token. The remove
# slice (issue #145) then revokes that node over the dashboard and asserts
# the whitelist shrinks and the revoked identity can no longer establish a
# relay link.
#
# Usage: e2e-web.sh
#   WA_E2E_BINARY=<path> overrides the binary (default: `cargo build` debug).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)

log() { printf '[e2e-web] %s\n' "$*"; }
die() { log "ERROR: $*"; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required (stub SMTP server)"
command -v jq >/dev/null 2>&1 || die "jq is required for status/probe parsing"
# The removed node's relay-link death is proven with a real node server; it
# initializes the TUI and needs a pty, which script(1) provides on loopback
# (the docker harness uses `docker run -t` for the same reason).
command -v script >/dev/null 2>&1 || die "script(1) is required (util-linux) for the pty"

if [ -n "${WA_E2E_BINARY:-}" ]; then
    [ -f "$WA_E2E_BINARY" ] || die "WA_E2E_BINARY not found: $WA_E2E_BINARY"
    BIN=$WA_E2E_BINARY
else
    log "building debug binary (set WA_E2E_BINARY to skip)"
    cargo build --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
    BIN=$REPO_ROOT/target/debug/waitagent
fi

RUN=${RUN:-$RANDOM$RANDOM}
export WAITAGENT_HOME=$(mktemp -d "/tmp/waitagent-e2e-web-$RUN.XXXXXX")
RELAY_PORT=$((18810 + RUN % 500))
WEB_PORT=$((19810 + RUN % 500))
SINK_PORT=$((17810 + RUN % 500))
# Loopback port for the removed node's real node server (pty via script(1)).
NODE2_PORT=$((20810 + RUN % 500))
UA="waitagent-e2e-web/1.0"
MAIL_FILE="$WAITAGENT_HOME/mail.txt"
NODE2_HOME=""
NODE2_SERVER_PID=""

RELAY_PID=""
WEB_PID=""
SINK_PID=""

cleanup() {
    [ -n "$WEB_PID" ] && kill "$WEB_PID" >/dev/null 2>&1 || true
    [ -n "$SINK_PID" ] && kill "$SINK_PID" >/dev/null 2>&1 || true
    if [ -n "$NODE2_SERVER_PID" ]; then
        kill "$NODE2_SERVER_PID" >/dev/null 2>&1 || true
        # script(1) may leave the shell/child behind; the port is unique to
        # this run, so a targeted pkill is safe.
        pkill -f -- "--port $NODE2_PORT" >/dev/null 2>&1 || true
    fi
    if [ -n "$RELAY_PID" ]; then
        "$BIN" relay shutdown --listen "127.0.0.1:$RELAY_PORT" >/dev/null 2>&1 || true
        wait "$RELAY_PID" 2>/dev/null || true
    fi
    [ -n "$NODE2_HOME" ] && rm -rf "$NODE2_HOME"
    rm -rf "$WAITAGENT_HOME"
}
trap cleanup EXIT INT TERM

wait_for_file() {
    local pattern=$1 file=$2 what=$3 tries=${4:-50}
    for _ in $(seq 1 "$tries"); do
        if grep -q "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    die "timed out waiting for $what (pattern '$pattern' in $file)"
}

log "starting relay on 127.0.0.1:$RELAY_PORT"
"$BIN" relay serve --listen "127.0.0.1:$RELAY_PORT" >"$WAITAGENT_HOME/relay.log" 2>&1 &
RELAY_PID=$!
wait_for_file "relay listening" "$WAITAGENT_HOME/relay.log" "the relay listener"

# Machine enrollment: the web process requires a pinned relay (relay.toml).
JOIN_TOKEN=$("$BIN" relay invite --listen "127.0.0.1:$RELAY_PORT" | sed -n 's/^token: //p')
[ -n "$JOIN_TOKEN" ] || die "relay invite produced no token"
"$BIN" relay join "127.0.0.1:$RELAY_PORT" "$JOIN_TOKEN" >/dev/null

# Stub SMTP server: speaks just enough ESMTP to hand the magic link over
# and records the DATA body. Plaintext is fine: loopback only, tests only.
python3 - "$SINK_PORT" "$MAIL_FILE" >"$WAITAGENT_HOME/sink.stdout" 2>"$WAITAGENT_HOME/sink.stderr" <<'PY' &
import socket, sys
port, outpath = int(sys.argv[1]), sys.argv[2]
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(8)
out = open(outpath, "a", buffering=1)
out.write("SINK READY\n")
def handle(conn):
    conn.sendall(b"220 stub ESMTP\r\n")
    f = conn.makefile("rb")
    state, body = "cmd", []
    def send(b): conn.sendall(b)
    while True:
        raw = f.readline()
        if raw == b"":
            return
        line = raw.decode(errors="replace").rstrip("\r\n")
        if state == "cmd":
            upper = line.upper()
            if upper.startswith("EHLO") or upper.startswith("HELO"):
                send(b"250-stub greets\r\n250 AUTH LOGIN\r\n")
            elif upper.startswith("AUTH LOGIN"):
                send(b"334 VXNlcm5hbWU6\r\n"); state = "au"
            elif upper.startswith(("MAIL FROM", "RCPT TO", "RSET", "NOOP")):
                send(b"250 ok\r\n")
            elif upper == "DATA":
                send(b"354 go\r\n"); state, body = "data", []
            elif upper.startswith("QUIT"):
                send(b"221 bye\r\n"); return
            elif upper.startswith("STARTTLS"):
                send(b"454 no tls\r\n")
            else:
                send(b"502 unimplemented\r\n")
        elif state == "au":
            send(b"334 UGFzc3dvcmQ6\r\n"); state = "ac"
        elif state == "ac":
            send(b"235 authenticated\r\n"); state = "cmd"
        elif state == "data":
            if line == ".":
                send(b"250 queued\r\n"); state = "cmd"
                out.write("=== MESSAGE ===\n" + "\n".join(body) + "\n")
            else:
                body.append(line[1:] if line.startswith("..") else line)
while True:
    conn, _ = srv.accept()
    try:
        handle(conn)
    except Exception as error:
        out.write("ERROR %r\n" % error)
    finally:
        conn.close()
PY
SINK_PID=$!
wait_for_file "SINK READY" "$MAIL_FILE" "the stub SMTP server"

cat > "$WAITAGENT_HOME/webui.toml" <<EOF
admin_email = "admin@example.com"
mail_auth_code = "e2e-auth-code"
public_base_url = "http://127.0.0.1:$WEB_PORT"
mail_custom_host = "127.0.0.1"
mail_custom_port = $SINK_PORT
mail_custom_tls = "none"
mail_custom_user = "admin@example.com"
EOF

log "starting web on 127.0.0.1:$WEB_PORT"
"$BIN" web serve --listen "127.0.0.1:$WEB_PORT" >"$WAITAGENT_HOME/web.log" 2>&1 &
WEB_PID=$!
wait_for_file "web listening" "$WAITAGENT_HOME/web.log" "the web listener"

BASE="http://127.0.0.1:$WEB_PORT"

# 1) Unauthenticated surface: dashboard redirects, healthz stays open.
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/")
[ "$code" = "303" ] || die "unauthenticated / should redirect (got $code)"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/healthz")
[ "$code" = "200" ] || die "/healthz must stay open (got $code)"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/login")
[ "$code" = "200" ] || die "/login must render (got $code)"

# 2) Magic-link login through the stub SMTP hop.
curl -s -A "$UA" -X POST "$BASE/auth/magic" \
    --data-urlencode "email=admin@example.com" \
    --data-urlencode "platform=Linux x86_64" \
    --data-urlencode "timezone=Asia/Shanghai" \
    --data-urlencode "language=en-US" \
    --data-urlencode "screen=1920x1080" >"$WAITAGENT_HOME/login-response.html"
wait_for_file "auth/magic?token=" "$MAIL_FILE" "the magic link mail"
MAGIC=$(grep -o "http://127.0.0.1:$WEB_PORT/auth/magic?token=[A-Za-z0-9._-]*" "$MAIL_FILE" | head -1)
[ -n "$MAGIC" ] || die "no magic link in the captured mail"
code=$(curl -s -c "$WAITAGENT_HOME/jar" -A "$UA" -o /dev/null -w '%{http_code}' "$MAGIC")
[ "$code" = "303" ] || die "magic redeem should redirect (got $code)"

# 3) The dashboard renders for the session (link registration can lag a
#    beat; poll like the meta-refresh browser would).
dashboard=""
for _ in $(seq 1 50); do
    dashboard=$(curl -s -b "$WAITAGENT_HOME/jar" -A "$UA" "$BASE/")
    case "$dashboard" in
        *"Connection table"*) break ;;
    esac
    sleep 0.2
done
case "$dashboard" in
    *"Connection table"*) ;;
    *) die "authenticated dashboard did not render" ;;
esac
CSRF=$(printf '%s' "$dashboard" | grep -o 'name="csrf" value="[^"]*"' | head -1 | sed 's/.*value="//; s/"$//')
[ -n "$CSRF" ] || die "dashboard rendered without a CSRF token"

# 4) CSRF gate: a write without the token is refused, session intact.
code=$(curl -s -b "$WAITAGENT_HOME/jar" -A "$UA" -o /dev/null -w '%{http_code}' \
    -X POST "$BASE/api/invite" --data "deploy=on")
[ "$code" = "403" ] || die "invite without CSRF must be 403 (got $code)"

# 5) Invite over the node channel: token shown once, relay whitelist grows
#    when a node redeems it.
mapfile -t whitelist_names_before < <(ls "$WAITAGENT_HOME/authorized_nodes" | sort)
whitelist_before=$(printf '%s\n' "${whitelist_names_before[@]}" | grep -c .)
invite_response=$(curl -s -b "$WAITAGENT_HOME/jar" -A "$UA" -X POST "$BASE/api/invite" \
    --data "csrf=$CSRF&ttl_secs=3600")
INVITE_TOKEN=$(printf '%s' "$invite_response" | grep -o 'token: [A-Za-z0-9_-]*' | head -1 | cut -d' ' -f2)
[ -n "$INVITE_TOKEN" ] || die "invite response carried no token"
[ "${#INVITE_TOKEN}" = "43" ] || die "invite token should be 43 chars, got ${#INVITE_TOKEN}"

# Node 2 joins with its own WAITAGENT_HOME (fresh node credentials there);
# `relay join` enrolls whatever identity that home holds.
NODE2_HOME=$(mktemp -d "/tmp/waitagent-e2e-web-node2-$RUN.XXXXXX")
WAITAGENT_HOME="$NODE2_HOME" "$BIN" relay join "127.0.0.1:$RELAY_PORT" "$INVITE_TOKEN" \
    >"$NODE2_HOME/join.out" 2>&1 \
    || { cat "$NODE2_HOME/join.out"; die "node 2 failed to join with the dashboard token"; }
whitelist_after=$(ls "$WAITAGENT_HOME/authorized_nodes" | wc -l)
if [ "$whitelist_after" -ne $((whitelist_before + 1)) ]; then
    log "join.out: $(cat "$NODE2_HOME/join.out")"
    log "invite token used: ${INVITE_TOKEN:0:12}... (len ${#INVITE_TOKEN})"
    log "authorized_nodes:"; ls -la "$WAITAGENT_HOME/authorized_nodes" | sed 's/^/  /'
    log "node2 join.out: $(cat "$NODE2_HOME/join.out")"
    log "relay log tail:"; tail -5 "$WAITAGENT_HOME/relay.log" | sed 's/^/  /'
    log "web log tail:"; tail -5 "$WAITAGENT_HOME/web.log" | sed 's/^/  /'
    die "whitelist did not grow ($whitelist_before -> $whitelist_after)"
fi
log "whitelist grew $whitelist_before -> $whitelist_after with the dashboard-minted token"

# Node 2's fingerprint: the whitelist entry that appeared with the invite.
mapfile -t whitelist_names_after < <(ls "$WAITAGENT_HOME/authorized_nodes" | sort)
node2_fp=$(comm -13 \
    <(printf '%s\n' "${whitelist_names_before[@]}") \
    <(printf '%s\n' "${whitelist_names_after[@]}"))
[ -n "$node2_fp" ] || die "could not derive node 2's fingerprint from the whitelist growth"

# 6) Bring node 2 fully online. Enrollment alone does not connect: the node
#    server (under a pty via script(1), mirroring the docker harness's
#    `docker run -t`) opens the relay link with the identity node 2
#    enrolled, so the relay's connection table — which the dashboard's
#    remove resolves fingerprints against — holds node 2.
WAITAGENT_HOME="$NODE2_HOME" script -qec "$BIN --port $NODE2_PORT" /dev/null \
    >"$NODE2_HOME/server.stdout" 2>&1 &
NODE2_SERVER_PID=$!
node2_ready=""
for _ in $(seq 1 150); do
    if WAITAGENT_HOME="$NODE2_HOME" "$BIN" __node-command "$NODE2_PORT" STATUS 2>/dev/null \
        | jq -e '.type == "Response" and .payload.ok == true' >/dev/null; then
        node2_ready=1
        break
    fi
    sleep 0.2
done
[ -n "$node2_ready" ] || { cat "$NODE2_HOME/server.stdout"; die "node 2's server never became ready"; }
node2_registered=""
for _ in $(seq 1 100); do
    if "$BIN" relay status --listen "127.0.0.1:$RELAY_PORT" 2>/dev/null \
        | jq -e --arg fp "$node2_fp" '.nodes[].node_id | select(. == $fp)' >/dev/null; then
        node2_registered=1
        break
    fi
    sleep 0.3
done
[ -n "$node2_registered" ] || { tail -10 "$WAITAGENT_HOME/relay.log"; die "node 2 never registered on the relay"; }
log "node 2's server is up and registered on the relay"

# 7) Remove over the dashboard (issue #145): the operator confirms by typing
#    the fingerprint (or a unique prefix), which resolves against a fresh
#    connection-table snapshot; the write is CSRF-gated exactly like invite.
# CSRF gate: a remove without the token is refused, session intact.
code=$(curl -s -b "$WAITAGENT_HOME/jar" -A "$UA" -o /dev/null -w '%{http_code}' \
    -X POST "$BASE/api/remove" --data "fingerprint=$node2_fp")
[ "$code" = "403" ] || die "remove without CSRF must be 403 (got $code)"

# Fresh dashboard render for a fresh CSRF token, then the prefix-confirmation
# happy path: 12 hex chars, as the dashboard's revoke button prefills. The
# banner carries the admin channel's answer.
dashboard=$(curl -s -b "$WAITAGENT_HOME/jar" -A "$UA" "$BASE/")
case "$dashboard" in
    *"Connection table"*) ;;
    *) die "dashboard did not render before remove" ;;
esac
CSRF=$(printf '%s' "$dashboard" | grep -o 'name="csrf" value="[^"]*"' | head -1 | sed 's/.*value="//; s/"$//')
[ -n "$CSRF" ] || die "dashboard rendered without a CSRF token"
remove_page=$(curl -s -b "$WAITAGENT_HOME/jar" -A "$UA" -X POST "$BASE/api/remove" \
    --data "csrf=$CSRF&fingerprint=${node2_fp:0:12}")
case "$remove_page" in
    *"removed:"*) ;;
    *) die "dashboard remove did not confirm: $(printf '%s' "$remove_page" | head -c 300)" ;;
esac

# Anchor (runtime truth): the connection table dropped node 2 and kept only
# the web node's link, undisturbed.
relay_status_json=$("$BIN" relay status --listen "127.0.0.1:$RELAY_PORT")
web_fp=$(jq -r --arg z "$node2_fp" '[.nodes[].node_id | select(. != $z)] | first' <<<"$relay_status_json")
[ -n "$web_fp" ] && [ "$web_fp" != "null" ] || die "no surviving node left in the connection table"
jq -e --arg fp "$web_fp" \
    '.registered_nodes == 1 and ([.nodes[].node_id] == [$fp])' <<<"$relay_status_json" >/dev/null \
    || die "relay connection table wrong after remove: $relay_status_json"
log "connection table shrank to the web node's link ($web_fp)"

# Anchor (enrollment truth): the whitelist shrank by exactly node 2's entry.
mapfile -t whitelist_names_final < <(ls "$WAITAGENT_HOME/authorized_nodes" | sort)
for fp in "${whitelist_names_final[@]}"; do
    [ "$fp" != "$node2_fp" ] || die "node 2's whitelist entry survived the dashboard remove"
done
[ "${#whitelist_names_final[@]}" = "$whitelist_before" ] \
    || die "whitelist should be back to $whitelist_before entries after remove, got ${#whitelist_names_final[@]}"
log "whitelist shrank $whitelist_after -> ${#whitelist_names_final[@]} (node 2 revoked)"

# Anchor (the revoked node's relay link is dead at the process level): the
# remove dropped node 2's live link with the NodeRevoked error frame, and
# every reconnect now fails the data port's whitelist client-auth, so the
# probe deterministically reports NotConnected while the local control
# socket stays healthy. The connection table must not regain node 2 no
# matter how long its client keeps retrying.
probe=$(WAITAGENT_HOME="$NODE2_HOME" "$BIN" __node-command "$NODE2_PORT" \
    "E2E_RELAY_PROBE $web_fp 1 1")
jq -e '.type == "Response" and .payload.ok == false' <<<"$probe" >/dev/null \
    || die "removed node 2 still has a working relay link: $probe"
relay_status_json=$("$BIN" relay status --listen "127.0.0.1:$RELAY_PORT")
jq -e --arg fp "$web_fp" \
    '.registered_nodes == 1 and ([.nodes[].node_id] == [$fp])' <<<"$relay_status_json" >/dev/null \
    || die "relay connection table changed after node 2's retries: $relay_status_json"
log "node 2's relay link stays dead (probe NotConnected, reconnect refused)"

log "OK: web e2e passed"
