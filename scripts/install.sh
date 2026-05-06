#!/usr/bin/env bash
set -euo pipefail

# WaitAgent install script.
# Usage: curl -fsSL https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh | bash

INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
VERSION="${VERSION:-latest}"
tmpdir=""

REPO="kikakkz/wait-agent"

# --- Platform detection ---
detect_platform() {
  local os arch

  case "$(uname -s)" in
    Linux)  os="linux" ;;
    Darwin) os="macos" ;;
    *)
      echo "error: unsupported OS: $(uname -s)" >&2
      exit 1
      ;;
  esac

  case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *)
      echo "error: unsupported architecture: $(uname -m)" >&2
      exit 1
      ;;
  esac

  # macOS only supports aarch64 builds (Apple Silicon).
  if [[ "$os" == "macos" && "$arch" == "x86_64" ]]; then
    echo "error: WaitAgent for macOS is only available for Apple Silicon (aarch64)." >&2
    echo "  On Intel Macs, build from source: https://github.com/kikakkz/wait-agent#build-from-source" >&2
    exit 1
  fi

  echo "${os}-${arch}"
}

# --- Resolve version ---
resolve_version() {
  if [[ "$VERSION" == "latest" ]]; then
    local api_url="https://api.github.com/repos/${REPO}/releases/latest"
    local tag
    tag="$(curl -fsSL "$api_url" 2>/dev/null \
      | grep '"tag_name":' \
      | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
    if [[ -z "$tag" ]]; then
      echo "error: failed to fetch latest release from GitHub API" >&2
      exit 1
    fi
    echo "${tag#v}"
  else
    echo "$VERSION"
  fi
}

# --- Main ---
main() {
  local platform version url tarball

  platform="$(detect_platform)"
  version="$(resolve_version)"

  echo ">>> WaitAgent ${version} for ${platform}"

  case "$platform" in
    linux-x86_64)
      tarball="waitagent-${version}-x86_64-linux.tar.gz"
      ;;
    macos-aarch64)
      tarball="waitagent-${version}-aarch64-macos.tar.gz"
      ;;
    *)
      echo "error: unsupported platform: ${platform}" >&2
      exit 1
      ;;
  esac

  url="https://github.com/${REPO}/releases/download/v${version}/${tarball}"

  echo ">>> Downloading ${url}"

  # Download to a temp directory
  tmpdir="$(mktemp -d)"
  trap 'rm -rf "$tmpdir"' EXIT

  curl -fsSL "$url" -o "${tmpdir}/${tarball}"

  echo ">>> Extracting..."
  tar xzf "${tmpdir}/${tarball}" -C "$tmpdir"

  local binary="${tmpdir}/waitagent"
  if [[ ! -f "$binary" ]]; then
    echo "error: waitagent binary not found in archive" >&2
    exit 1
  fi

  echo ">>> Installing waitagent to ${INSTALL_DIR}"
  mkdir -p "$INSTALL_DIR"

  if [[ -n "${PREFIX:-}" ]]; then
    # User-mode install under PREFIX
    mkdir -p "${PREFIX}${INSTALL_DIR}"
    cp "$binary" "${PREFIX}${INSTALL_DIR}/waitagent"
    chmod 755 "${PREFIX}${INSTALL_DIR}/waitagent"
  elif [[ "$INSTALL_DIR" == /usr/local/bin && "$(id -u)" -ne 0 ]]; then
    # System install without root: use sudo
    echo ">>> (sudo required for ${INSTALL_DIR})"
    sudo cp "$binary" "${INSTALL_DIR}/waitagent"
    sudo chmod 755 "${INSTALL_DIR}/waitagent"
  else
    cp "$binary" "${INSTALL_DIR}/waitagent"
    chmod 755 "${INSTALL_DIR}/waitagent"
  fi

  echo ""
  echo "✔  waitagent ${version} installed to ${INSTALL_DIR}/waitagent"
  echo ""
  echo "To get started:"
  echo "  waitagent --help"
}

main
