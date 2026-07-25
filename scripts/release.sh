#!/bin/bash
# Release the current waitagent version and bump main to the next patch version.
#
# This script enforces the ordering from primitive.release-new-version:
#   1. The current Cargo.toml version is the version to release.
#   2. Tag v<current> on HEAD and push it to trigger the release workflow.
#   3. Bump Cargo.toml to the next patch version and push the bump commit.
#
# Usage: ./scripts/release.sh

set -euo pipefail

cd "$(dirname "$0")/.."

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
