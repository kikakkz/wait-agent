#!/bin/bash
# Coordination relay/presence e2e (issue #253, AGENTS.md constraint #6): loopback-only.
# A real coordination serve process runs its embedded iroh relay on the same port
# (TCP), announces the relay URL through the node ALPN, tracks presence from node
# heartbeats, resolves authorized node addresses, and a mesh-probe pair connects
# through the relay with a bogus direct address so only the relay path can succeed.
#
# Usage: e2e-coordination-relay.sh
#   WA_E2E_BINARY=<path> overrides the binary (default: `cargo build` debug).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)

log() { printf '[e2e-coordination-relay] %s\n' "$*"; }
die() { log "ERROR: $*"; exit 1; }

command -v jq >/dev/null 2>&1 || die "jq is required"

if [ -n "${WA_E2E_BINARY:-}" ]; then
    [ -f "$WA_E2E_BINARY" ] || die "WA_E2E_BINARY not found: $WA_E2E_BINARY"
    BIN=$WA_E2E_BINARY
else
    log "building debug binary (set WA_E2E_BINARY to skip)"
    cargo build --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
    BIN=$REPO_ROOT/target/debug/waitagent
fi

RUN=${RUN:-$RANDOM$RANDOM}
export WAITAGENT_HOME=$(mktemp -d "/tmp/waitagent-e2e-coordrelay-$RUN.XXXXXX")
PORT=$((22010 + RUN % 500))
LISTEN="127.0.0.1:$PORT"
SERVE_PID=""
PROBE_PID=""

cleanup() {
    [ -n "$PROBE_PID" ] && kill "$PROBE_PID" >/dev/null 2>&1 || true
    [ -n "$SERVE_PID" ] && kill "$SERVE_PID" >/dev/null 2>&1 || true
    rm -rf "$WAITAGENT_HOME"
}
trap cleanup EXIT INT TERM

wait_for_file() {
    local pattern=$1 file=$2 what=$3
    for _ in $(seq 1 75); do
        if grep -q "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    die "timed out waiting for $what (pattern '$pattern' in $file)"
}

log "isolating state under $WAITAGENT_HOME"

log "bootstrapping the operator key"
OP_OUT=$("$BIN" coordination requests --coordination "$LISTEN" 2>&1 || true)
OPERATOR=$(printf '%s\n' "$OP_OUT" | sed -n 's/.*(pubkey \([^)]*\)).*/\1/p' | head -1)
[ -n "$OPERATOR" ] || die "could not learn the operator pubkey from: $OP_OUT"
mkdir -p "$WAITAGENT_HOME/coordination"
printf 'operators = ["%s"]\n' "$OPERATOR" > "$WAITAGENT_HOME/coordination/coordination.toml"

log "starting coordination serve on $LISTEN (embedded iroh relay on the same port)"
"$BIN" coordination serve --listen "$LISTEN" --admin-stdout \
    >"$WAITAGENT_HOME/serve.log" 2>&1 &
SERVE_PID=$!
wait_for_file "coordination listening" "$WAITAGENT_HOME/serve.log" "the serve mode"

NODE_ID=$(sed -n 's/^node_id: //p' "$WAITAGENT_HOME/serve.log")
NODE_ADDR=$(sed -n 's/^node_addr: //p' "$WAITAGENT_HOME/serve.log")
RELAY_URL=$(sed -n 's/^relay_url: //p' "$WAITAGENT_HOME/serve.log")
[ -n "$NODE_ID" ] && [ -n "$NODE_ADDR" ] || die "serve did not print its node_addr"
[ -n "$RELAY_URL" ] || die "serve did not announce the embedded relay url"
case "$RELAY_URL" in
    "http://127.0.0.1:$PORT/") ;;
    *) die "unexpected relay url (want the embedded relay on the serve port): $RELAY_URL" ;;
esac
log "embedded relay announced at $RELAY_URL"

log "enroll-request from a fresh node"
ENROLL_OUT=$("$BIN" coordination enroll-request --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$ENROLL_OUT"
    die "enroll-request failed"
}
printf '%s\n' "$ENROLL_OUT"
printf '%s\n' "$ENROLL_OUT" | grep -q "\"relay_url\":\"$RELAY_URL\"" \
    || die "enroll-request did not announce the relay url: $ENROLL_OUT"
NODE_KEYLINE=$(printf '%s\n' "$ENROLL_OUT" | sed -n 's/.*(pubkey \([^)]*\)).*/\1/p' | head -1)
[ -n "$NODE_KEYLINE" ] || die "enroll-request did not print the node key notice"
REQUEST_ID=$(printf '%s\n' "$ENROLL_OUT" | grep -o '"request_id":"[^"]*"' | head -1 | cut -d'"' -f4)
[ -n "$REQUEST_ID" ] || die "enroll-request produced no request id"

log "operator approve $REQUEST_ID"
APPROVE_OUT=$("$BIN" coordination approve "$REQUEST_ID" --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$APPROVE_OUT"
    die "approve failed"
}
JOIN_LINK=$(printf '%s\n' "$APPROVE_OUT" | sed -n 's/^join_link: //p' | tail -1)
[ -n "$JOIN_LINK" ] || die "approve did not print the join link"

