#!/usr/bin/env bash
# AI-generated release automation.
set -euo pipefail

fail() { echo "$*" >&2; exit 1; }

registry_status() {
  local status
  status=$(curl --silent --show-error --retry 2 --max-time 20 \
    --user-agent 'fledgling-release (https://github.com/kmiller96/fledgling)' \
    --output /dev/null --write-out '%{http_code}' \
    "https://crates.io/api/v1/crates/fledgling/$1")
  case "$status" in
    200|404) echo "$status" ;;
    *) fail "crates.io returned HTTP $status for fledgling $1." ;;
  esac
}

[[ "$(git rev-parse HEAD)" == "$GITHUB_SHA" ]] || fail "Expected the dispatch commit."
[[ -z "$(git status --porcelain)" ]] || fail "Expected a clean checkout."

versions=$(python3 .github/scripts/bump-version.py "$1")
read -r current version <<< "$versions"
echo "version=$version" >> "$GITHUB_OUTPUT"
tag="refs/tags/v$version"
tagged_commit=$(git ls-remote --refs origin "$tag" | cut -f1)

if [[ -z "$tagged_commit" ]]; then
  branch_tip=$(git ls-remote --refs origin "$GITHUB_REF" | cut -f1)
  [[ "$branch_tip" == "$GITHUB_SHA" ]] || fail "Default branch moved. Start a fresh release run."
  [[ "$(registry_status "$current")" == 200 ]] || fail "Version $current is not published. Retry its original release run first."
  [[ "$(registry_status "$version")" == 404 ]] || fail "Version $version already exists on crates.io."
fi

cargo update --workspace
git add Cargo.toml Cargo.lock

if [[ -n "$tagged_commit" ]]; then
  # A rerun must publish exactly the release created by the original dispatch.
  git fetch origin "$tag:$tag"
  [[ "$(git rev-parse "$tag")" == "$tagged_commit" &&
     "$(git rev-parse "$tag^")" == "$GITHUB_SHA" &&
     "$(git rev-parse "$tag^{tree}")" == "$(git write-tree)" ]] ||
    fail "Tag v$version does not match this release."

  if [[ "$(registry_status "$version")" == 200 ]]; then
    echo "Version $version is already published."
    echo "publish=false" >> "$GITHUB_OUTPUT"
    exit 0
  fi

  git restore --staged --worktree Cargo.toml Cargo.lock
  git switch --detach "$tag"
else
  git config user.name 'github-actions[bot]'
  git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
  git commit -m "Release v$version"
fi

bash .github/scripts/check.sh
if [[ -z "$tagged_commit" ]]; then
  git tag -a "v$version" -m "Release v$version"
  git push --atomic origin "HEAD:$GITHUB_REF" "$tag"
fi

echo "publish=true" >> "$GITHUB_OUTPUT"
