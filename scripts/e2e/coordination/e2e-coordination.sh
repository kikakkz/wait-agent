#!/bin/bash
# Coordination e2e (issue #236, AGENTS.md constraint #6): loopback-only, no
# docker. A real coordination serve process plus node/operator CLI processes
# walk the full enrollment flow: enroll-request -> magic link (--admin-stdout)
# -> operator approve -> join -> registry entry, plus the one-time token rule.
#
# Usage: e2e-coordination.sh
#   WA_E2E_BINARY=<path> overrides the binary (default: `cargo build` debug).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)

log() { printf '[e2e-coordination] %s\n' "$*"; }
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
export WAITAGENT_HOME=$(mktemp -d "/tmp/waitagent-e2e-coordination-$RUN.XXXXXX")
PORT=$((21010 + RUN % 500))
LISTEN="127.0.0.1:$PORT"
SERVE_PID=""

cleanup() {
    [ -n "$SERVE_PID" ] && kill "$SERVE_PID" >/dev/null 2>&1 || true
    rm -rf "$WAITAGENT_HOME"
}
trap cleanup EXIT INT TERM

wait_for_file() {
    local pattern=$1 file=$2 what=$3
    for _ in $(seq 1 50); do
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

log "starting coordination serve on $LISTEN (--admin-stdout)"
"$BIN" coordination serve --listen "$LISTEN" --admin-stdout \
    >"$WAITAGENT_HOME/serve.log" 2>&1 &
SERVE_PID=$!
wait_for_file "coordination listening" "$WAITAGENT_HOME/serve.log" "the serve mode"

NODE_ID=$(sed -n 's/^node_id: //p' "$WAITAGENT_HOME/serve.log")
NODE_ADDR=$(sed -n 's/^node_addr: //p' "$WAITAGENT_HOME/serve.log")
[ -n "$NODE_ID" ] && [ -n "$NODE_ADDR" ] || die "serve did not print its node_addr"

log "enroll-request from a fresh node"
ENROLL_OUT=$("$BIN" coordination enroll-request --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$ENROLL_OUT"
    die "enroll-request failed"
}
printf '%s\n' "$ENROLL_OUT"
NODE_KEYLINE=$(printf '%s\n' "$ENROLL_OUT" | sed -n 's/.*(pubkey \([^)]*\)).*/\1/p' | head -1)
[ -n "$NODE_KEYLINE" ] || die "enroll-request did not print the node key notice"
REQUEST_ID=$(printf '%s\n' "$ENROLL_OUT" | grep -o '"request_id":"[^"]*"' | head -1 | cut -d'"' -f4)
[ -n "$REQUEST_ID" ] || die "enroll-request produced no request id"

wait_for_file "magic_link: waitagent://join?token=" "$WAITAGENT_HOME/serve.log" "the admin-stdout magic link"
MAGIC=$(sed -n 's/^magic_link: waitagent:\/\/join?token=//p' "$WAITAGENT_HOME/serve.log" | tail -1)
[ -n "$MAGIC" ] || die "no magic link in the serve log"

log "join before approval must fail"
if "$BIN" coordination join --link "$MAGIC" >/dev/null 2>&1; then
    die "join redeemed before approval"
fi

log "operator approve $REQUEST_ID"
APPROVE_OUT=$("$BIN" coordination approve "$REQUEST_ID" --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$APPROVE_OUT"
    die "approve failed"
}
printf '%s\n' "$APPROVE_OUT"
JOIN_LINK=$(printf '%s\n' "$APPROVE_OUT" | sed -n 's/^join_link: //p' | tail -1)
[ -n "$JOIN_LINK" ] || die "approve did not print the join link"
case "$JOIN_LINK" in
    *"token=$MAGIC") ;;
    *) die "approve link token does not match the magic link" ;;
esac

log "operator requests lists the approved request"
REQ_OUT=$("$BIN" coordination requests --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$REQ_OUT"
    die "requests failed"
}
printf '%s\n' "$REQ_OUT" | grep -q "\"id\":\"$REQUEST_ID\"" || die "requests does not list $REQUEST_ID: $REQ_OUT"
printf '%s\n' "$REQ_OUT" | grep -q '"state":"approved"' || die "request is not approved: $REQ_OUT"

log "join with the forwarded link"
JOIN_OUT=$("$BIN" coordination join --link "$JOIN_LINK" 2>&1) || {
    printf '%s\n' "$JOIN_OUT"
    die "join failed"
}
printf '%s\n' "$JOIN_OUT"

log "the registry carries the node public key"
[ -f "$WAITAGENT_HOME/coordination/registry/$NODE_KEYLINE.json" ] \
    || die "registry file for $NODE_KEYLINE is missing"

log "the token redeems exactly once"
if "$BIN" coordination join --link "$JOIN_LINK" >/dev/null 2>&1; then
    die "token replay succeeded"
fi

log "operator revoke removes the node"
REV_OUT=$("$BIN" coordination revoke "$NODE_KEYLINE" --coordination "$NODE_ADDR" 2>&1) || {
    printf '%s\n' "$REV_OUT"
    die "revoke failed"
}
printf '%s\n' "$REV_OUT"
[ ! -f "$WAITAGENT_HOME/coordination/registry/$NODE_KEYLINE.json" ] \
    || die "registry file survived the revoke"

log "OK: coordination e2e passed"
