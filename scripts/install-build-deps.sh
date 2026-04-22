#!/usr/bin/env bash
set -euo pipefail

MODE="install"
if [[ "${1:-}" == "--print" ]]; then
  MODE="print"
elif [[ "${1:-}" == "--help" ]]; then
  cat <<'EOF'
Usage:
  ./scripts/install-build-deps.sh
  ./scripts/install-build-deps.sh --print

Installs the system packages required to build waitagent, including vendored tmux.
`--print` shows the command without executing it.
EOF
  exit 0
fi

run_cmd() {
  if [[ "$MODE" == "print" ]]; then
    printf '%s\n' "$*"
    return 0
  fi
  "$@"
}

sudo_run() {
  if [[ "$EUID" -eq 0 ]]; then
    run_cmd "$@"
  else
    run_cmd sudo "$@"
  fi
}

detect_linux_id() {
  if [[ -r /etc/os-release ]]; then
    . /etc/os-release
    printf '%s\n' "${ID:-}"
    return 0
  fi
  return 1
}

if command -v brew >/dev/null 2>&1; then
  run_cmd brew install bison pkg-config libevent ncurses automake autoconf
  exit 0
fi

LINUX_ID="$(detect_linux_id || true)"
case "$LINUX_ID" in
  ubuntu|debian)
    sudo_run apt-get update
    sudo_run apt-get install -y \
      bison \
      pkg-config \
      libevent-dev \
      libncurses-dev \
      build-essential \
      automake \
      autoconf
    ;;
  fedora)
    sudo_run dnf install -y \
      bison \
      pkgconf-pkg-config \
      libevent-devel \
      ncurses-devel \
      gcc \
      make \
      automake \
      autoconf
    ;;
  arch|manjaro)
    sudo_run pacman -Sy --needed \
      bison \
      pkgconf \
      libevent \
      ncurses \
      base-devel \
      automake \
      autoconf
    ;;
  alpine)
    sudo_run apk add \
      bison \
      pkgconf \
      libevent-dev \
      ncurses-dev \
      build-base \
      automake \
      autoconf
    ;;
  opensuse*|sles)
    sudo_run zypper install -y \
      bison \
      pkg-config \
      libevent-devel \
      ncurses-devel \
      gcc \
      make \
      automake \
      autoconf
    ;;
  *)
    cat >&2 <<'EOF'
Unsupported platform for automatic dependency install.
Install these tools manually, then rerun cargo build:
  bison or yacc
  pkg-config
  libevent development headers
  ncurses development headers
  cc
  make
  automake
  autoconf
EOF
    exit 1
    ;;
esac
