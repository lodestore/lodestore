#!/usr/bin/env bash
# Prints the body of the "## [VERSION]" section of CHANGELOG.md, without the heading.
#
#   scripts/release-notes.sh VERSION
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 VERSION" >&2; exit 2; }
version=$1
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]] || { echo "bad version: $version" >&2; exit 1; }

root=$(cd "$(dirname "$0")/.." && pwd)
notes=$(awk -v heading="## [$version]" '
  index($0, heading) == 1 { inside = 1; next }
  inside && index($0, "## [") == 1 { exit }
  inside { print }
' "$root/CHANGELOG.md")

if ! printf '%s\n' "$notes" | grep -q '[^[:space:]]'; then
  echo "CHANGELOG.md has no notes for $version" >&2
  exit 1
fi
printf '%s\n' "$notes"
