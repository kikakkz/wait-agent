#!/usr/bin/env bash
set -euo pipefail

# Deploy the locally-built waitagent binary to a remote host and start it as a
# ratatui node server that connects back to the local WaitAgent.
#
# This script is invoked by the SSH bootstrapper instead of downloading the
# release installer from the network. It copies target/release/waitagent to the
# remote host, kills any existing remote node server for the same port, and
# starts a fresh one in a detached tmux session so the process has a stable TTY
# and can be inspected later with `tmux attach -t waitagent-<port>`.
#
# Usage:
#   ./scripts/deploy-ratatui-remote.sh \
#     --host <remote-host> \
#     --user <ssh-user> \
#     --remote-port <remote-port> \
#     --connect <local-endpoint> \
#     --node-id <authority-node-id> \
#     [--ssh-port <ssh-port>] \
#     [--identity <ssh-key-path>] \
#     [--local-bin <path>] \
#     [--remote-bin <path>]
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

usage() {
  cat >&2 <<'EOF'
Usage: deploy-ratatui-remote.sh --host <host> --user <user> --remote-port <port> --connect <endpoint> --node-id <id> [options]

Required:
  --host          remote SSH host
  --user          remote SSH user
  --remote-port   port the remote daemon will listen on
  --connect       local WaitAgent endpoint the remote daemon connects back to
  --node-id       authority node id for the remote daemon

Options:
  --ssh-port      SSH port (default: 22)
  --identity      SSH private key path
  --local-bin     path to local waitagent binary (default: target/release/waitagent)
  --remote-bin    remote install path (default: $HOME/.local/bin/waitagent)

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

if [[ -z "$HOST" || -z "$USER" || -z "$REMOTE_PORT" || -z "$CONNECT" || -z "$NODE_ID" ]]; then
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
SESSION_NAME="waitagent-$REMOTE_PORT"

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
    if tmux has-session -t $(shq "$SESSION_NAME") 2>/dev/null; then
      tmux kill-session -t $(shq "$SESSION_NAME")
    fi
  "
}

start_daemon() {
  # Build the start command. We run the ratatui node server on the remote host
  # so it connects back to the local WaitAgent using the ratatui session backend
  # instead of the tmux-based __remote-daemon.
  local start_cmd
  start_cmd="exec $(shq "$REMOTE_BIN") --ratatui --port $(shq "$REMOTE_PORT") --connect $(shq "$CONNECT") --node-id $(shq "$NODE_ID") __ratatui-node-server"

  local start_script
  start_script="/tmp/waitagent-start-$REMOTE_PORT-$$.sh"

  # Upload a small start script and execute it inside a detached tmux session.
  "${SSH[@]}" "${SSH_OPTS[@]}" "$REMOTE" "cat > $(shq "$start_script")" <<EOF
#!/bin/bash
$start_cmd
EOF
  remote "
    chmod 755 $(shq "$start_script")
    tmux new-session -d -s $(shq "$SESSION_NAME") $(shq "$start_script")
  "
}

main() {
  ensure_remote_dir
  copy_binary
  kill_existing
  start_daemon
  echo "Deployed $LOCAL_BIN -> $REMOTE:$REMOTE_BIN"
  echo "Started tmux session '$SESSION_NAME' on $REMOTE"
}

main
