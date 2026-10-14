#!/usr/bin/env bash
# Fails when a change modifies, deletes or renames a file of the frozen v1 fixtures.
# New files are allowed.
#
#   scripts/check-fixtures-append-only.sh [BASE]    (default: origin/main)
set -euo pipefail

base=${1:-origin/main}
dir=crates/lodestore-format/tests/fixtures/v1

changed=$(git diff --diff-filter=MDR --name-only "$base...HEAD" -- "$dir")
if [ -n "$changed" ]; then
  echo "The v1 format fixtures are append-only; these files were changed:" >&2
  echo "$changed" >&2
  exit 1
fi
