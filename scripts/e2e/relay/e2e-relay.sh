#!/bin/bash
# Relay docker e2e harness (issue #37).
#
# Relay topology: one relay container attached to TWO isolated bridges; node
# A only to the first, node B only to the second. A and B have no network
# path to each other by construction — every relay scenario exercises the
# relay as the only route, which is the acceptance criterion's "no LAN
# shortcuts".
#
# Direct-dial regression: the `direct` scenario brings up a separate
# topology — one bridge, NO relay container. Node A (--node-id node-a
# --connect node-b:9002) publishes its catalog to node B over the direct
# dial; in a topology with no relay at all, any connectivity IS the direct
# path (docs/relay-design.md compatibility promise).
#
# Images: by default the full Dockerfile builds the release binary inside
# docker (works on any host with docker). Set WA_E2E_BINARY to a prebuilt
# host binary to skip the in-docker build (CI and quick local runs).
#
# Usage: e2e-relay.sh [scenario ...]
#   scenarios: smoke (default), reconnect, reregister, streams, direct
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
    local name
    for name in "$RELAY" "$NODE_A" "$NODE_B"; do
        if docker inspect "$name" >/dev/null 2>&1; then
            log "$name logs (tail):"
            docker logs --tail 20 "$name" 2>&1 | sed 's/^/  /' || true
        fi
    done
    exit 1
}

RUN=${RUN:-$RANDOM$RANDOM}
RELAY="wa37-relay-$RUN"
NODE_A="wa37-node-a-$RUN"
NODE_B="wa37-node-b-$RUN"
NET_A="wa37-net-a-$RUN"
NET_B="wa37-net-b-$RUN"
NET_DIRECT="wa37-net-direct-$RUN"
FINGERPRINTS=()

command -v docker >/dev/null 2>&1 || die "docker is required"
command -v jq >/dev/null 2>&1 || die "jq is required for host-side status parsing"
command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen is required for the direct scenario"

