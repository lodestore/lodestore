#!/bin/bash
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
env_file=$here/.out/stand.env
[ -f "$env_file" ] || { echo "seed-extra failed: run stand.sh up first" >&2; exit 1; }

set -a
. "$env_file"
set +a

base=http://127.0.0.1:${DEMO_PORT:?}
project=lodestore-site-stand
policy_name=demo
extra_policy=archive-sync
jar=$(mktemp)
hdr=$(mktemp)
csrf=
trap 'rm -f "$jar" "$hdr"' EXIT

fail() {
  echo "seed-extra failed: $*" >&2
  exit 1
}

wait_for() {
  local what=$1
  shift
  for _ in $(seq 90); do
    if "$@" >/dev/null; then
      return 0
    fi
    sleep 2
  done
  fail "timed out waiting for $what"
}

save_cookies() {
  local line
  while IFS= read -r line; do
    { grep -v "^${line%%=*}=" "$jar" || true; } >"$jar.new"
    printf '%s\n' "$line" >>"$jar.new"
    mv "$jar.new" "$jar"
  done < <(tr -d '\r' <"$hdr" | sed -n 's/^[Ss]et-[Cc]ookie: *\([^;]*\).*/\1/p')
}

api() {
  local method=$1 path=$2 body=${3:-} rc=0
  local args=(-sS --fail-with-body -X "$method" -D "$hdr"
    -H "Cookie: $(paste -sd ';' "$jar")" -H "x-csrf-token: $csrf")
  [ -z "$body" ] || args+=(-H 'content-type: application/json' --data "$body")
  curl "${args[@]}" "$base$path" || rc=$?
  save_cookies
  return "$rc"
}

login() {
  api POST /api/v1/auth/login "$(jq -n --arg u "$DEMO_ADMIN_USERNAME" --arg p "$DEMO_ADMIN_PASSWORD" \
    '{username: $u, password: $p}')" >/dev/null
  csrf=$(api GET /api/v1/auth/session | jq -r .csrf_token)
  [ -n "$csrf" ] && [ "$csrf" != null ] || fail "no csrf token"
}

job_finished() {
  case "$(api GET "/api/v1/jobs/$1" | jq -r .state)" in
    succeeded | warnings | failed | cancelled | interrupted) return 0 ;;
    *) return 1 ;;
  esac
}

run_policy() {
  local created id
  created=$(api POST "/api/v1/policies/$1/run" '{}')
  for id in $(jq -r '.jobs[].id' <<<"$created"); do
    wait_for "job $id" job_finished "$id"
  done
}

host_id() {
  api GET /api/v1/hosts | jq -r --arg n "$1" '[.[] | select(.hostname == $n)][0].id // empty'
}

host_offline() {
  [ "$(api GET /api/v1/hosts | jq -r --arg n "$1" '[.[] | select(.hostname == $n)][0].status')" = offline ]
}

login

for _ in 1 2; do
  run_policy "$policy_name"
done

db_id=$(host_id demo-db-01)
[ -n "$db_id" ] || fail "host demo-db-01 not found"
api PUT "/api/v1/hosts/$db_id/labels" \
  '{"labels": [{"key": "demo", "value": "agent"}, {"key": "role", "value": "archive"}]}' >/dev/null

api POST /api/v1/policies "$(jq -n --rawfile y /dev/stdin '{yaml: $y}' <<EOF
apiVersion: lodestore.io/v1alpha1
kind: BackupPolicy
metadata:
  name: $extra_policy
spec:
  repository: demo
  selector:
    matchLabels:
      role: archive
  paths:
  - /srv/archive
  schedule:
    cron: "15 2 * * *"
    timezone: UTC
  retention:
    keepLast: 7
EOF
)" >/dev/null
run_policy "$extra_policy"

docker stop "$project-agent-3-1" >/dev/null
wait_for "demo-files-01 to go offline" host_offline demo-files-01
echo "extra seed done: job history, a failed job on demo-db-01, demo-files-01 offline"
