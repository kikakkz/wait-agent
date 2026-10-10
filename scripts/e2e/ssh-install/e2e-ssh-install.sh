#!/bin/bash
# Install-source docker e2e harness (issue #168).
#
# Topology: one control-node container (the waitagent runtime image) and one
# sshd container on a single INTERNAL docker bridge. The sshd host has no
# waitagent binary and no outbound network access by construction, so the
# only way a connect can succeed is the LocalUpload install source: the
# control node serves the artifact from its primed ~/.waitagent/cache and
# uploads it over the SSH exec channel.
#
# Coverage scenarios:
#   installsource — profile with install_source = "upload":
#                   1. connect bootstraps the remote host end to end: arch
#                      detection exec, cached-artifact upload (base64 chunks
#                      over SSH exec), remote install into /usr/local/bin,
#                      credentials, daemon start, online session row;
#                   2. cache hit: after killing the remote daemon and
#                      deleting the remote binary, a second connect installs
#                      again WITHOUT re-downloading — asserted via the local
#                      artifact mtime.
#
# Images: WA_E2E_BINARY stages the prebuilt host binary into the runtime
# image via scripts/e2e/relay/Dockerfile.runtime (same as the relay harness);
# the sshd image is built locally from Dockerfile.sshd.
#
# Usage: e2e-ssh-install.sh [installsource]
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)
IMAGE=waitagent-e2e:local
SSHD_IMAGE=waitagent-e2e-sshd:local
NODE_PORT=9001
GATE_TIMEOUT_SECS=${GATE_TIMEOUT_SECS:-180}

log() { printf '[e2e-ssh-install] %s\n' "$*"; }
die() {
    log "ERROR: $*"
    local name
    for name in "$SSH_HOST" "$NODE"; do
        if docker inspect "$name" >/dev/null 2>&1; then
            log "$name logs (tail):"
            docker logs --tail 20 "$name" 2>&1 | sed 's/^/  /' || true
        fi
    done
    exit 1
}

RUN=${RUN:-$RANDOM$RANDOM}
SSH_HOST="wa168-ssh-$RUN"
NODE="wa168-node-$RUN"
NET="wa168-net-$RUN"
STAGE=$(mktemp -d)

command -v docker >/dev/null 2>&1 || die "docker is required"
command -v jq >/dev/null 2>&1 || die "jq is required for host-side status parsing"
command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen is required for the e2e key"

cleanup() {
    docker rm -f "$SSH_HOST" "$NODE" >/dev/null 2>&1 || true
    docker network rm "$NET" >/dev/null 2>&1 || true
    rm -rf "$STAGE"
}
trap cleanup EXIT INT TERM

build_images() {
    if [ -n "${WA_E2E_BINARY:-}" ]; then
        [ -f "$WA_E2E_BINARY" ] || die "WA_E2E_BINARY not found: $WA_E2E_BINARY"
        cp "$WA_E2E_BINARY" "$STAGE/waitagent"
        docker build -q -f "$SCRIPT_DIR/../relay/Dockerfile.runtime" -t "$IMAGE" "$STAGE" >/dev/null
    else
        log "building $IMAGE (in-docker release build; set WA_E2E_BINARY to skip)"
        docker build -q -f "$SCRIPT_DIR/../relay/Dockerfile" -t "$IMAGE" "$REPO_ROOT" >/dev/null
    fi
    docker build -q -f "$SCRIPT_DIR/Dockerfile.sshd" -t "$SSHD_IMAGE" "$SCRIPT_DIR" >/dev/null
}

node_command() {
    # Sends a control command to the node server inside the node container.
    docker exec "$NODE" waitagent __node-command "$NODE_PORT" "$1"
}

# Polls the node control socket until it answers STATUS with ok=true (the
# relay harness proved startup races die without this gate).
wait_for_node_control() {
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    while ((SECONDS < deadline)); do
        if node_command STATUS 2>/dev/null \
            | jq -e '.type == "Response" and .payload.ok == true' >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    die "node control socket never became ready"
}