log "join (also saves the announced relay url and heartbeats once)"
JOIN_OUT=$("$BIN" coordination join --link "$JOIN_LINK" 2>&1) || {
    printf '%s\n' "$JOIN_OUT"
    die "join failed"
}
printf '%s\n' "$JOIN_OUT"
[ -f "$WAITAGENT_HOME/coordination-relay.json" ] \
    || die "the announced relay url was not saved for the node"
[ "$(cat "$WAITAGENT_HOME/coordination-relay.json")" = "$RELAY_URL" ] \
    || die "saved relay url mismatch: $(cat "$WAITAGENT_HOME/coordination-relay.json")"

log "one-shot heartbeat registers presence"
HEARTBEAT_OUT=$("$BIN" coordination heartbeat --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$HEARTBEAT_OUT"
    die "heartbeat failed"
}
printf '%s\n' "$HEARTBEAT_OUT" | grep -q '"ok":true' || die "heartbeat was rejected: $HEARTBEAT_OUT"

log "operator presence lists the node with the announced relay url"
PRESENCE_OUT=$("$BIN" coordination presence --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$PRESENCE_OUT"
    die "presence failed"
}
printf '%s\n' "$PRESENCE_OUT"
printf '%s\n' "$PRESENCE_OUT" | jq -e --arg id "$NODE_KEYLINE" --arg relay "$RELAY_URL" \
    '.nodes[] | select(.node_id == $id and .addr.relay_url == $relay)' >/dev/null \
    || die "presence does not list $NODE_KEYLINE with the relay url: $PRESENCE_OUT"

log "an unregistered node cannot resolve"
STRANGER_HOME=$(mktemp -d "/tmp/waitagent-e2e-stranger-$RUN.XXXXXX")
STRANGER_OUT=$(WAITAGENT_HOME="$STRANGER_HOME" "$BIN" coordination resolve "$NODE_KEYLINE" \
    --coordination "$NODE_ADDR" 2>&1 || true)
printf '%s\n' "$STRANGER_OUT" | grep -q '"ok":false' \
    || die "an unregistered node resolved an address: $STRANGER_OUT"
rm -rf "$STRANGER_HOME"

log "the registered node resolves its own address"
RESOLVE_OUT=$("$BIN" coordination resolve "$NODE_KEYLINE" --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$RESOLVE_OUT"
    die "resolve failed"
}
printf '%s\n' "$RESOLVE_OUT"
RESOLVED_ADDR=$(printf '%s\n' "$RESOLVE_OUT" | jq -r '.node_addr')
[ -n "$RESOLVED_ADDR" ] && [ "$RESOLVED_ADDR" != "null" ] || die "resolve returned no node_addr"
printf '%s\n' "$RESOLVED_ADDR" | jq -e --arg relay "$RELAY_URL" '.relay_url == $relay' >/dev/null \
    || die "resolved address lacks the relay url: $RESOLVED_ADDR"

log "mesh-probe listener publishes a relay url under --relay"
"$BIN" mesh-probe listen --relay "$RELAY_URL" \
    >"$WAITAGENT_HOME/probe-listen.log" 2>&1 &
PROBE_PID=$!
wait_for_file '"relay_url"' "$WAITAGENT_HOME/probe-listen.log" "the mesh listener addr"
PROBE_ID=$(sed -n 's/^node_id: //p' "$WAITAGENT_HOME/probe-listen.log")
PROBE_ADDR=$(sed -n 's/^node_addr: //p' "$WAITAGENT_HOME/probe-listen.log")
[ -n "$PROBE_ID" ] && [ -n "$PROBE_ADDR" ] || die "mesh listener did not print its node_addr"
printf '%s\n' "$PROBE_ADDR" | jq -e --arg relay "$RELAY_URL" '.relay_url == $relay' >/dev/null \
    || die "mesh listener addr lacks the relay url: $PROBE_ADDR"

log "dial through the relay only (bogus direct address, --relay on the dialer)"
DIAL_ADDR=$(printf '%s\n' "$PROBE_ADDR" | jq -c '.direct_addresses = ["127.0.0.1:9"]')
DIAL_OUT=$(timeout 60 "$BIN" mesh-probe dial --key-file "$WAITAGENT_HOME/mesh-dialer.key" \
    --node-id "$PROBE_ID" --addr "$DIAL_ADDR" \
    --relay "$RELAY_URL" --seconds 2 2>&1) || {
    printf '%s\n' "$DIAL_OUT"
    die "relay dial failed"
}
printf '%s\n' "$DIAL_OUT"
printf '%s\n' "$DIAL_OUT" | jq -e '.ok == true and .relay == "'"$RELAY_URL"'"' >/dev/null \
    || die "dial did not report the relay mode: $DIAL_OUT"
printf '%s\n' "$DIAL_OUT" | jq -e '.connect_ms != null and .echo_mib_per_sec != null' >/dev/null \
    || die "dial did not report the probe measurements: $DIAL_OUT"

log "OK: coordination relay/presence e2e passed"
