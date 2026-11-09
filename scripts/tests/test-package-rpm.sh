#!/usr/bin/env bash
# Hermetic regression test for scripts/package-rpm.sh (issue #258).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

printf '#!/bin/sh\necho waitagent\n' > "$WORK/waitagent-payload"
chmod +x "$WORK/waitagent-payload"

STUB="$WORK/bin"
mkdir -p "$STUB"
cat > "$STUB/rpmbuild" <<'STUBEOF'
#!/usr/bin/env bash
set -euo pipefail
spec=""
topdir=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --define)
      topdir="${2#_topdir }"
      shift 2
      ;;
    -bb)
      spec="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
[[ -n "$spec" && -n "$topdir" ]] || { echo "stub: bad args" >&2; exit 2; }

name="$(awk '/^Name:/{print $2; exit}' "$spec")"
version="$(awk '/^Version:/{print $2; exit}' "$spec")"
builddir="$topdir/BUILD/$name-$version-build"
buildroot="$topdir/BUILDROOT/$name-$version-1.x86_64"
mkdir -p "$builddir" "$buildroot"

install_section="$(awk '/^%install/{flag=1;next}/^%[a-z]/{flag=0}flag' "$spec")"
[[ -n "$install_section" ]] || { echo "stub: no %install section" >&2; exit 2; }

# rpmbuild runs %install via /bin/sh -e from the build dir with %{buildroot}
# expanded. rpm >= 4.20 uses a per-package build dir (<name>-<version>-build)
# and resolves %{_builddir} there, so a spec referencing %{_builddir} breaks.
printf '%s\n' "${install_section//%\{buildroot\}/$buildroot}" > "$builddir/install.sh"
if ! (cd "$builddir" && sh -e install.sh); then
  echo "stub: %install failed" >&2
  exit 1
fi

[[ -x "$buildroot/usr/bin/waitagent" ]] || {
  echo "stub: %{buildroot}/usr/bin/waitagent missing after %install" >&2
  exit 1
}
mkdir -p "$topdir/RPMS/x86_64"
echo "stub rpm" > "$topdir/RPMS/x86_64/$name-$version-1.x86_64.rpm"
STUBEOF
chmod +x "$STUB/rpmbuild"

PATH="$STUB:$PATH" "$REPO_ROOT/scripts/package-rpm.sh" \
  "$WORK/waitagent-payload" "$WORK/out.rpm" "0.1.100"
[[ -f "$WORK/out.rpm" ]] || { echo "FAIL: output rpm missing" >&2; exit 1; }

PATH="$STUB:$PATH" "$REPO_ROOT/scripts/package-rpm.sh" \
  "$WORK/waitagent-payload" "$WORK/out2.rpm" "9.8.7"
[[ -f "$WORK/out2.rpm" ]] || { echo "FAIL: output rpm missing (case 2)" >&2; exit 1; }

echo "test-package-rpm: OK"
