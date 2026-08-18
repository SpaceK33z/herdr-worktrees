#!/usr/bin/env bash
# scripts/dev-relink.sh — build the binary and re-link the local checkout so
# Herdr picks up manifest changes (Herdr caches the manifest when linked).
set -euo pipefail

plugin_id=${1:-worktrees}
herdr=${HERDR_BIN_PATH:-herdr}
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

cargo build --release --manifest-path "$root/Cargo.toml"

"$herdr" plugin unlink "$plugin_id" >/dev/null 2>&1 || true
"$herdr" plugin link "$root"
"$herdr" plugin action list --plugin "$plugin_id"
