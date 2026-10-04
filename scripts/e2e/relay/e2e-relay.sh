#!/bin/bash
# Relay docker e2e harness (issue #37).
#
# Topology: one relay container attached to TWO isolated bridges; node A only
# to the first, node B only to the second. A and B have no network path to
# each other by construction — every scenario exercises the relay as the
# only route, which is the acceptance criterion's "no LAN shortcuts".
#
# Images: by default the full Dockerfile builds the release binary inside
# docker (works on any host with docker). Set WA_E2E_BINARY to a prebuilt
# host binary to skip the in-docker build (CI and quick local runs).
#
# Usage: e2e-relay.sh [scenario ...]
#   scenarios: smoke (default), reconnect, reregister
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)
IMAGE=waitagent-e2e:local
RELAY_LISTEN=0.0.0.0:7475
RELAY_PORT=7475
NODE_PORT_A=9001
NODE_PORT_B=9002
GATE_TIMEOUT_SECS=${GATE_TIMEOUT_SECS:-120}

log() { printf '[e2e-relay] %s\n' "$*"; }
die() {
    log "ERROR: $*"
    log "relay logs (tail):"
    docker logs --tail 20 "$RELAY" 2>&1 | sed 's/^/  /' || true
    log "node-a logs (tail):"
    docker logs --tail 20 "$NODE_A" 2>&1 | sed 's/^/  /' || true
    log "node-b logs (tail):"
    docker logs --tail 20 "$NODE_B" 2>&1 | sed 's/^/  /' || true
    exit 1
}

RUN=${RUN:-$RANDOM$RANDOM}
RELAY="wa37-relay-$RUN"
NODE_A="wa37-node-a-$RUN"
NODE_B="wa37-node-b-$RUN"
NET_A="wa37-net-a-$RUN"
NET_B="wa37-net-b-$RUN"
FINGERPRINTS=()

command -v docker >/dev/null 2>&1 || die "docker is required"
command -v jq >/dev/null 2>&1 || die "jq is required for host-side status parsing"

