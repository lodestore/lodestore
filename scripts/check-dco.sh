#!/usr/bin/env bash
# Fails when a non-merge commit in <base>..<head> has no Signed-off-by trailer that matches
# its author (name and email exactly). Trailers are parsed by git, so a sign-off line outside
# the trailer block does not count. Merge commits are skipped.
#
#   scripts/check-dco.sh <base> <head>
#   scripts/check-dco.sh --self-test
#
# Read-only on the repository it inspects. --self-test builds a temporary repository.
set -euo pipefail

self=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")

check_range() {
  local base=$1 head=$2 sha author bad=() total=0
  while read -r sha; do
    [ -n "$sha" ] || continue
    total=$((total + 1))
    author=$(git log -1 --format='%an <%ae>' "$sha")
    if ! git log -1 --format='%(trailers:key=Signed-off-by,valueonly)' "$sha" | grep -Fxq -- "$author"; then
      bad+=("$(git log -1 --format='%h %s' "$sha")")
    fi
  done < <(git rev-list --no-merges "$base..$head")
  if [ "${#bad[@]}" -gt 0 ]; then
    echo "commits without a Signed-off-by that matches their author:" >&2
    printf '  %s\n' "${bad[@]}" >&2
    echo "sign off with git commit -s; for commits you already made, git rebase --signoff $base" >&2
    return 1
  fi
  echo "dco ok: $total commits signed off"
}

self_test() {
  local rc out base
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
  export GIT_AUTHOR_NAME=Ada GIT_AUTHOR_EMAIL=ada@example.com
  export GIT_COMMITTER_NAME=Ada GIT_COMMITTER_EMAIL=ada@example.com
  cd "$tmp"
  git init -q -b main .
  commit() { git commit -q --allow-empty -m "$1"; }
  expect() {
    local want=$1 label=$2
    shift 2
    rc=0
    out=$("$@" 2>&1) || rc=$?
    if [ "$rc" -ne "$want" ]; then
      echo "self-test failed: $label: expected exit $want, got $rc" >&2
      echo "$out" >&2
      exit 1
    fi
  }
  local signed=$'\n\nSigned-off-by: Ada <ada@example.com>'

  commit "base$signed"
  base=$(git rev-parse HEAD)

  commit "signed$signed"
  expect 0 "signed by the author" "$self" "$base" HEAD

  commit "unsigned"
  expect 1 "no sign-off" "$self" "$base" HEAD
  case $out in *unsigned*) ;; *) echo "self-test failed: violation not listed: $out" >&2; exit 1 ;; esac
  git reset -q --hard "$base"

  commit "other email$signed"
  git commit -q --amend --allow-empty -m "other email"$'\n\nSigned-off-by: Ada <other@example.com>'
  expect 1 "sign-off with another email" "$self" "$base" HEAD
  git reset -q --hard "$base"

  git commit -q --allow-empty -m "stray"$'\n\nSigned-off-by: Ada <ada@example.com>\n\nmore text after the sign-off'
  expect 1 "sign-off outside the trailer block" "$self" "$base" HEAD
  git reset -q --hard "$base"

  git checkout -q -b left
  commit "left$signed"
  git checkout -q main
  git checkout -q -b right
  commit "right$signed"
  git checkout -q left
  git merge -q --no-ff -m "merge without sign-off" right
  expect 0 "unsigned merge commit" "$self" "$base" HEAD

  expect 0 "empty range" "$self" HEAD HEAD
  expect 2 "no arguments" "$self"

  echo "self-test ok"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit 0
fi

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <base> <head> | --self-test" >&2
  exit 2
fi

check_range "$1" "$2"
