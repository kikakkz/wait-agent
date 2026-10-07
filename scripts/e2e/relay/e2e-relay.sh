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
# Coverage scenarios (issue #145, closing the #137 reconciliation backlog):
#   revoke    — `relay remove` drops the whitelist entry AND the live link;
#               the revoked node's relay client can never re-establish
#               (data-port client auth checks the whitelist), while the
#               deploy token stays valid for clean nodes.
#   capacity  — relay.toml `capacity_max_nodes` admission: a third node's
#               enroll succeeds but its register is refused with the
#               NodeCapacity error frame; the connection table stays at cap.
#   presence  — a peer's relay liveness drives the remote session row's
#               availability on the observer node: online -> offline while
#               the peer is partitioned, back online on heal.
#   joinkeypaths — the global --node-key-path/--node-cert-path overrides
#               are honored end to end: a node seeded with a default
#               identity enrolls (and comes fully ONLINE) under the custom
#               certificate's fingerprint instead — join (#141) and the
#               runtime relay link (#151) share one identity source.
#   relaymgmt — issue #156 slice 1 (TUI relay management): the control
#               channel's RELAY_JOIN/RELAY_JOIN FORCE/RELAY_REMOVE drive
#               the exact flow the Ctrl-W popup sends — pin + link up,
#               quiet re-pin on an unchanged fingerprint, pin-mismatch
#               refusal that restores the previous pin, forced switch, and
#               removal that tears the link down.
#   viaautodirect — issue #156 slice 2 (via = auto): one bridge with the
#               relay present, so BOTH paths exist; the auto profile must
#               pick the direct path (probe succeeds) and remember it.
#   viaautorelay — issue #156 slice 2 (via = auto): the isolated two-bridge
#               topology (direct path impossible by construction); the auto
#               profile must fall back to the relay and remember it.
#
# Usage: e2e-relay.sh [scenario ...]
#   scenarios: smoke (default), reconnect, reregister, streams, direct,
#   pastefile, revoke, capacity, presence, joinkeypaths, relaymgmt,
#   viaautodirect, viaautorelay
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)
IMAGE=waitagent-e2e:local
RELAY_LISTEN=0.0.0.0:7475
RELAY_PORT=7475
NODE_PORT_A=9001
NODE_PORT_B=9002
NODE_PORT_C=9003
GATE_TIMEOUT_SECS=${GATE_TIMEOUT_SECS:-120}

log() { printf '[e2e-relay] %s\n' "$*"; }
die() {
    log "ERROR: $*"
    local name
    for name in "$RELAY" "$RELAY2" "$NODE_A" "$NODE_B" "$NODE_C"; do
        if docker inspect "$name" >/dev/null 2>&1; then
            log "$name logs (tail):"
            docker logs --tail 20 "$name" 2>&1 | sed 's/^/  /' || true
        fi
    done
    exit 1
}

RUN=${RUN:-$RANDOM$RANDOM}
RELAY="wa37-relay-$RUN"
RELAY2="wa37-relay2-$RUN"
NODE_A="wa37-node-a-$RUN"
NODE_B="wa37-node-b-$RUN"
NODE_C="wa37-node-c-$RUN"
NET_A="wa37-net-a-$RUN"
NET_B="wa37-net-b-$RUN"
NET_C="wa37-net-c-$RUN"
NET_DIRECT="wa37-net-direct-$RUN"
FINGERPRINTS=()

command -v docker >/dev/null 2>&1 || die "docker is required"
command -v jq >/dev/null 2>&1 || die "jq is required for host-side status parsing"
command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen is required for the direct scenario"

