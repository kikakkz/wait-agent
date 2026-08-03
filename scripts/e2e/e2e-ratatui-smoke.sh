#!/usr/bin/env bash
set -euo pipefail

# Ratatui smoke E2E: start a local ratatui node server, create a local session,
# verify it appears in LIST_SESSIONS, and clean up.

PORT=17474
UID_VAL="$(id -u)"
SOCK_DIR="/tmp/waitagent-ratatui-${UID_VAL}"
SOCK_PATH="${SOCK_DIR}/${PORT}.sock"
BIN="${BIN:-./target/release/waitagent}"
TIMEOUT_SECS=10

step() { echo "== $* =="; }

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  rm -f "${SOCK_PATH}"
}
trap cleanup EXIT

step "building release binary if needed"
if [[ ! -x "${BIN}" ]]; then
  cargo build --release
fi

step "cleaning previous server state"
mkdir -p "${SOCK_DIR}"
rm -f "${SOCK_PATH}"

step "starting ratatui node server on port ${PORT}"
"${BIN}" --port "${PORT}" __ratatui-node-server &
SERVER_PID=$!

step "waiting for node socket"
DEADLINE="$(($(date +%s) + TIMEOUT_SECS))"
while [[ ! -S "${SOCK_PATH}" ]] && [[ "$(date +%s)" -lt "${DEADLINE}" ]]; do
  if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
    echo "error: node server exited early" >&2
    exit 1
  fi
  sleep 0.1
done

if [[ ! -S "${SOCK_PATH}" ]]; then
  echo "error: node socket did not appear within ${TIMEOUT_SECS}s" >&2
  exit 1
fi

# Give the server a moment to finish initialization.
sleep 0.2

send_cmd() {
  python3 - "$1" "${SOCK_PATH}" <<'PY'
import socket, sys
cmd = sys.argv[1]
path = sys.argv[2]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(path)
sock.sendall((cmd + '\n').encode())
response = b''
while b'\n' not in response:
    chunk = sock.recv(4096)
    if not chunk:
        break
    response += chunk
sys.stdout.write(response.decode())
sock.close()
PY
}

step "creating local session"
CREATE_RESP="$(send_cmd "CREATE_LOCAL_SESSION")"
echo "create response: ${CREATE_RESP}"
if ! echo "${CREATE_RESP}" | jq -e '.type == "Response" and .payload.ok' >/dev/null; then
  echo "error: CREATE_LOCAL_SESSION failed: ${CREATE_RESP}" >&2
  exit 1
fi

step "listing sessions"
LIST_RESP="$(send_cmd "LIST_SESSIONS")"
echo "list response: ${LIST_RESP}"

SESSION_COUNT="$(echo "${LIST_RESP}" | jq -r '.payload.data | length')"
if [[ "${SESSION_COUNT}" -lt 1 ]]; then
  echo "error: expected at least one session, got ${SESSION_COUNT}" >&2
  exit 1
fi

step "verifying session fields"
echo "${LIST_RESP}" | jq -e '.payload.data[0].id and .payload.data[0].transport == "local"' >/dev/null

step "stopping node server"
STOP_RESP="$(send_cmd "STOP")"
echo "stop response: ${STOP_RESP}"

# Wait briefly for the server process to exit.
for _ in {1..20}; do
  if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

step "ratatui smoke E2E passed"
