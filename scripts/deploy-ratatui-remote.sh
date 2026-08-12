#!/usr/bin/env bash
set -euo pipefail

# Deploy the locally-built waitagent binary to a remote host and start it as a
# ratatui node server.
#
# In inbound mode the remote daemon connects back to the local WaitAgent. In
# outbound-dial mode (omit --connect) the daemon listens for the control host to
# dial in.
#
# This script is invoked by the SSH bootstrapper instead of downloading the
# release installer from the network. It copies target/release/waitagent to the
# remote host and kills any existing remote node server for the same port. The
# bootstrapper generates credentials and starts the daemon separately, so this
# script must not start a daemon before the key and certificate exist.
#
# Usage:
#   ./scripts/deploy-ratatui-remote.sh \
#     --host <remote-host> \
#     --user <ssh-user> \
#     --remote-port <remote-port> \
#     --node-id <authority-node-id> \
#     [--connect <local-endpoint>] \
#     [--ssh-port <ssh-port>] \
#     [--identity <ssh-key-path>] \
#     [--local-bin <path>] \
#     [--remote-bin <path>] \
#     [--node-key-path <path>] \
#     [--node-cert-path <path>]
#
# Authentication:
#   - Key-based: pass --identity <path> (preferred).
#   - Password-based: set WAITAGENT_SSH_PASSWORD in the environment. The script
#     uses sshpass(1) if it is installed; otherwise password auth is unsupported.

LOCAL_BIN="${WAITAGENT_LOCAL_BIN:-target/release/waitagent}"
if [[ -n "${WAITAGENT_REMOTE_BIN:-}" ]]; then
  REMOTE_BIN="$WAITAGENT_REMOTE_BIN"
else
  REMOTE_BIN='$HOME/.local/bin/waitagent'
fi
SSH_PORT="22"
HOST=""
USER=""
IDENTITY=""
REMOTE_PORT=""
CONNECT=""
NODE_ID=""
NODE_KEY_PATH=""
NODE_CERT_PATH=""

usage() {
  cat >&2 <<'EOF'
Usage: deploy-ratatui-remote.sh --host <host> --user <user> --remote-port <port> --node-id <id> [options]

Required:
  --host          remote SSH host
  --user          remote SSH user
  --remote-port   port the remote daemon will listen on
  --node-id       authority node id for the remote daemon

Options:
  --connect       local WaitAgent endpoint for inbound mode (omit for outbound dial)
  --ssh-port      SSH port (default: 22)
  --identity      SSH private key path
  --local-bin     path to local waitagent binary (default: target/release/waitagent)
  --remote-bin    remote install path (default: $HOME/.local/bin/waitagent)
  --node-key-path TLS private key path on the remote host
  --node-cert-path TLS certificate path on the remote host

Environment:
  WAITAGENT_SSH_PASSWORD    SSH password (only used when --identity is omitted)
  WAITAGENT_LOCAL_BIN       default for --local-bin
  WAITAGENT_REMOTE_BIN      default for --remote-bin
EOF
}

# Quote a value for the remote shell. Single quotes prevent expansion, which is
# what we want now that remote paths are resolved to absolute values before use.
shq() {
  printf '%q' "$1"
}

