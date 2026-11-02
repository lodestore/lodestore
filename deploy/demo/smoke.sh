#!/usr/bin/env bash
set -euo pipefail

usage='usage: smoke.sh BASE_URL [--external HOST]'
base=${1:?$usage}
base=${base%/}
external=
if [ "${2:-}" = --external ]; then
  external=${3:?$usage}
elif [ "$#" -gt 1 ]; then
  echo "$usage" >&2
  exit 2
fi

workdir=$(mktemp -d)
jar=$workdir/cookies
csrf=
trap 'rm -rf "$workdir"' EXIT

fail() {
  echo "smoke check failed: $*" >&2
  exit 1
}

pass() {
  echo "ok: $*"
}

status_of() {
  local method=$1 path=$2 out=$3 body=${4:-}
  local args=(-sS -o "$out" -w '%{http_code}' -X "$method" -b "$jar" -c "$jar"
    -H "Origin: $base" -H "x-csrf-token: $csrf" -H 'content-type: application/json')
  [ -z "$body" ] || args+=(--data "$body")
  curl "${args[@]}" "$base$path"
}

get() {
  curl -fsS -b "$jar" -c "$jar" -H "Origin: $base" "$base$1"
}

b64url() {
  printf '%s' "$1" | base64 | tr -d '\n' | tr '+/' '-_' | tr -d '='
}

[ "$(curl -fsS "$base/api/v1/server/status" | jq -r .demo_mode)" = true ] ||
  fail "demo_mode is not true on $base"
pass "demo_mode is true"

code=$(status_of POST /api/v1/auth/demo "$workdir/out")
[ "$code" = 204 ] || fail "POST /api/v1/auth/demo answered $code"
csrf=$(get /api/v1/auth/session | jq -r .csrf_token)
[ -n "$csrf" ] && [ "$csrf" != null ] || fail "no csrf token for the demo session"
pass "the demo session starts without a password"

hosts=$(get /api/v1/hosts)
online=$(jq '[.[] | select(.status == "online")] | length' <<<"$hosts")
[ "$online" -ge 2 ] || fail "$online hosts online, expected at least 2"
pass "$online hosts online"

snapshot_id=
snapshot_path=
total=0
for host_id in $(jq -r '.[] | select(.status == "online") | .id' <<<"$hosts"); do
  page=$(get "/api/v1/hosts/$host_id/snapshots")
  count=$(jq '.total' <<<"$page")
  total=$((total + count))
  if [ -z "$snapshot_id" ] && [ "$count" -gt 0 ]; then
    snapshot_id=$(jq -r '.items[0].id' <<<"$page")
    snapshot_path=$(jq -r '.items[0].paths[0]' <<<"$page")
  fi
done
[ "$total" -ge 1 ] && [ -n "$snapshot_id" ] || fail "no snapshots yet"
pass "$total snapshots"

code=$(status_of POST /api/v1/policies "$workdir/out" '{"yaml": "x"}')
[ "$code" = 403 ] || fail "POST /api/v1/policies answered $code, expected 403"
[ "$(jq -r .code "$workdir/out")" = demo_mode ] || fail "the refusal has code $(jq -r .code "$workdir/out"), expected demo_mode"
pass "writes answer 403 demo_mode"

policies=$(get /api/v1/policies | jq -r '.[].name')
for name in $policies; do
  detail=$(get "/api/v1/policies/$name")
  jq -e '[.. | objects | select(has("hooks")) | .hooks | select(. != null and . != [] and . != {})] | length == 0' <<<"$detail" >/dev/null ||
    fail "policy $name has hooks"
  jq -e '(.yaml // "") | test("hooks") | not' <<<"$detail" >/dev/null || fail "the YAML of policy $name mentions hooks"
done
pass "no policy has hooks"

item=$(b64url "$snapshot_path")
created=$(jq -n --arg s "$snapshot_id" --arg i "$item" '{snapshot_id: $s, items: [$i], format: "tar_gz"}')
code=$(status_of POST /api/v1/downloads "$workdir/out" "$created")
[ "$code" = 201 ] || fail "POST /api/v1/downloads answered $code: $(cat "$workdir/out")"
link=$(jq -r .url "$workdir/out")
case "$link" in
  /*) ;;
  *) link=/${link#*://*/} ;;
esac
curl -fsS -b "$jar" -c "$jar" -H "Origin: $base" -o "$workdir/archive" "$base$link" || fail "the archive download failed"
[ -s "$workdir/archive" ] || fail "the downloaded archive is empty"
tar -tzf "$workdir/archive" >/dev/null || fail "the downloaded archive is not a readable tar.gz"
pass "archive downloaded, $(wc -c <"$workdir/archive" | tr -d ' ') bytes"

metrics=$(curl -sS -b "$jar" "$base/metrics" -o "$workdir/metrics" -w '%{http_code}')
if grep -qE '^# (TYPE|HELP)' "$workdir/metrics"; then
  fail "/metrics serves metrics to a demo session (status $metrics)"
fi
pass "/metrics serves no metrics (status $metrics)"

limiter_check() {
  local flood_from=198.51.100.7 other=198.51.100.8 limited='' code
  for _ in $(seq 40); do
    code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H "X-Forwarded-For: $flood_from" -H "Origin: $base" "$base/api/v1/auth/demo")
    if [ "$code" != 204 ]; then
      limited=$code
      break
    fi
  done
  [ "$limited" = 429 ] ||
    fail "40 sign-ins from one address never hit the server limiter (last status ${limited:-204}); run against the direct port, not through the proxy"
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H "X-Forwarded-For: $other" -H "Origin: $base" "$base/api/v1/auth/demo")
  [ "$code" = 204 ] ||
    fail "a second client address got $code: the limiter keys on the proxy address, check trusted_proxies"
  pass "the limiter keys on the client address"
}

case $base in
  https://*) echo "skip: the limiter check needs the direct http port, the proxy rewrites the client address" ;;
  *) limiter_check ;;
esac

if [ -n "$external" ]; then
  command -v timeout >/dev/null || fail "--external needs the timeout command"
  for port in 8443 9000; do
    # shellcheck disable=SC2016
    if timeout 5 bash -c 'exec 3<>"/dev/tcp/$0/$1"' "$external" "$port" 2>/dev/null; then
      fail "$external:$port accepts connections from outside"
    fi
  done
  pass "ports 8443 and 9000 are closed on $external"
fi

echo "demo ok"
