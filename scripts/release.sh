#!/bin/bash
# Bumps the workspace version, commits and tags. Pushing is left to you.
#
# Usage: scripts/release.sh 0.2.0
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

VERSION="${1:?usage: scripts/release.sh X.Y.Z}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not X.Y.Z: $VERSION" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null && { echo "v$VERSION already exists" >&2; exit 1; }

sed -i.bak -E "/^\[workspace.package\]/,/^\[/ s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml
rm Cargo.toml.bak
cargo update --workspace --quiet

git add Cargo.toml Cargo.lock
git commit -q -m "chore: release v$VERSION"
git tag "v$VERSION"
echo "tagged v$VERSION, publish with: git push origin master v$VERSION"
