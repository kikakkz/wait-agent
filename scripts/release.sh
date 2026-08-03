#!/bin/bash
# Release the current waitagent version and bump main to the next patch version.
#
# This script enforces the ordering from primitive.release-new-version:
#   1. The current Cargo.toml version is the version to release.
#   2. Tag v<current> on HEAD and push it to trigger the release workflow.
#   3. Bump Cargo.toml to the next patch version and push the bump commit.
#
# Usage: ./scripts/release.sh [--skip-continuity-check]

set -euo pipefail

cd "$(dirname "$0")/.."

SKIP_CONTINUITY_CHECK=false
for arg in "$@"; do
    case "$arg" in
        --skip-continuity-check)
            SKIP_CONTINUITY_CHECK=true
            ;;
        *)
            echo "error: unknown argument: $arg" >&2
            echo "usage: ./scripts/release.sh [--skip-continuity-check]" >&2
            exit 1
            ;;
    esac
done

if [[ -n "$(git status --short)" ]]; then
    echo "error: working directory is not clean" >&2
    git status --short >&2
    exit 1
fi

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
if [[ -z "$VERSION" ]]; then
    echo "error: could not read version from Cargo.toml" >&2
    exit 1
fi

TAG="v$VERSION"

if git rev-parse "$TAG" >/dev/null 2>&1; then
    echo "error: local tag $TAG already exists" >&2
    exit 1
fi

if git ls-remote --tags origin "$TAG" | grep -q "$TAG"; then
    echo "error: remote tag $TAG already exists on origin" >&2
    exit 1
fi

# Enforce version continuity: for 0.0.x releases, the current patch must be
# exactly one greater than the latest existing 0.0.x tag. This prevents tags
# like v0.0.191 being created when v0.0.190 was never released.
LATEST_0_0_TAG=$(git tag --list 'v0.0.*' --sort=-v:refname | head -n1 || true)
if [[ -n "$LATEST_0_0_TAG" ]]; then
    LATEST_PATCH=$(sed 's/^v0\.0\.//' <<<"$LATEST_0_0_TAG")
    EXPECTED_PATCH=$((LATEST_PATCH + 1))
    EXPECTED_VERSION="0.0.$EXPECTED_PATCH"
    if [[ "$VERSION" != "$EXPECTED_VERSION" ]]; then
        if [[ "$SKIP_CONTINUITY_CHECK" == true ]]; then
            echo "warning: skipping version continuity check" >&2
            echo "         latest released tag is $LATEST_0_0_TAG" >&2
            echo "         expected $EXPECTED_VERSION, but Cargo.toml has $VERSION" >&2
        else
            echo "error: version continuity check failed" >&2
            echo "       latest released tag is $LATEST_0_0_TAG" >&2
            echo "       expected Cargo.toml version to release is $EXPECTED_VERSION" >&2
            echo "       but Cargo.toml currently has $VERSION" >&2
            echo "       use --skip-continuity-check to override" >&2
            exit 1
        fi
    fi
fi

echo ">>> Releasing waitagent $VERSION"

echo ">>> cargo check"
cargo check

echo ">>> Tagging $TAG"
git tag -a "$TAG" -m "Release $TAG"

echo ">>> Pushing $TAG to origin"
git push origin "$TAG"

NEXT_VERSION=$(awk -F. '{print $1"."$2"."($3+1)}' <<<"$VERSION")
echo ">>> Bumping version to $NEXT_VERSION"
sed -i "s/^version = \"$VERSION\"/version = \"$NEXT_VERSION\"/" Cargo.toml

echo ">>> cargo update -p waitagent"
cargo update -p waitagent

echo ">>> cargo check"
cargo check

echo ">>> Committing version bump"
git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to $NEXT_VERSION"

echo ">>> Pushing bump commit to origin main"
git push origin main

echo ">>> Done. $TAG released; main is now at $NEXT_VERSION."
