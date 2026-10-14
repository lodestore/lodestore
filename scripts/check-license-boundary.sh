#!/usr/bin/env bash
# Fails when any crate other than lodestore-server depends on lodestore-server,
# in normal, build or dev dependencies. Apache-2.0 crates must not reach the AGPL server.
#
#   scripts/check-license-boundary.sh [--self-test]
#
# Read-only: it only inspects the dependency tree. --self-test injects a violation into a
# temporary worktree and expects the check to catch it.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

check_boundary() {
  local dir=$1 locked=$2 out
  if ! out=$(cd "$dir" && cargo tree $locked -i lodestore-server --workspace -e normal,build,dev --prefix none 2>&1); then
    echo "cargo tree failed:" >&2
    echo "$out" >&2
    return 1
  fi
  local dependents
  dependents=$(printf '%s\n' "$out" | sed 's/ (\*)$//' | sort -u | grep -v '^lodestore-server v' || true)
  if [ -n "$dependents" ]; then
    echo "crates depending on lodestore-server (AGPL-3.0):" >&2
    echo "$dependents" >&2
    return 1
  fi
}

if [ "${1:-}" = "--self-test" ]; then
  tmp=$(mktemp -d)
  trap 'git -C "$root" worktree remove --force "$tmp/wt" >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT
  git -C "$root" worktree add --detach "$tmp/wt" HEAD >/dev/null 2>&1
  manifest="$tmp/wt/crates/lodestore-agent/Cargo.toml"
  awk '{ print } /^\[dev-dependencies\]/ { print "lodestore-server = { path = \"../lodestore-server\" }" }' "$manifest" >"$manifest.new"
  mv "$manifest.new" "$manifest"
  if err=$(check_boundary "$tmp/wt" "" 2>&1); then
    echo "self-test failed: an injected dev-dependency on lodestore-server was not detected" >&2
    exit 1
  fi
  if ! printf '%s\n' "$err" | grep -q '^lodestore-agent v'; then
    echo "self-test failed: the check failed for another reason:" >&2
    echo "$err" >&2
    exit 1
  fi
  echo "self-test ok: the injected violation was detected"
  exit 0
fi

check_boundary "$root" "--locked"
echo "license boundary ok"