# Resolve $HOME or ~ at the start of a remote path by querying the remote host.
resolve_remote_path() {
  local path="$1"
  if [[ "$path" == '$HOME/'* || "$path" == '~/'* ]]; then
    local remote_home
    remote_home=$(remote 'echo $HOME')
    if [[ -z "$remote_home" ]]; then
      echo "error: failed to resolve remote home directory" >&2
      exit 1
    fi
    if [[ "$path" == '$HOME/'* ]]; then
      path="${remote_home}/${path#'$HOME'/}"
    else
      path="${remote_home}/${path#~/}"
    fi
  fi
  printf '%s' "$path"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host)
      HOST="${2:-}"
      shift 2
      ;;
    --user)
      USER="${2:-}"
      shift 2
      ;;
    --ssh-port)
      SSH_PORT="${2:-22}"
      shift 2
      ;;
    --identity)
      IDENTITY="${2:-}"
      shift 2
      ;;
    --local-bin)
      LOCAL_BIN="${2:-}"
      shift 2
      ;;
    --remote-bin)
      REMOTE_BIN="${2:-}"
      shift 2
      ;;
    --remote-port)
      REMOTE_PORT="${2:-}"
      shift 2
      ;;
    --connect)
      CONNECT="${2:-}"
      shift 2
      ;;
    --node-key-path)
      NODE_KEY_PATH="${2:-}"
      shift 2
      ;;
    --node-cert-path)
      NODE_CERT_PATH="${2:-}"
      shift 2
      ;;
    --node-id)
      NODE_ID="${2:-}"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage
      exit 1
      ;;
  esac
done

if [[ -z "$HOST" || -z "$USER" || -z "$REMOTE_PORT" || -z "$NODE_ID" ]]; then
  echo "error: missing required argument" >&2
  usage
  exit 1
fi

if [[ ! -f "$LOCAL_BIN" ]]; then
  echo "error: local binary not found: $LOCAL_BIN" >&2
  exit 1
fi

SSH_PASSWORD="${WAITAGENT_SSH_PASSWORD:-}"

if [[ -n "$IDENTITY" && -n "$SSH_PASSWORD" ]]; then
  echo "warning: both --identity and WAITAGENT_SSH_PASSWORD are set; using key authentication" >&2
fi

# Build SSH/SCP base options.
SSH_OPTS=(-p "$SSH_PORT" -o StrictHostKeyChecking=accept-new -o BatchMode=no)
SCP_OPTS=(-P "$SSH_PORT" -o StrictHostKeyChecking=accept-new)
if [[ -n "$IDENTITY" ]]; then
  SSH_OPTS+=(-i "$IDENTITY")
  SCP_OPTS+=(-i "$IDENTITY")
fi

if [[ -n "$SSH_PASSWORD" ]]; then
  if ! command -v sshpass >/dev/null 2>&1; then
    echo "error: WAITAGENT_SSH_PASSWORD is set but sshpass(1) is not installed" >&2
    exit 1
  fi
  SSH=(sshpass -p "$SSH_PASSWORD" ssh)
  SCP=(sshpass -p "$SSH_PASSWORD" scp)
else
  SSH=(ssh)
  SCP=(scp)
fi

REMOTE="$USER@$HOST"
LOG_FILE="/tmp/waitagent-$REMOTE_PORT.log"

remote() {
  "${SSH[@]}" "${SSH_OPTS[@]}" "$REMOTE" "$1"
}

# Resolve default remote paths to absolute values so quoting is simple and
# scp/ssh agree on the target location.
REMOTE_BIN=$(resolve_remote_path "$REMOTE_BIN")
TMP_REMOTE_BIN="$REMOTE_BIN.tmp.$$"

ensure_remote_dir() {
  remote "mkdir -p \"\$(dirname $(shq "$REMOTE_BIN"))\""
}

copy_binary() {
  "${SCP[@]}" "${SCP_OPTS[@]}" "$LOCAL_BIN" "$REMOTE:$(shq "$TMP_REMOTE_BIN")"
  remote "mv -f $(shq "$TMP_REMOTE_BIN") $(shq "$REMOTE_BIN") && chmod 755 $(shq "$REMOTE_BIN")"
}

kill_existing() {
  remote "
    set -e
    pkill -f $(shq "waitagent.*--port $REMOTE_PORT.*__ratatui-node-server") 2>/dev/null || true
  "
}

main() {
  ensure_remote_dir
  copy_binary
  kill_existing
  echo "Deployed $LOCAL_BIN -> $REMOTE:$REMOTE_BIN"
  echo "Remote binary ready on $REMOTE (port $REMOTE_PORT)"
}

main