cleanup() {
    docker rm -f "$RELAY" "$NODE_A" "$NODE_B" >/dev/null 2>&1 || true
    docker network rm "$NET_A" "$NET_B" "$NET_DIRECT" >/dev/null 2>&1 || true
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

node_command() {
    local container=$1 port=$2 command=$3
    docker exec "$container" waitagent __node-command "$port" "$command"
}

# Polls a node's control socket until it answers STATUS with ok=true.
wait_node_ready() {
    local container=$1 port=$2
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        if node_command "$container" "$port" STATUS 2>/dev/null \
            | jq -e '.type == "Response" and .payload.ok == true' >/dev/null; then
            return 0
        fi
        sleep 1
    done
    die "$container control socket never became ready"
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

scenario_streams() {
    log "scenario: streams (concurrent relay streams between nodes)"
    bring_up_topology

    local fp_b
    fp_b=$(docker exec "$NODE_B" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    [ -n "$fp_b" ] || die "could not read node B's fingerprint"

    # Node A opens 4 concurrent relay streams to node B and holds them for
    # 10s; the admin usage must show them as active streams while held.
    local probe_out
    probe_out=$(mktemp)
    docker exec "$NODE_A" waitagent __node-command "$NODE_PORT_A" \
        "E2E_RELAY_PROBE $fp_b 4 10" >"$probe_out" 2>&1 &
    local probe_pid=$!

    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    local observed=0 active
    while ((SECONDS < deadline)); do
        if active=$(relay_status 2>/dev/null | jq -r '.usage.active_streams' 2>/dev/null); then
            if [ "$active" -ge 4 ] 2>/dev/null; then
                observed=1
                break
            fi
        fi
        sleep 1
    done
    wait "$probe_pid" || true

    if [ "$observed" != "1" ]; then
        cat "$probe_out"
        rm -f "$probe_out"
        die "usage.active_streams never reached 4 during the probe"
    fi
    if ! jq -e '.type == "Response" and .payload.ok == true
        and (.payload.message | contains("\"streams_held\":4"))' "$probe_out" >/dev/null; then
        cat "$probe_out"
        rm -f "$probe_out"
        die "probe response did not confirm 4 held streams"
    fi
    rm -f "$probe_out"

    log "streams OK: 4 concurrent streams held through the relay"
}

scenario_direct() {
    log "scenario: direct (direct-dial regression; no relay in topology)"
    docker network create --internal "$NET_DIRECT" >/dev/null

    # A's state dir is host-mounted so the harness can read back the
    # operator key; --connect aims A's authority publication dial at node-b
    # and the dial stays direct (no relay is even present). The dial retries
    # with backoff until B is up and authorizes the key, so B may start last.
    local a_home stage
    a_home=$(mktemp -d)
    stage=$(mktemp -d)
    docker run -dt --name "$NODE_A" --network "$NET_DIRECT" \
        --network-alias node-a \
        -v "$a_home:/root/.waitagent" "$IMAGE" \
        sh -c "exec waitagent --port $NODE_PORT_A --node-id node-a --connect node-b:$NODE_PORT_B" >/dev/null

    # B authorizes A's operator public key. The key is generated inside A at
    # first auth use; the bind mount leaves it root-owned, so copy it out via
    # docker cp (daemon-side read) and derive the public key on the host.
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        [ -f "$a_home/operator.key" ] && break
        assert_running "$NODE_A"
        sleep 1
    done
    [ -f "$a_home/operator.key" ] || die "node A never generated its operator key"
    docker cp "$NODE_A:/root/.waitagent/operator.key" "$stage/operator.key" >/dev/null
    ssh-keygen -y -f "$stage/operator.key" >"$stage/node-a.pub" 2>/dev/null \
        || die "could not derive node A's operator public key"

    docker run -dt --name "$NODE_B" --network "$NET_DIRECT" \
        --network-alias node-b \
        -v "$stage/node-a.pub:/root/.waitagent/authorized_operators/node-a.pub:ro" "$IMAGE" \
        sh -c "exec waitagent --port $NODE_PORT_B" >/dev/null

    # A is a peer-mode node (--node-id), so it hosts a default
    # authority-host session for remote viewers and publishes it through the
    # catalog; the row arriving at all is the "A dialed B directly"
    # assertion — B records inbound authorities with via=None (direct).
    wait_node_ready "$NODE_B" "$NODE_PORT_B"

    local row=""
    deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        row=$(node_command "$NODE_B" "$NODE_PORT_B" LIST_SESSIONS 2>/dev/null \
            | jq -c '[.payload.data[]?
                | select(.id == "node-a:1"
                    and .transport == "remote"
                    and .authority_node_id == "node-a"
                    and .availability == "online")]
                | first' 2>/dev/null || true)
        if [ -n "$row" ] && [ "$row" != "null" ]; then
            break
        fi
        row=""
        sleep 2
    done
    [ -n "$row" ] || die "node B never saw node-a's default session over the direct link"
    log "node B sees node-a's published session row: $row"

    # Data plane: B opens a viewer on A's session, sizes it (headless
    # activation defaults to a 1x1 grid, which cannot show the marker), types
    # an echo, and the history must come back over the same direct link.
    # Activation mirrors the TUI select: a catalog row is only a published
    # view until the live remote session is created.
    local b_target marker b64 history="" activate_out resize_out paste_out
    b_target=$(jq -r '.id' <<<"$row")
    activate_out=$(node_command "$NODE_B" "$NODE_PORT_B" "ACTIVATE_TARGET $b_target")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$activate_out" >/dev/null \
        || die "ACTIVATE_TARGET on B failed: $activate_out"
    resize_out=$(node_command "$NODE_B" "$NODE_PORT_B" "RESIZE 80 24")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$resize_out" >/dev/null \
        || die "RESIZE on B failed: $resize_out"

    marker=WA37_DIRECT_OK
    b64=$(printf 'echo %s\n' "$marker" | base64 -w0)
    paste_out=$(node_command "$NODE_B" "$NODE_PORT_B" "PASTE_TEXT $b_target $b64")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$paste_out" >/dev/null \
        || die "PASTE_TEXT on B failed: $paste_out"

    deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        history=$(node_command "$NODE_B" "$NODE_PORT_B" "GET_HISTORY $b_target" 2>/dev/null || true)
        if jq -e --arg m "$marker" '
            [(.payload.lines // [])[], (.payload.styled_lines // [])[]]
            | any(contains($m))' <<<"$history" >/dev/null 2>&1; then
            break
        fi
        history=""
        sleep 2
    done
    [ -n "$history" ] || die "echo marker never came back over the direct link"
    log "echo marker round-tripped through the direct session"

    # Structural negative: A has no relay configured, so the relay probe must
    # refuse; together with the relay-less topology this pins the session to
    # the direct path.
    local probe
    probe=$(node_command "$NODE_A" "$NODE_PORT_A" "E2E_RELAY_PROBE node-b 1 1")
    jq -e '.type == "Response" and .payload.ok == false' <<<"$probe" >/dev/null \
        || die "node A has no relay but the probe did not refuse: $probe"

    rm -rf "$a_home" "$stage"
    log "direct OK: catalog + data plane over the direct dial, no relay involved"
}

scenarios=("$@")
if [ "${#scenarios[@]}" -eq 0 ]; then
    scenarios=(smoke reconnect reregister streams direct)
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
        streams)
            scenario_streams
            ;;
        direct)
            scenario_direct
            ;;
        *)
            die "unknown scenario '$scenario' (known: smoke reconnect reregister streams direct)"
            ;;
    esac
    log "scenario '$scenario' passed"
done
