#!/usr/bin/env bash
set -euo pipefail

# Build a .rpm package for waitagent using rpmbuild.
# Usage: ./scripts/package-rpm.sh <binary-path> <output-rpm> [version]

BINARY="${1:?missing binary path}"
OUTPUT_RPM="${2:?missing output rpm path}"
VERSION="${3:-0.1.0}"

if [[ ! -f "$BINARY" ]]; then
  echo "error: binary not found at $BINARY" >&2
  exit 1
fi

RPMBUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$RPMBUILD_DIR"' EXIT

mkdir -p "$RPMBUILD_DIR"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
mkdir -p "$RPMBUILD_DIR"/BUILD/waitagent-${VERSION}/usr/bin

cp "$BINARY" "$RPMBUILD_DIR/BUILD/waitagent-${VERSION}/usr/bin/waitagent"
chmod 755 "$RPMBUILD_DIR/BUILD/waitagent-${VERSION}/usr/bin/waitagent"

cat > "$RPMBUILD_DIR/SPECS/waitagent.spec" <<EOF
Name: waitagent
Version: ${VERSION}
Release: 1
Summary: Terminal-native interaction scheduler for multi-agent workflows
License: MIT
URL: https://github.com/kikakkz/wait-agent
Group: Applications/System
Requires: libevent, ncurses-libs
BuildRoot: %{_tmppath}/%{name}-%{version}-root

%description
WaitAgent is a terminal multiplexer and workspace manager that lets
multiple AI agent sessions share one terminal. It provides session
management, remote node aggregation, and tmux-native workspace UI.

%install
mkdir -p %{buildroot}/usr/bin
install -m 755 %{_builddir}/waitagent-%{version}/usr/bin/waitagent %{buildroot}/usr/bin/waitagent

%files
/usr/bin/waitagent

%clean
rm -rf %{buildroot}

%post
/sbin/ldconfig 2>/dev/null || true
EOF

rpmbuild --define "_topdir $RPMBUILD_DIR" -bb "$RPMBUILD_DIR/SPECS/waitagent.spec"

find "$RPMBUILD_DIR/RPMS" -name '*.rpm' -exec cp {} "$OUTPUT_RPM" \;
echo "created $OUTPUT_RPM"