cleanup() {
    docker rm -f "$RELAY" "$RELAY2" "$NODE_A" "$NODE_B" "$NODE_C" >/dev/null 2>&1 || true
    docker network rm "$NET_A" "$NET_B" "$NET_C" "$NET_DIRECT" >/dev/null 2>&1 || true
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

# Relay-container-parameterized variants for scenarios that bring up a
# second relay (relaymgmt's pin-mismatch switch).
relay_status_on() {
    docker exec "$1" waitagent relay status --listen "$RELAY_LISTEN"
}

# Polls a relay container's admin status until exactly the expected node ids
# are online.
wait_for_node_ids_on() {
    local relay_container=$1
    shift
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    local want have
    want=$(printf '%s\n' "$@" | sort | paste -sd' ')
    while ((SECONDS < deadline)); do
        if status=$(relay_status_on "$relay_container" 2>/dev/null); then
            have=$(jq -r '[.nodes[].node_id] | sort | join(" ")' <<<"$status")
            if [ "$have" = "$want" ]; then
                return 0
            fi
        fi
        sleep 2
    done
    die "nodes [$want] did not come online within ${GATE_TIMEOUT_SECS}s (last status: ${status:-<none>})"
}

node_count_on() {
    relay_status_on "$1" 2>/dev/null | jq -r '.nodes | length'
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

# Polls a node's relay probe until it is deterministically refused
# (ok=false) and returns the refusal response. A node whose relay link can
# never register — revoked (whitelist client-auth refuses the handshake) or
# capacity-refused (NodeCapacity error frame tears the link down before any
# usable opener exists) — has no relay opener for the probe's open_stream,
# so NotConnected is the steady-state answer.
wait_probe_refused() {
    local container=$1 port=$2 peer=$3
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    local probe_out=""
    while ((SECONDS < deadline)); do
        probe_out=$(node_command "$container" "$port" "E2E_RELAY_PROBE $peer 1 1" 2>/dev/null || true)
        if jq -e '.type == "Response" and .payload.ok == false' <<<"$probe_out" >/dev/null 2>&1; then
            printf '%s\n' "$probe_out"
            return 0
        fi
        probe_out=""
        sleep 2
    done
    die "$container relay probe was not refused within ${GATE_TIMEOUT_SECS}s"
}

# Polls LIST_SESSIONS on a node until the row with the given target id
# reports the wanted availability ("online"/"offline"/"exited") and returns
# the row. The row keeps its catalog entry across a disconnect (the observer
# screen is preserved for reconnect), so only the availability field moves.
wait_row_availability() {
    local container=$1 port=$2 target=$3 want=$4
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    local row=""
    while ((SECONDS < deadline)); do
        row=$(node_command "$container" "$port" LIST_SESSIONS 2>/dev/null \
            | jq -c --arg t "$target" --arg a "$want" '[.payload.data[]?
                | select(.id == $t and .availability == $a)]
                | first' 2>/dev/null || true)
        if [ -n "$row" ] && [ "$row" != "null" ]; then
            printf '%s\n' "$row"
            return 0
        fi
        row=""
        sleep 2
    done
    die "$container never saw $target availability=$want within ${GATE_TIMEOUT_SECS}s"
}

# Polls LIST_SESSIONS on a node until a row with the given authority node
# id reports availability=online and returns the row. Used by the via-auto
# scenarios as the data-plane anchor for "the auto connect really
# established a session".
wait_authority_row_online() {
    local container=$1 port=$2 authority=$3
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    local row=""
    while ((SECONDS < deadline)); do
        row=$(node_command "$container" "$port" LIST_SESSIONS 2>/dev/null \
            | jq -c --arg a "$authority" '[.payload.data[]?
                | select(.authority_node_id == $a and .availability == "online")]
                | first' 2>/dev/null || true)
        if [ -n "$row" ] && [ "$row" != "null" ]; then
            printf '%s\n' "$row"
            return 0
        fi
        row=""
        sleep 2
    done
    die "$container never saw $authority online within ${GATE_TIMEOUT_SECS}s"
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

scenario_joinkeypaths() {
    log "scenario: joinkeypaths (relay join honors --node-key-path/--node-cert-path)"
    docker network create --internal "$NET_A" >/dev/null

    docker run -d --name "$RELAY" --network "$NET_A" --network-alias relay "$IMAGE" \
        waitagent relay serve --listen "$RELAY_LISTEN" >/dev/null

    token=$(docker exec "$RELAY" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token" ] || die "relay invite produced no token"

    # Seed the DEFAULT credential location with a distinct identity before
    # joining through the custom paths. If join ignored the overrides, the
    # whitelist entry would be the default identity's fingerprint and
    # /tmp/custom would never be created.
    docker run -dt --name "$NODE_A" --network "$NET_A" "$IMAGE" \
        sh -c "waitagent __generate-node-credentials > /tmp/default-creds.txt && \
            waitagent --node-key-path /tmp/custom/node.key --node-cert-path /tmp/custom/node.crt \
                relay join relay:$RELAY_PORT '$token' && \
            exec waitagent --node-key-path /tmp/custom/node.key --node-cert-path /tmp/custom/node.crt \
                --port $NODE_PORT_A" >/dev/null

    # The join must whitelist exactly one node: the custom identity.
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        mapfile -t FINGERPRINTS < <(docker exec "$RELAY" \
            sh -c 'ls /root/.waitagent/authorized_nodes' 2>/dev/null)
        [ "${#FINGERPRINTS[@]}" -eq 1 ] && break
        sleep 1
    done
    [ "${#FINGERPRINTS[@]}" -eq 1 ] \
        || die "expected exactly 1 whitelisted node, got ${#FINGERPRINTS[@]}"

    # The custom files hold the enrolled identity; the seeded defaults were
    # NOT what join presented to the relay.
    docker exec "$NODE_A" test -f /tmp/custom/node.key || die "custom key was not created"
    docker exec "$NODE_A" test -f /tmp/custom/node.crt || die "custom cert was not created"
    docker exec "$NODE_A" test -f /root/.waitagent/node.key || die "default key seed missing"
    docker exec "$NODE_A" test -f /root/.waitagent/node.crt || die "default cert seed missing"

    local default_fp custom_fp
    default_fp=$(docker exec "$NODE_A" \
        sh -c "sed -n 's/^WAITAGENT_CREDENTIALS\\([0-9a-f]*\\):.*/\\1/p' /tmp/default-creds.txt")
    [ -n "$default_fp" ] || die "could not parse the default identity fingerprint"
    # `__generate-node-credentials` returns the existing cert's fingerprint,
    # so this reads the fingerprint of the custom certificate.
    custom_fp=$(docker exec "$NODE_A" \
        sh -c "waitagent --node-key-path /tmp/custom/node.key --node-cert-path /tmp/custom/node.crt __generate-node-credentials" \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]*\):.*/\1/p')
    [ -n "$custom_fp" ] || die "could not fingerprint the custom certificate"
    [ "$custom_fp" = "${FINGERPRINTS[0]}" ] \
        || die "whitelist entry ${FINGERPRINTS[0]} is not the custom cert's fingerprint $custom_fp"
    [ "$custom_fp" != "$default_fp" ] \
        || die "join enrolled the default identity despite the overrides"

    # Full-online anchor (issue #151): the node server registers its relay
    # link under the SAME custom identity (not the seeded default), so the
    # relay's connection table must show exactly the whitelisted
    # fingerprint online. Before the runtime honored the overrides this
    # steady-stated as whitelist client-auth refusals (UnknownIssuer).
    wait_for_node_ids "${FINGERPRINTS[@]}"
    assert_running "$NODE_A"

    log "joinkeypaths OK: custom identity ($custom_fp) enrolled AND online, not the seeded default ($default_fp)"
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

scenario_pastefile() {
    log "scenario: pastefile (file paste over a relay link lands in the host cache)"
    # Custom bring-up (not bring_up_topology): A must start first so its
    # operator key exists, B then starts with A's operator public key
    # authorized, and A needs a host-mounted .waitagent for the remote-hosts
    # profile written below.
    docker network create --internal "$NET_A" >/dev/null
    docker network create --internal "$NET_B" >/dev/null

    docker run -d --name "$RELAY" --network "$NET_A" --network-alias relay "$IMAGE" \
        waitagent relay serve --listen "$RELAY_LISTEN" >/dev/null
    docker network connect --alias relay "$NET_B" "$RELAY" >/dev/null

    token=$(docker exec "$RELAY" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token" ] || die "relay invite produced no token"

    local a_home stage
    a_home=$(mktemp -d)
    stage=$(mktemp -d)
    docker run -dt --name "$NODE_A" --network "$NET_A" \
        -v "$a_home:/root/.waitagent" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_A" >/dev/null

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

    docker run -dt --name "$NODE_B" --network "$NET_B" \
        -v "$stage/node-a.pub:/root/.waitagent/authorized_operators/node-a.pub:ro" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_B" >/dev/null

    mapfile -t FINGERPRINTS < <(docker exec "$RELAY" sh -c 'ls /root/.waitagent/authorized_nodes')
    [ "${#FINGERPRINTS[@]}" -eq 2 ] \
        || die "expected 2 whitelisted nodes, got ${#FINGERPRINTS[@]}: ${FINGERPRINTS[*]:-<none>}"
    wait_for_node_ids "${FINGERPRINTS[@]}"

    local fp_b
    fp_b=$(docker exec "$NODE_B" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    [ -n "$fp_b" ] || die "could not read node B's fingerprint"

    # A connects to B through the relay via a remote-hosts profile. The relay
    # dial keys on tls_pin_sha256 (== B's enrolled fingerprint), so
    # last_remote_port only feeds the authority id "node-b#9002" and the
    # reuse fast path; SSH bootstrap never runs.
    cat >"$a_home/remote-hosts.toml" <<EOF
[[hosts]]
name = "node-b"
host = "node-b"
ssh_user = "root"
auth_kind = "key"
key_path = "/root/.ssh/unused"
remote_shell = "posix"
last_remote_port = $NODE_PORT_B
tls_pin_sha256 = "$fp_b"
via = "relay"
EOF

    local connect_out
    connect_out=$(node_command "$NODE_A" "$NODE_PORT_A" "CONNECT_REMOTE_HOST node-b")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$connect_out" >/dev/null \
        || die "CONNECT_REMOTE_HOST on A failed: $connect_out"

    # The connected authority shows up as a remote session row.
    local row=""
    deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        row=$(node_command "$NODE_A" "$NODE_PORT_A" LIST_SESSIONS 2>/dev/null \
            | jq -c '[.payload.data[]?
                | select(.transport == "remote"
                    and .authority_node_id == "node-b#9002"
                    and .availability == "online")]
                | first' 2>/dev/null || true)
        if [ -n "$row" ] && [ "$row" != "null" ]; then
            break
        fi
        row=""
        sleep 2
    done
    [ -n "$row" ] || die "node A never saw node B through the relay profile"
    log "node A sees node B through the relay: $row"

    # Create a session on B and open a viewer on it.
    local create_out b_target
    create_out=$(node_command "$NODE_A" "$NODE_PORT_A" "CREATE_REMOTE_SESSION node-b#9002 /root")
    b_target=$(jq -r '.payload.message // ""' <<<"$create_out" \
        | sed -n 's/^created remote session //p')
    [ -n "$b_target" ] || die "CREATE_REMOTE_SESSION on A failed: $create_out"

    deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    row=""
    while ((SECONDS < deadline)); do
        row=$(node_command "$NODE_A" "$NODE_PORT_A" LIST_SESSIONS 2>/dev/null \
            | jq -c --arg t "$b_target" '[.payload.data[]?
                | select(.id == $t and .availability == "online")]
                | first' 2>/dev/null || true)
        if [ -n "$row" ] && [ "$row" != "null" ]; then
            break
        fi
        row=""
        sleep 2
    done
    [ -n "$row" ] || die "created session $b_target never came online on node A"
    log "node A sees the created remote session: $row"

    local activate_out resize_out paste_out
    activate_out=$(node_command "$NODE_A" "$NODE_PORT_A" "ACTIVATE_TARGET $b_target")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$activate_out" >/dev/null \
        || die "ACTIVATE_TARGET on A failed: $activate_out"
    resize_out=$(node_command "$NODE_A" "$NODE_PORT_A" "RESIZE 80 24")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$resize_out" >/dev/null \
        || die "RESIZE on A failed: $resize_out"

    # Small file: a single __node-command argument is capped by
    # MAX_ARG_STRLEN (~128 KiB), so multi-chunk transfers are covered by
    # component tests, not this scenario.
    local marker b64 cached
    marker=WA37_PASTEFILE_MARKER_7f3d9b
    b64=$(printf '%s\n' "$marker" | base64 -w0)
    paste_out=$(node_command "$NODE_A" "$NODE_PORT_A" "PASTE_FILE $b_target e2e-paste.txt $b64")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$paste_out" >/dev/null \
        || die "PASTE_FILE on A failed: $paste_out"

    # The host (B) must reassemble the chunks, write the file into its
    # clipboard cache, and feed the cached path into the hosted session.
    deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    cached=""
    while ((SECONDS < deadline)); do
        cached=$(docker exec "$NODE_B" sh -c \
            "grep -rlF '$marker' /tmp/waitagent/ 2>/dev/null | head -n 1" 2>/dev/null || true)
        if [ -n "$cached" ]; then
            break
        fi
        sleep 2
    done
    [ -n "$cached" ] || die "pasted file never landed in node B's clipboard cache"
    log "pasted file landed in node B's clipboard cache: $cached"

    # The cached path reference is fed into the hosted session's PTY; its
    # echo comes back over the mirror to A's observer. (The host's own local
    # grid for the session is not backfilled by design, so the observable
    # anchor for "the session received the reference" is the viewer side.)
    local history=""
    deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        history=$(node_command "$NODE_A" "$NODE_PORT_A" "GET_HISTORY $b_target" 2>/dev/null || true)
        if jq -e '[.payload.lines // [], .payload.styled_lines // []][]
            | join(" ") | contains("/tmp/waitagent/")' <<<"$history" >/dev/null 2>&1; then
            break
        fi
        history=""
        sleep 2
    done
    [ -n "$history" ] || die "viewer never saw the cached file reference in the session"
    log "viewer saw the cached file reference typed into the session"

    rm -rf "$a_home" "$stage"
    log "pastefile OK: file crossed the relay link into the host clipboard cache"
}

scenario_revoke() {
    log "scenario: revoke (relay remove drops the whitelist entry and the live link)"
    bring_up_topology

    # Identity comes from each node, not from the relay's ls order: the
    # whitelist file names sort independently of which container enrolled
    # first, and the scenario hinges on revoking the node we later probe.
    local fp_a fp_b
    fp_a=$(docker exec "$NODE_A" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    fp_b=$(docker exec "$NODE_B" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    [ -n "$fp_a" ] && [ -n "$fp_b" ] || die "could not read node fingerprints"

    # The CLI remove surface answers with both halves named; asserting the
    # message pins the admin-channel path end to end.
    local remove_out
    remove_out=$(docker exec "$RELAY" waitagent relay remove "$fp_a" --listen "$RELAY_LISTEN")
    grep -q "removed: $fp_a (whitelist entry removed; live link dropped)" <<<"$remove_out" \
        || die "relay remove did not report whitelist+link removal: $remove_out"
    log "relay remove answered: $(head -n1 <<<"$remove_out")"

    # Anchor (relay runtime truth): registered_nodes 2 -> 1 and only B remains.
    # retire_notifying drops A's table entry synchronously with the admin
    # command, so this converges as soon as the admin socket answers.
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS)) status=""
    while ((SECONDS < deadline)); do
        status=$(relay_status 2>/dev/null || true)
        if [ "$(jq -r '.registered_nodes' <<<"$status" 2>/dev/null)" = "1" ] \
            && [ "$(jq -r '[.nodes[].node_id] | sort | join(" ")' <<<"$status" 2>/dev/null)" = "$fp_b" ]; then
            break
        fi
        status=""
        sleep 2
    done
    [ -n "$status" ] || die "relay status did not converge to 1 node ($fp_b) after remove"

    # Anchor (enrollment ground truth): A's whitelist entry is gone, so the
    # data port's whitelist client-auth refuses every future TLS handshake.
    if docker exec "$RELAY" sh -c "test -f '/root/.waitagent/authorized_nodes/$fp_a'" 2>/dev/null; then
        die "A's whitelist entry survived relay remove"
    fi

    # Anchor (A's process-level death): the revoked link got a NodeRevoked
    # Error frame and tore down; A's relay client retries forever, but with
    # the whitelist entry gone the handshake is refused before Register, so
    # no opener is ever installed and the probe deterministically reports
    # NotConnected. Poll: the old link may still be draining right after
    # remove.
    wait_probe_refused "$NODE_A" "$NODE_PORT_A" "$fp_b"
    log "node A's relay probe is refused after revocation"

    # The deploy token minted at bring-up is NOT revoked by removing A (a
    # fingerprint-scoped revocation): a clean node C enrolls with the same
    # token and registers, while A stays locked out.
    docker run -dt --name "$NODE_C" --network "$NET_A" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_C" >/dev/null

    local fp_c
    fp_c=$(docker exec "$NODE_C" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    [ -n "$fp_c" ] || die "could not read node C's fingerprint"
    [ "$fp_c" != "$fp_a" ] && [ "$fp_c" != "$fp_b" ] \
        || die "node C reused an existing fingerprint: $fp_c"

    wait_for_node_ids "$fp_b" "$fp_c"

    # A is still refused after C registered: reconnect attempts keep failing
    # the whitelist handshake, so the probe stays refused.
    wait_probe_refused "$NODE_A" "$NODE_PORT_A" "$fp_b"
    log "node A's relay probe still refused after C registered"

    # The whitelist now holds exactly {B, C}.
    local whitelist
    whitelist=$(docker exec "$RELAY" sh -c 'ls /root/.waitagent/authorized_nodes' | sort | paste -sd' ')
    [ "$whitelist" = "$(printf '%s\n' "$fp_b" "$fp_c" | sort | paste -sd' ')" ] \
        || die "whitelist should be {B, C}, got: $whitelist"

    log "revoke OK: A revoked (whitelist + live link), token still enrolls C, A stays locked out"
}

scenario_capacity() {
    log "scenario: capacity (relay.toml capacity_max_nodes refuses a third node)"
    # Custom bring-up (not bring_up_topology): the relay must start with its
    # own relay.toml setting the connection-table cap — the
    # relay_serve_toml_store config surface (issue #35 wiring).
    docker network create --internal "$NET_A" >/dev/null
    docker network create --internal "$NET_B" >/dev/null
    docker network create --internal "$NET_C" >/dev/null

    docker run -d --name "$RELAY" --network "$NET_A" --network-alias relay "$IMAGE" \
        sh -c "mkdir -p /root/.waitagent \
            && printf 'capacity_max_nodes = 2\n' > /root/.waitagent/relay.toml \
            && exec waitagent relay serve --listen $RELAY_LISTEN" >/dev/null
    docker network connect --alias relay "$NET_B" "$RELAY" >/dev/null
    docker network connect --alias relay "$NET_C" "$RELAY" >/dev/null

    token=$(docker exec "$RELAY" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token" ] || die "relay invite produced no token"

    docker run -dt --name "$NODE_A" --network "$NET_A" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_A" >/dev/null
    docker run -dt --name "$NODE_B" --network "$NET_B" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_B" >/dev/null

    mapfile -t FINGERPRINTS < <(docker exec "$RELAY" sh -c 'ls /root/.waitagent/authorized_nodes')
    [ "${#FINGERPRINTS[@]}" -eq 2 ] \
        || die "expected 2 whitelisted nodes, got ${#FINGERPRINTS[@]}: ${FINGERPRINTS[*]:-<none>}"
    wait_for_node_ids "${FINGERPRINTS[@]}"

    # Anchor (config surface took effect): the admin status carries the
    # capacity knob, so a wrong value here means relay.toml was not read.
    local status
    status=$(relay_status)
    [ "$(jq -r '.capacity.max_nodes' <<<"$status")" = "2" ] \
        || die "relay did not pick up capacity_max_nodes=2: $status"
    log "relay admin status reports capacity.max_nodes=2"

    # Node C enrolls with the same deploy token. Enrollment deliberately has
    # no capacity check (the token authorizes, the whitelist is the
    # enrollment truth); the refusal happens at register time on the data
    # link, where admission control lives.
    docker run -dt --name "$NODE_C" --network "$NET_C" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_C" >/dev/null
    local fp_c
    fp_c=$(docker exec "$NODE_C" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    [ -n "$fp_c" ] || die "could not read node C's fingerprint"

    # Anchor (typed refusal): register_link answers the at-cap register with
    # the NodeCapacity error frame and logs the refusal. ERROR_LOG writes to
    # the fixed diag file (/tmp/waitagent-diag.log) inside the relay
    # container — docker logs only carries the startup banner — and the
    # container is fresh per scenario, so a match cannot be stale. C's
    # client retries forever, so the line lands within one retry cycle.
    local diag
    diag="grep -aq 'register refused, max_nodes reached' /tmp/waitagent-diag.log 2>/dev/null"
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        if docker exec "$RELAY" sh -c "$diag"; then
            break
        fi
        sleep 2
    done
    docker exec "$RELAY" sh -c "$diag" \
        || die "relay never logged the max_nodes register refusal"

    # Anchor (runtime truth): the connection table holds exactly {A, B}
    # through a settle window — C keeps retrying but never admits.
    local want have
    want=$(printf '%s\n' "${FINGERPRINTS[@]}" | sort | paste -sd' ')
    local settle_end=$((SECONDS + 15))
    while ((SECONDS < settle_end)); do
        status=$(relay_status 2>/dev/null || true)
        have=$(jq -r '[.nodes[].node_id] | sort | join(" ")' <<<"$status" 2>/dev/null || true)
        if [ "$have" != "$want" ]; then
            die "connection table changed past the cap: want [$want], got [$have]"
        fi
        sleep 2
    done
    log "connection table stayed at cap: [$want]"

    # Anchor (C's process-level view): C's relay client gets the NodeCapacity
    # Error frame and the link tears down before any opener is observable
    # outside the register retry window, so the probe reports NotConnected.
    wait_probe_refused "$NODE_C" "$NODE_PORT_C" "${FINGERPRINTS[0]}"
    log "node C's relay probe is refused while the relay is at capacity"

    log "capacity OK: max_nodes=2 admitted A+B, C's register refused (NodeCapacity), table stayed at cap"
}

scenario_presence() {
    log "scenario: presence (peer relay liveness drives the remote row's availability)"
    # Custom bring-up (mirrors pastefile): A's operator key is authorized on
    # B and A reaches B through a remote-hosts profile (via=relay, tls_pin =
    # B's enrolled fingerprint).
    docker network create --internal "$NET_A" >/dev/null
    docker network create --internal "$NET_B" >/dev/null

    docker run -d --name "$RELAY" --network "$NET_A" --network-alias relay "$IMAGE" \
        waitagent relay serve --listen "$RELAY_LISTEN" >/dev/null
    docker network connect --alias relay "$NET_B" "$RELAY" >/dev/null

    token=$(docker exec "$RELAY" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token" ] || die "relay invite produced no token"

    local a_home stage
    a_home=$(mktemp -d)
    stage=$(mktemp -d)
    docker run -dt --name "$NODE_A" --network "$NET_A" \
        -v "$a_home:/root/.waitagent" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_A" >/dev/null

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

    docker run -dt --name "$NODE_B" --network "$NET_B" \
        -v "$stage/node-a.pub:/root/.waitagent/authorized_operators/node-a.pub:ro" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_B" >/dev/null

    mapfile -t FINGERPRINTS < <(docker exec "$RELAY" sh -c 'ls /root/.waitagent/authorized_nodes')
    [ "${#FINGERPRINTS[@]}" -eq 2 ] \
        || die "expected 2 whitelisted nodes, got ${#FINGERPRINTS[@]}: ${FINGERPRINTS[*]:-<none>}"
    wait_for_node_ids "${FINGERPRINTS[@]}"

    local fp_b
    fp_b=$(docker exec "$NODE_B" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    [ -n "$fp_b" ] || die "could not read node B's fingerprint"

    cat >"$a_home/remote-hosts.toml" <<EOF
[[hosts]]
name = "node-b"
host = "node-b"
ssh_user = "root"
auth_kind = "key"
key_path = "/root/.ssh/unused"
remote_shell = "posix"
last_remote_port = $NODE_PORT_B
tls_pin_sha256 = "$fp_b"
via = "relay"
EOF

    local connect_out
    connect_out=$(node_command "$NODE_A" "$NODE_PORT_A" "CONNECT_REMOTE_HOST node-b")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$connect_out" >/dev/null \
        || die "CONNECT_REMOTE_HOST on A failed: $connect_out"

    # A live remote session row on A, observed through the control socket —
    # the same SessionView data the console sidebar renders.
    local b_target row
    connect_out=$(node_command "$NODE_A" "$NODE_PORT_A" "CREATE_REMOTE_SESSION node-b#9002 /root")
    b_target=$(jq -r '.payload.message // ""' <<<"$connect_out" \
        | sed -n 's/^created remote session //p')
    [ -n "$b_target" ] || die "CREATE_REMOTE_SESSION on A failed: $connect_out"

    row=$(wait_row_availability "$NODE_A" "$NODE_PORT_A" "$b_target" "online")
    log "node A sees $b_target online: $row"

    local activate_out resize_out
    activate_out=$(node_command "$NODE_A" "$NODE_PORT_A" "ACTIVATE_TARGET $b_target")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$activate_out" >/dev/null \
        || die "ACTIVATE_TARGET on A failed: $activate_out"
    resize_out=$(node_command "$NODE_A" "$NODE_PORT_A" "RESIZE 80 24")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$resize_out" >/dev/null \
        || die "RESIZE on A failed: $resize_out"

    # Partition B. The relay loses B's link (TCP read error, then worst case
    # the heartbeat sweeper) and closes the A<->B relay streams with it;
    # A's mirror observes the transport death and the state loop marks the
    # row offline — the sidebar-rendered availability — while the presence
    # frames from the relay's PresenceHub drive the per-row `relay:offline`
    # console marker for the same event. Both flip on the same link loss, so
    # the row availability is the deterministic control-socket anchor for
    # presence reaching the console.
    docker network disconnect "$NET_B" "$NODE_B" >/dev/null
    row=$(wait_row_availability "$NODE_A" "$NODE_PORT_A" "$b_target" "offline")
    log "partition observed: node A marks $b_target offline"

    # Heal: B's relay client re-registers (backoff <= 5s), A's reconnect and
    # outbound-dial workers re-establish through the relay, and the row
    # flips back online.
    docker network connect --alias node-b "$NET_B" "$NODE_B" >/dev/null
    row=$(wait_row_availability "$NODE_A" "$NODE_PORT_A" "$b_target" "online")
    log "heal observed: node A marks $b_target back online"

    rm -rf "$a_home" "$stage"
    log "presence OK: remote row availability followed B's relay liveness (online -> offline -> online)"
}

scenario_relaymgmt() {
    log "scenario: relaymgmt (control-channel relay join/remove, issue #156 slice 1)"
    docker network create --internal "$NET_A" >/dev/null

    # Two relays with distinct identities: relay2 only exists so the join
    # can present a fingerprint the node never pinned (the pin-mismatch
    # path needs a real second identity, not a file edit).
    docker run -d --name "$RELAY" --network "$NET_A" --network-alias relay "$IMAGE" \
        waitagent relay serve --listen "$RELAY_LISTEN" >/dev/null
    docker run -d --name "$RELAY2" --network "$NET_A" --network-alias relay2 "$IMAGE" \
        waitagent relay serve --listen "$RELAY_LISTEN" >/dev/null

    token=$(docker exec "$RELAY" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token" ] || die "relay invite produced no token"
    token2=$(docker exec "$RELAY2" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token2" ] || die "relay2 invite produced no token"

    relay_fp=$(docker exec "$RELAY" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]*\):.*/\1/p')
    [ -n "$relay_fp" ] || die "could not fingerprint the relay identity"
    relay2_fp=$(docker exec "$RELAY2" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]*\):.*/\1/p')
    [ -n "$relay2_fp" ] || die "could not fingerprint the relay2 identity"
    [ "$relay_fp" != "$relay2_fp" ] || die "the two relays must have distinct identities"

    # Node A starts WITHOUT joining: enrollment must arrive through the
    # node control channel — the exact RELAY_JOIN command the Ctrl-W popup
    # sends (base64 address/token, matching the TUI encoding).
    docker run -dt --name "$NODE_A" --network "$NET_A" "$IMAGE" \
        sh -c "exec waitagent --port $NODE_PORT_A" >/dev/null
    wait_node_ready "$NODE_A" "$NODE_PORT_A"
    if docker exec "$NODE_A" test -f /root/.waitagent/relay.toml; then
        die "node A must start without a relay pin"
    fi

    join_relay_cmd=$(docker exec "$NODE_A" sh -c \
        "printf 'RELAY_JOIN %s %s' \"\$(printf %s 'relay:$RELAY_PORT' | base64 -w0)\" \"\$(printf %s '$token' | base64 -w0)\"")
    join_relay2_cmd=$(docker exec "$NODE_A" sh -c \
        "printf 'RELAY_JOIN %s %s' \"\$(printf %s 'relay2:$RELAY_PORT' | base64 -w0)\" \"\$(printf %s '$token2' | base64 -w0)\"")

    join_out=$(node_command "$NODE_A" "$NODE_PORT_A" "$join_relay_cmd")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$join_out" >/dev/null \
        || die "RELAY_JOIN failed: $join_out"
    grep -q "relay joined" <<<"$(jq -r '.payload.message // ""' <<<"$join_out")" \
        || die "RELAY_JOIN answer must report the joined relay: $join_out"

    # The pin names the relay address and the relay identity fingerprint.
    docker exec "$NODE_A" grep -q "^address = \"relay:$RELAY_PORT\"" /root/.waitagent/relay.toml \
        || die "relay.toml does not pin the joined relay address"
    docker exec "$NODE_A" grep -q "^relay_fingerprint = \"$relay_fp\"" /root/.waitagent/relay.toml \
        || die "relay.toml does not pin the relay identity fingerprint"

    # The link comes up: the relay's connection table lists A online.
    fp_a=$(docker exec "$NODE_A" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]*\):.*/\1/p')
    [ -n "$fp_a" ] || die "could not fingerprint node A"
    wait_for_node_ids "$fp_a"
    log "relay link established after control-channel join"

    # Re-join with the unchanged fingerprint is the quiet path: ok, and
    # the answer reports the pin was already in place.
    rejoin_out=$(node_command "$NODE_A" "$NODE_PORT_A" "$join_relay_cmd")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$rejoin_out" >/dev/null \
        || die "quiet re-join failed: $rejoin_out"
    grep -q "relay already pinned" <<<"$(jq -r '.payload.message // ""' <<<"$rejoin_out")" \
        || die "unchanged fingerprint must answer 'relay already pinned': $rejoin_out"

    # Pin-mismatch guard: joining the OTHER relay (fingerprint the node
    # never pinned) refuses by default and restores the previous pin — the
    # control-channel equivalent of the TUI's abort-by-default warning.
    mismatch_out=$(node_command "$NODE_A" "$NODE_PORT_A" "$join_relay2_cmd")
    jq -e '.type == "Response" and .payload.ok == false' <<<"$mismatch_out" >/dev/null \
        || die "pin-mismatch join must refuse: $mismatch_out"
    grep -q "pin mismatch" <<<"$(jq -r '.payload.message // ""' <<<"$mismatch_out")" \
        || die "the refusal must name the pin mismatch: $mismatch_out"
    docker exec "$NODE_A" grep -q "^address = \"relay:$RELAY_PORT\"" /root/.waitagent/relay.toml \
        || die "the refused join must restore the previous relay address"
    docker exec "$NODE_A" grep -q "^relay_fingerprint = \"$relay_fp\"" /root/.waitagent/relay.toml \
        || die "the refused join must restore the previous pin"
    wait_for_node_ids "$fp_a"
    log "pin mismatch refused by default and the previous pin was restored"

    # The explicit confirmation (the TUI's Switch anyway = FORCE) accepts
    # the new enrollment, re-pins relay2, and restarts the link there.
    force_out=$(node_command "$NODE_A" "$NODE_PORT_A" "$join_relay2_cmd FORCE")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$force_out" >/dev/null \
        || die "forced join failed: $force_out"
    grep -q "relay joined" <<<"$(jq -r '.payload.message // ""' <<<"$force_out")" \
        || die "the forced join must report the joined relay: $force_out"
    docker exec "$NODE_A" grep -q "^address = \"relay2:$RELAY_PORT\"" /root/.waitagent/relay.toml \
        || die "the forced join must pin relay2"
    docker exec "$NODE_A" grep -q "^relay_fingerprint = \"$relay2_fp\"" /root/.waitagent/relay.toml \
        || die "the forced join must pin relay2's fingerprint"
    wait_for_node_ids_on "$RELAY2" "$fp_a"
    log "forced switch re-pinned relay2 and the link re-established"

    # Removal: relay.toml is cleared and the link is torn down (the active
    # relay's table empties; relay IO through the control channel is
    # refused because no relay is configured anymore).
    remove_out=$(node_command "$NODE_A" "$NODE_PORT_A" "RELAY_REMOVE")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$remove_out" >/dev/null \
        || die "RELAY_REMOVE failed: $remove_out"
    if docker exec "$NODE_A" test -f /root/.waitagent/relay.toml; then
        die "relay.toml must be removed"
    fi
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        [ "$(node_count_on "$RELAY2")" = "0" ] && break
        sleep 2
    done
    [ "$(node_count_on "$RELAY2")" = "0" ] \
        || die "relay2 table must empty after removal (last status: $(relay_status_on "$RELAY2"))"
    wait_probe_refused "$NODE_A" "$NODE_PORT_A" deadbeef >/dev/null
    log "relay removed: pin cleared, link down"
}

scenario_viaautodirect() {
    log "scenario: viaautodirect (via = auto picks direct when the peer is directly reachable)"
    # One bridge with the relay container also attached: both the direct
    # path (node-b is on the same network) and the relay path exist. The
    # auto profile must probe direct, dial direct, and remember it.
    docker network create --internal "$NET_DIRECT" >/dev/null

    docker run -d --name "$RELAY" --network "$NET_DIRECT" --network-alias relay "$IMAGE" \
        waitagent relay serve --listen "$RELAY_LISTEN" >/dev/null

    token=$(docker exec "$RELAY" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token" ] || die "relay invite produced no token"

    local a_home stage
    a_home=$(mktemp -d)
    stage=$(mktemp -d)
    # A enrolls (relay available) with a host-mounted state dir so the
    # harness can write the remote-hosts profile and read back the
    # last_via_used annotation.
    docker run -dt --name "$NODE_A" --network "$NET_DIRECT" --network-alias node-a \
        -v "$a_home:/root/.waitagent" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_A" >/dev/null

    # B authorizes A's operator key (same pattern as pastefile/direct): the
    # reuse dial answers B's operator challenge with A's key.
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

    docker run -dt --name "$NODE_B" --network "$NET_DIRECT" --network-alias node-b \
        -v "$stage/node-a.pub:/root/.waitagent/authorized_operators/node-a.pub:ro" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_B" >/dev/null

    mapfile -t FINGERPRINTS < <(docker exec "$RELAY" sh -c 'ls /root/.waitagent/authorized_nodes')
    [ "${#FINGERPRINTS[@]}" -eq 2 ] \
        || die "expected 2 whitelisted nodes, got ${#FINGERPRINTS[@]}: ${FINGERPRINTS[*]:-<none>}"
    wait_for_node_ids "${FINGERPRINTS[@]}"

    local fp_b
    fp_b=$(docker exec "$NODE_B" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    [ -n "$fp_b" ] || die "could not read node B's fingerprint"

    # Auto profile: both paths exist, so the direct probe must win. The
    # cached shell/port/pin keep the connect on the reuse fast path (no SSH).
    cat >"$a_home/remote-hosts.toml" <<EOF
[[hosts]]
name = "node-b"
host = "node-b"
ssh_user = "root"
auth_kind = "key"
key_path = "/root/.ssh/unused"
remote_shell = "posix"
last_remote_port = $NODE_PORT_B
tls_pin_sha256 = "$fp_b"
via = "auto"
EOF

    local connect_out
    connect_out=$(node_command "$NODE_A" "$NODE_PORT_A" "CONNECT_REMOTE_HOST node-b")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$connect_out" >/dev/null \
        || die "CONNECT_REMOTE_HOST on A failed: $connect_out"
    jq -r '.payload.message // ""' <<<"$connect_out" | grep -q "via direct" \
        || die "auto must report the direct path it took: $connect_out"
    log "connect answered: $(jq -r '.payload.message' <<<"$connect_out")"

    # The auto choice must be preserved with the effective path annotated,
    # not rewritten to direct.
    grep -q '^via = "auto"$' "$a_home/remote-hosts.toml" \
        || die "the via = auto choice must be preserved: $(cat "$a_home/remote-hosts.toml")"
    grep -q '^last_via_used = "direct"$' "$a_home/remote-hosts.toml" \
        || die "last_via_used must remember the direct path: $(cat "$a_home/remote-hosts.toml")"

    # A live remote session row on A, observed through the control socket —
    # the data-plane anchor that the auto->direct connect established a
    # usable session.
    local row
    row=$(wait_authority_row_online "$NODE_A" "$NODE_PORT_A" "node-b#$NODE_PORT_B")
    log "node A sees node B online over the auto->direct path: $row"

    rm -rf "$a_home" "$stage"
    log "viaautodirect OK: auto picked and remembered the direct path with the relay available"
}

scenario_viaautorelay() {
    log "scenario: viaautorelay (via = auto falls back to relay when direct is impossible)"
    # Isolated two-bridge topology (same construction as pastefile/presence):
    # A and B share no network, so the direct probe fails by construction
    # and the auto profile must fall back to the relay dial path.
    docker network create --internal "$NET_A" >/dev/null
    docker network create --internal "$NET_B" >/dev/null

    docker run -d --name "$RELAY" --network "$NET_A" --network-alias relay "$IMAGE" \
        waitagent relay serve --listen "$RELAY_LISTEN" >/dev/null
    docker network connect --alias relay "$NET_B" "$RELAY" >/dev/null

    token=$(docker exec "$RELAY" waitagent relay invite --listen "$RELAY_LISTEN" --deploy \
        | sed -n 's/^token: //p')
    [ -n "$token" ] || die "relay invite produced no token"

    local a_home stage
    a_home=$(mktemp -d)
    stage=$(mktemp -d)
    docker run -dt --name "$NODE_A" --network "$NET_A" \
        -v "$a_home:/root/.waitagent" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_A" >/dev/null

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

    docker run -dt --name "$NODE_B" --network "$NET_B" \
        -v "$stage/node-a.pub:/root/.waitagent/authorized_operators/node-a.pub:ro" "$IMAGE" \
        sh -c "waitagent relay join relay:$RELAY_PORT '$token' && exec waitagent --port $NODE_PORT_B" >/dev/null

    mapfile -t FINGERPRINTS < <(docker exec "$RELAY" sh -c 'ls /root/.waitagent/authorized_nodes')
    [ "${#FINGERPRINTS[@]}" -eq 2 ] \
        || die "expected 2 whitelisted nodes, got ${#FINGERPRINTS[@]}: ${FINGERPRINTS[*]:-<none>}"
    wait_for_node_ids "${FINGERPRINTS[@]}"

    local fp_b
    fp_b=$(docker exec "$NODE_B" waitagent __generate-node-credentials \
        | sed -n 's/^WAITAGENT_CREDENTIALS\([0-9a-f]\{64\}\):.*/\1/p')
    [ -n "$fp_b" ] || die "could not read node B's fingerprint"

    # Auto profile on the relay-only topology.
    cat >"$a_home/remote-hosts.toml" <<EOF
[[hosts]]
name = "node-b"
host = "node-b"
ssh_user = "root"
auth_kind = "key"
key_path = "/root/.ssh/unused"
remote_shell = "posix"
last_remote_port = $NODE_PORT_B
tls_pin_sha256 = "$fp_b"
via = "auto"
EOF

    local connect_out
    connect_out=$(node_command "$NODE_A" "$NODE_PORT_A" "CONNECT_REMOTE_HOST node-b")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$connect_out" >/dev/null \
        || die "CONNECT_REMOTE_HOST on A failed: $connect_out"
    jq -r '.payload.message // ""' <<<"$connect_out" | grep -q "via relay" \
        || die "auto must report the relay fallback it took: $connect_out"
    log "connect answered: $(jq -r '.payload.message' <<<"$connect_out")"

    grep -q '^via = "auto"$' "$a_home/remote-hosts.toml" \
        || die "the via = auto choice must be preserved: $(cat "$a_home/remote-hosts.toml")"
    grep -q '^last_via_used = "relay"$' "$a_home/remote-hosts.toml" \
        || die "last_via_used must remember the relay path: $(cat "$a_home/remote-hosts.toml")"

    local row
    row=$(wait_authority_row_online "$NODE_A" "$NODE_PORT_A" "node-b#$NODE_PORT_B")
    log "node A sees node B online over the auto->relay path: $row"

    rm -rf "$a_home" "$stage"
    log "viaautorelay OK: auto fell back to and remembered the relay path"
}

scenarios=("$@")
if [ "${#scenarios[@]}" -eq 0 ]; then
    scenarios=(smoke reconnect reregister streams direct pastefile revoke capacity presence joinkeypaths relaymgmt viaautodirect viaautorelay)
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
        pastefile)
            scenario_pastefile
            ;;
        revoke)
            scenario_revoke
            ;;
        capacity)
            scenario_capacity
            ;;
        presence)
            scenario_presence
            ;;
        joinkeypaths)
            scenario_joinkeypaths
            ;;
        relaymgmt)
            scenario_relaymgmt
            ;;
        viaautodirect)
            scenario_viaautodirect
            ;;
        viaautorelay)
            scenario_viaautorelay
            ;;
        *)
            die "unknown scenario '$scenario' (known: smoke reconnect reregister streams direct pastefile revoke capacity presence joinkeypaths relaymgmt viaautodirect viaautorelay)"
            ;;
    esac
    log "scenario '$scenario' passed"
done