cleanup() {
    docker rm -f "$RELAY" "$NODE_A" "$NODE_B" >/dev/null 2>&1 || true
    docker network rm "$NET_A" "$NET_B" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

build_image() {
    if [ -n "${WA_E2E_BINARY:-}" ]; then
        [ -f "$WA_E2E_BINARY" ] || die "WA_E2E_BINARY not found: $WA_E2E_BINARY"
        local stage
        stage=$(mktemp -d)
        cp "$WA_E2E_BINARY" "$stage/waitagent"
        docker build -q -f "$SCRIPT_DIR/Dockerfile.runtime" -t "$IMAGE" "$stage" >/dev/null
        rm -rf "$stage"
    else
        log "building $IMAGE (in-docker release build; set WA_E2E_BINARY to skip)"
        docker build -q -f "$SCRIPT_DIR/Dockerfile" -t "$IMAGE" "$REPO_ROOT" >/dev/null
    fi
}

relay_status() {
    docker exec "$RELAY" waitagent relay status --listen "$RELAY_LISTEN"
}

# Polls the admin status until exactly the expected node ids are online.
# The whitelist directory is the enrollment ground truth; the status JSON is
# the runtime truth.
wait_for_node_ids() {
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    local want have
    want=$(printf '%s\n' "$@" | sort | paste -sd' ')
    while ((SECONDS < deadline)); do
        if status=$(relay_status 2>/dev/null); then
            have=$(jq -r '[.nodes[].node_id] | sort | join(" ")' <<<"$status")
            if [ "$have" = "$want" ]; then
                return 0
            fi
        fi
        sleep 2
    done
    die "nodes [$want] did not come online within ${GATE_TIMEOUT_SECS}s (last status: ${status:-<none>})"
}

node_count() {
    relay_status 2>/dev/null | jq -r '.nodes | length'
}

assert_running() {
    local name=$1
    [ "$(docker inspect -f '{{.State.Running}}' "$name")" = "true" ] \
        || die "$name is not running"
}

# Brings up the topology and gates on both nodes being enrolled and online.
# Fills FINGERPRINTS with the whitelisted node ids.
bring_up_topology() {
    docker network create --internal "$NET_A" >/dev/null
    docker network create --internal "$NET_B" >/dev/null

    docker run -d --name "$RELAY" --network "$NET_A" --network-alias relay "$IMAGE" \
        waitagent relay serve --listen "$RELAY_LISTEN" >/dev/null
    docker network connect --alias relay "$NET_B" "$RELAY" >/dev/null

    token=$(docker exec "$RELAY" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token" ] || die "relay invite produced no token"

    # -t: the node server initializes a ratatui TUI and needs a pty; there is
    # no headless mode. The pty stays attached to the detached container and
    # the idle TUI blocks on input long before the pty buffer matters.
    docker run -dt --name "$NODE_A" --network "$NET_A" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_A" >/dev/null
    docker run -dt --name "$NODE_B" --network "$NET_B" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_B" >/dev/null

    mapfile -t FINGERPRINTS < <(docker exec "$RELAY" sh -c 'ls /root/.waitagent/authorized_nodes')
    [ "${#FINGERPRINTS[@]}" -eq 2 ] \
        || die "expected 2 whitelisted nodes, got ${#FINGERPRINTS[@]}: ${FINGERPRINTS[*]:-<none>}"

    wait_for_node_ids "${FINGERPRINTS[@]}"
}

scenario_smoke() {
    log "scenario: smoke (relay + two enrolled nodes, status gate)"
    bring_up_topology

    status=$(relay_status)
    [ "$(jq -r '.ok' <<<"$status")" = "true" ] || die "status returned ok=false: $status"
    [ "$(jq -r '.registered_nodes' <<<"$status")" = "2" ] \
        || die "registered_nodes != 2: $status"
    assert_running "$RELAY"
    assert_running "$NODE_A"
    assert_running "$NODE_B"

    log "smoke OK: 2/2 nodes enrolled and online through the relay"
    jq . <<<"$status" | sed 's/^/  /'
}

scenario_reconnect() {
    log "scenario: reconnect (network partition and heal)"
    bring_up_topology

    # Partition node A: pulling its only interface kills the relay link
    # (the relay reaps the dead link; heartbeat silence reaps it worst case).
    docker network disconnect "$NET_A" "$NODE_A" >/dev/null
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        if [ "$(node_count)" = "1" ]; then
            break
        fi
        sleep 2
    done
    [ "$(node_count)" = "1" ] \
        || die "relay still sees 2 nodes after partitioning node A"
    log "partition observed: relay sees 1 node"

    docker network connect "$NET_A" "$NODE_A" >/dev/null
    wait_for_node_ids "${FINGERPRINTS[@]}"
    assert_running "$NODE_A"
    assert_running "$NODE_B"

    log "reconnect OK: node A re-enrolled through the relay"
}

scenario_reregister() {
    log "scenario: reregister (relay restart)"
    bring_up_topology

    # Restarting the relay kills both links; the nodes' relay clients must
    # re-register on the new process. The container keeps its writable layer
    # (whitelist, relay identity) and both network attachments.
    docker restart "$RELAY" >/dev/null
    assert_running "$RELAY"
    wait_for_node_ids "${FINGERPRINTS[@]}"
    assert_running "$NODE_A"
    assert_running "$NODE_B"

    log "reregister OK: both nodes re-registered after the relay restart"
}

scenarios=("$@")
if [ "${#scenarios[@]}" -eq 0 ]; then
    scenarios=(smoke reconnect reregister)
fi
build_image
for scenario in "${scenarios[@]}"; do
    # Each scenario gets a fresh topology; RUN is fixed so names stay
    # predictable, containers are recreated after the per-scenario cleanup.
    cleanup
    case "$scenario" in
        smoke)
            scenario_smoke
            ;;
        reconnect)
            scenario_reconnect
            ;;
        reregister)
            scenario_reregister
            ;;
        *)
            die "unknown scenario '$scenario' (known: smoke reconnect reregister)"
            ;;
    esac
    log "scenario '$scenario' passed"
done
