#!/bin/sh
# Run from the repository root; optional tag also validates a released commit.
set -eu
package_version=$(awk '
  /^\[package\]$/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^version = / { gsub(/["[:space:]]/, "", $3); print $3; exit }
' Cargo.toml)
plugin_version=$(awk '/^version = / { gsub(/["[:space:]]/, "", $3); print $3; exit }' herdr-plugin.toml)
[ -n "$package_version" ] && [ "$package_version" = "$plugin_version" ] || {
  echo "Cargo.toml ($package_version) and herdr-plugin.toml ($plugin_version) disagree" >&2
  exit 1
}
if [ "$#" -gt 0 ]; then
  [ "v$package_version" = "$1" ] || { echo "version does not match tag $1" >&2; exit 1; }
fi
