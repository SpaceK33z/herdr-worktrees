#!/usr/bin/env bash
# One-command release: bumps both manifests, refreshes Cargo.lock, dates the
# changelog's Unreleased section, tags, and pushes. The tag triggers the
# release workflow, which validates everything again and creates the GitHub
# release.
#
# Usage: scripts/release.sh <major|minor|patch|x.y.z>
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
REPO_URL="https://github.com/SpaceK33z/herdr-worktrees"

die() { echo "error: $*" >&2; exit 1; }

[[ $# == 1 ]] || die "usage: $0 <major|minor|patch|x.y.z>"

# --- Preflight -----------------------------------------------------------------

git fetch --quiet origin
[[ $(git rev-parse --abbrev-ref HEAD) == "main" ]] || die "not on main"
[[ -z $(git status --porcelain) ]] || die "working tree is not clean"
git rev-parse HEAD --verify --quiet "$(git rev-parse HEAD)^{commit}" >/dev/null
[[ $(git rev-parse HEAD) == $(git rev-parse origin/main) ]] ||
  die "main is not in sync with origin/main"

current=$(awk '
  /^\[package\]$/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^version = / { gsub(/["[:space:]]/, "", $3); print $3; exit }
' Cargo.toml)
[[ $current =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "cannot parse current version ($current)"

IFS=. read -r maj min pat <<<"$current"
case $1 in
  major) next="$((maj + 1)).0.0" ;;
  minor) next="$maj.$((min + 1)).0" ;;
  patch) next="$maj.$min.$((pat + 1))" ;;
  *)     next="$1"; [[ $next =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
           die "invalid version '$next'" ;;
esac

[[ $next != "$current" ]] || die "next version equals current version ($current)"
git rev-parse -q --verify "refs/tags/v$next" >/dev/null &&
  die "tag v$next already exists"
grep -q '^## \[Unreleased\]$' CHANGELOG.md || die "CHANGELOG.md has no Unreleased section"

# The Unreleased section must have content between its heading and the next one.
unreleased_body=$(awk '/^## \[Unreleased\]$/{on=1; next} on && /^## /{exit} on{print}' CHANGELOG.md)
[[ -n ${unreleased_body//[[:space:]]/} ]] ||
  die "the Unreleased changelog section is empty; record changes first"

echo "releasing v$current -> v$next"

# --- Bump versions --------------------------------------------------------------

sed -i "0,/^\(version = \"\)$current\(\"\)$/{s//\1$next\2/}" Cargo.toml herdr-plugin.toml
cargo update -w --quiet   # refresh Cargo.lock for the workspace member only

# --- Date the changelog ---------------------------------------------------------

date_today=$(date +%Y-%m-%d)
tmp=$(mktemp)
awk -v section="## [$next] - $date_today" '
  !done && /^## \[Unreleased\]$/ { print; print ""; print section; done = 1; next }
  { print }
' CHANGELOG.md >"$tmp"
printf '[%s]: %s/releases/tag/v%s\n' "$next" "$REPO_URL" "$next" >>"$tmp"
mv "$tmp" CHANGELOG.md

# --- Commit, tag, push ----------------------------------------------------------

git add Cargo.toml Cargo.lock herdr-plugin.toml CHANGELOG.md
git commit --quiet -m "chore: prepare v$next"
git tag -a "v$next" -m "herdr-worktrees v$next"
git push --quiet origin main "v$next"

cat <<EOF

Done. The release workflow will validate and publish:
  $REPO_URL/actions/workflows/release.yml

After it finishes, verify the install path:
  herdr plugin install SpaceK33z/herdr-worktrees --yes
EOF
