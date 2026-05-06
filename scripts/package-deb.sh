#!/usr/bin/env bash
set -euo pipefail

# Build a .deb package for waitagent using dpkg-deb.
# Usage: ./scripts/package-deb.sh <binary-path> <output-deb> [version]

BINARY="${1:?missing binary path}"
OUTPUT_DEB="${2:?missing output deb path}"
VERSION="${3:-0.1.0}"

if [[ ! -f "$BINARY" ]]; then
  echo "error: binary not found at $BINARY" >&2
  exit 1
fi

STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT

PKG_ROOT="$STAGING/waitagent_${VERSION}_amd64"
mkdir -p "$PKG_ROOT/DEBIAN"
mkdir -p "$PKG_ROOT/usr/bin"

cp "$BINARY" "$PKG_ROOT/usr/bin/waitagent"
chmod 755 "$PKG_ROOT/usr/bin/waitagent"

cat > "$PKG_ROOT/DEBIAN/control" <<EOF
Package: waitagent
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: amd64
Maintainer: kikakkz <kikakkz@users.noreply.github.com>
Depends: libevent-2.1-7 | libevent-2.1-6, libncurses6 | libncurses5
Description: Terminal-native interaction scheduler for multi-agent workflows
 WaitAgent is a terminal multiplexer and workspace manager that lets
 multiple AI agent sessions share one terminal. It provides session
 management, remote node aggregation, and tmux-native workspace UI.
Homepage: https://github.com/kikakkz/wait-agent
EOF

dpkg-deb --root-owner-group --build "$PKG_ROOT" "$OUTPUT_DEB"
echo "created $OUTPUT_DEB"