# Polls LIST_SESSIONS until a remote session row for an authority matching
# $1 is online. Echoes the matching row.
wait_for_online_remote_row() {
    local authority_prefix=$1
    local deadline=$((SECONDS + GATE_TIMEOUT_SECS))
    local row=""
    while ((SECONDS < deadline)); do
        row=$(node_command LIST_SESSIONS 2>/dev/null \
            | jq -c "[.payload.data[]?
                | select(.transport == \"remote\"
                    and (.authority_node_id | startswith(\"$authority_prefix\"))
                    and .availability == \"online\")]
                | first" 2>/dev/null || true)
        if [ -n "$row" ] && [ "$row" != "null" ]; then
            printf '%s' "$row"
            return 0
        fi
        row=""
        sleep 2
    done
    return 1
}

bring_up_topology() {
    log "building images"
    build_images

    log "generating e2e ssh key"
    ssh-keygen -q -t ed25519 -N '' -f "$STAGE/e2e_key"

    log "creating internal network $NET (no outbound access by construction)"
    docker network create --internal "$NET" >/dev/null

    log "starting sshd host $SSH_HOST"
    # --cap-add NET_ADMIN: install.sh grants cap_net_admin+ep and the exec
    # of a file-capped binary requires the capability in the container's
    # bounding set (real hosts have it); without this the capped binary
    # cannot exec at all (EPERM).
    docker run -d --name "$SSH_HOST" --network "$NET" --network-alias ssh-host \
        --cap-add NET_ADMIN \
        -v "$STAGE/e2e_key.pub:/etc/e2e/authorized_keys:ro" \
        "$SSHD_IMAGE" >/dev/null

    log "starting control node $NODE"
    # -t: the node server initializes a ratatui TUI and needs a pty; there is
    # no headless mode (same constraint as the relay harness).
    docker run -dt --name "$NODE" --network "$NET" "$IMAGE" \
        sh -c "exec waitagent --port $NODE_PORT" >/dev/null
    wait_for_node_control

    docker exec "$NODE" mkdir -p /root/.waitagent/cache /root/.ssh
    docker cp "$STAGE/e2e_key" "$NODE:/root/.ssh/e2e_key" >/dev/null

    log "seeding remote-hosts.toml with an upload-source profile"
    docker exec -i "$NODE" sh -c 'cat > /root/.waitagent/remote-hosts.toml' <<'EOF'
[[hosts]]
name = "ssh-host"
host = "ssh-host"
ssh_user = "root"
auth_kind = "key"
key_path = "/root/.ssh/e2e_key"
preferred_remote_port = "auto"
use_install_proxy = false
host_kind = "lan"
install_source = "upload"
EOF

    WA_VERSION=$(docker exec "$NODE" waitagent --version | awk 'NR==1 {print $2}')
    [ -n "$WA_VERSION" ] || die "could not read the node version"
    log "priming the artifact cache with waitagent $WA_VERSION (x86_64-linux)"
    docker exec "$NODE" sh -c "
        set -e
        tar czf /root/.waitagent/cache/waitagent-$WA_VERSION-x86_64-linux.tar.gz -C /usr/local/bin waitagent
        cd /root/.waitagent/cache
        sha256sum waitagent-$WA_VERSION-x86_64-linux.tar.gz > waitagent-$WA_VERSION-x86_64-linux.tar.gz.sha256
    "
}

assert_remote_binary() {
    docker exec "$SSH_HOST" test -x /usr/local/bin/waitagent \
        || die "remote waitagent binary missing or not executable"
    local remote_version
    remote_version=$(docker exec "$SSH_HOST" /usr/local/bin/waitagent --version | awk 'NR==1 {print $2}')
    [ "$remote_version" = "$WA_VERSION" ] \
        || die "remote waitagent version $remote_version != uploaded $WA_VERSION"
}

scenario_installsource() {
    log "scenario: installsource (LocalUpload bootstrap + cache hit)"
    bring_up_topology
    local artifact=/root/.waitagent/cache/waitagent-$WA_VERSION-x86_64-linux.tar.gz

    log "phase 1: first connect uploads the cached artifact and starts the node"
    local connect_out
    connect_out=$(node_command "CONNECT_REMOTE_HOST ssh-host")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$connect_out" >/dev/null \
        || die "CONNECT_REMOTE_HOST was not accepted: $connect_out"

    local row
    row=$(wait_for_online_remote_row "ssh-host#") \
        || die "no online remote session row for ssh-host after the upload connect"
    log "online remote row: $row"
    assert_remote_binary
    docker exec "$SSH_HOST" pgrep -f __ratatui-node-server >/dev/null \
        || die "remote waitagent daemon is not running"
    log "phase 1 OK: artifact uploaded over SSH, installed, daemon online"

    log "phase 2: second connect must reuse the cache (no re-download)"
    # Force a fresh install: kill the daemon (defeats the reuse fast path)
    # and delete the binary (defeats the version gate). The artifact mtime
    # before/after proves the local cache satisfied the miss. The bracket
    # pattern keeps pkill from matching its own sh -c command line.
    docker exec "$SSH_HOST" sh -c 'pkill -f "[r]atatui-node-server"; rm -f /usr/local/bin/waitagent'
    sleep 1

    local before after
    before=$(docker exec "$NODE" stat -c %y "$artifact")
    connect_out=$(node_command "CONNECT_REMOTE_HOST ssh-host")
    jq -e '.type == "Response" and .payload.ok == true' <<<"$connect_out" >/dev/null \
        || die "second CONNECT_REMOTE_HOST was not accepted: $connect_out"

    row=$(wait_for_online_remote_row "ssh-host#") \
        || die "no online remote session row for ssh-host after the cache-hit connect"
    log "online remote row: $row"
    assert_remote_binary
    after=$(docker exec "$NODE" stat -c %y "$artifact")
    [ "$before" = "$after" ] \
        || die "artifact was re-downloaded on a cache hit (mtime $before -> $after)"
    log "phase 2 OK: second install reused the cached artifact (mtime unchanged: $after)"
}

scenario=${1:-installsource}
case "$scenario" in
    installsource)
        scenario_installsource
        ;;
    *)
        die "unknown scenario: $scenario (available: installsource)"
        ;;
esac

log "PASS: $scenario"
