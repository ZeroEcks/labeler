#!/usr/bin/env bash
# Cuts a release:
#   1. Bumps the version in Cargo.toml (and the matching Cargo.lock entry)
#   2. Bumps the version in CloudronManifest.json and prepends a CHANGELOG
#      entry summarizing every commit since the last release tag
#   3. Registers the release in CloudronVersions.json via `cloudron versions add`
#   4. Builds the devenv container + Cloudron image, and pushes both the
#      versioned tag and `latest` to the registry
#
# Usage:
#   RELEASE_REGISTRY=ghcr.io/zeroecks/labeler scripts/release.sh 0.2.0 [testing]
#
#   version         e.g. 0.2.0 (no leading "v")
#   publish-state   "published" (default) or "testing"
#
# Requires on PATH: git, jq, docker (logged in to the target registry),
# devenv, and the `cloudron` CLI (`npm i -g cloudron`).
#
# Leaves one commit ("chore: release vX.Y.Z") and an annotated tag "vX.Y.Z"
# on the current branch. Does not push git refs; push manually once you're
# happy with the diff.

set -euo pipefail

VERSION="${1:?Usage: $0 <version> [publish-state]}"
STATE="${2:-published}"
REGISTRY="${RELEASE_REGISTRY:?Set RELEASE_REGISTRY, e.g. ghcr.io/zeroecks/labeler}"

if [[ "$STATE" != "published" && "$STATE" != "testing" ]]; then
  echo "publish-state must be 'published' or 'testing', got '$STATE'" >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

for tool in git jq docker devenv cloudron; do
  command -v "$tool" >/dev/null || { echo "Missing required tool: $tool" >&2; exit 1; }
done

if [[ -n "$(git status --porcelain)" ]]; then
  echo "Working tree is dirty. Commit or stash changes before releasing." >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
  echo "Tag v$VERSION already exists." >&2
  exit 1
fi

echo "==> [1/4] Bumping Cargo.toml and Cargo.lock to $VERSION"
sed -i "0,/^version = \".*\"/s//version = \"$VERSION\"/" Cargo.toml
sed -i "/^name = \"labeler\"\$/{n;s/^version = \".*\"/version = \"$VERSION\"/}" Cargo.lock

echo "==> [2/4] Bumping CloudronManifest.json and prepending a CHANGELOG entry"
jq --arg v "$VERSION" '.version = $v' CloudronManifest.json > CloudronManifest.json.tmp
mv CloudronManifest.json.tmp CloudronManifest.json

last_tag="$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)"
range="${last_tag:+${last_tag}..}HEAD"
commits="$(git log "$range" --no-merges --pretty='format:* %s (%h)')"
if [[ -z "$commits" ]]; then
  commits="* No commits recorded since ${last_tag:-the initial commit}."
fi

{
  printf '[%s]\n%s\n\n' "$VERSION" "$commits"
  cat CHANGELOG
} > CHANGELOG.tmp
mv CHANGELOG.tmp CHANGELOG

echo "==> [3/4] Registering v$VERSION in CloudronVersions.json"
cloudron versions add --image "$REGISTRY:$VERSION" --state "$STATE"

git add Cargo.toml Cargo.lock CloudronManifest.json CHANGELOG CloudronVersions.json
git commit -m "chore: release v$VERSION"
git tag -a "v$VERSION" -m "v$VERSION"

echo "==> [4/4] Building and pushing $REGISTRY:$VERSION / :latest"
devenv container build builder
devenv container copy builder
docker build \
  --build-arg BUILDER_IMAGE=labeler-builder:latest \
  -t "$REGISTRY:$VERSION" \
  -t "$REGISTRY:latest" \
  .
docker push "$REGISTRY:$VERSION"
docker push "$REGISTRY:latest"

echo
echo "Released v$VERSION. Push it with: git push && git push origin v$VERSION"
