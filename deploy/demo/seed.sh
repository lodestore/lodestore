#!/bin/bash
set -euo pipefail

mode=${1:-all}
case "$mode" in
  all | first-backup) ;;
  *) echo "usage: seed.sh [all|first-backup]" >&2; exit 2 ;;
esac

base=${LODESTORE_URL:?}
minio=${MINIO_URL:?}
bucket=demo
alias_name=demo
repo_name=demo
policy_name=demo
agents="demo-web-01 demo-db-01 demo-files-01"
jar=$(mktemp)
csrf=
trap 'rm -f "$jar"' EXIT

fail() {
  echo "seed failed: $*" >&2
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

api() {
  local method=$1 path=$2 body=${3:-}
  if [ -n "$body" ]; then
    curl -sS --fail-with-body -X "$method" -b "$jar" -c "$jar" \
      -H "x-csrf-token: $csrf" -H 'content-type: application/json' \
      --data "$body" "$base$path"
  else
    curl -sS --fail-with-body -X "$method" -b "$jar" -c "$jar" \
      -H "x-csrf-token: $csrf" "$base$path"
  fi
}

random_hex() {
  head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'
}

login() {
  [ "$(curl -fsS "$base/api/v1/server/status" | jq -r .demo_mode)" = false ] ||
    fail "the server is in demo mode; set LODESTORE_DEMO_MODE=false and restart it first"
  api POST /api/v1/auth/login "$(jq -n --arg u "$LODESTORE_ADMIN_USERNAME" --arg p "$LODESTORE_ADMIN_PASSWORD" \
    '{username: $u, password: $p}')" >/dev/null
  csrf=$(api GET /api/v1/auth/session | jq -r .csrf_token)
  [ -n "$csrf" ] && [ "$csrf" != null ] || fail "no csrf token"
}

prepare_bucket() {
  mc alias set "$alias_name" "$minio" "$MINIO_ROOT_USER" "$MINIO_ROOT_PASSWORD" >/dev/null
  mc mb --ignore-existing "$alias_name/$bucket" >/dev/null
  if mc quota set --help >/dev/null 2>&1; then
    mc quota set "$alias_name/$bucket" --size "$((DEMO_DISK_LIMIT_GB * 6 / 10))GiB" >/dev/null ||
      echo "note: the bucket quota could not be set; the file system limit still applies" >&2
  else
    echo "note: this mc has no 'quota set'; only the file system limit applies" >&2
  fi
}

create_repository() {
  local existing
  existing=$(api GET /api/v1/repositories | jq -r --arg n "$repo_name" '[.[] | select(.name == $n)][0].id // empty')
  if [ -n "$existing" ]; then
    repo_id=$existing
    return 0
  fi
  prepare_bucket
  local agent_access agent_secret commands
  agent_access=$(random_hex 8)
  agent_secret=$(random_hex 16)
  commands=$(api GET "/api/v1/agent-policy?bucket=$bucket" | jq -r .minio_commands)
  commands=$(printf '%s\n' "$commands" | sed \
    -e "s/ALIAS/$alias_name/g" \
    -e "s/AGENT_ACCESS_KEY/$agent_access/g" \
    -e "s/AGENT_SECRET_KEY/$agent_secret/g")
  (cd /tmp && bash -c "$commands") >/dev/null

  local draft started wizard_id key
  draft=$(jq -n \
    --arg name "$repo_name" --arg bucket "$bucket" --arg endpoint "$minio" \
    --arg su "$MINIO_ROOT_USER" --arg sp "$MINIO_ROOT_PASSWORD" \
    --arg au "$agent_access" --arg ap "$agent_secret" \
    '{name: $name,
      location: {backend: "s3", options: {bucket: $bucket, endpoint: $endpoint, path_style: "true"}},
      server_credentials: {access_key_id: $su, secret_access_key: $sp},
      agent_credentials: {access_key_id: $au, secret_access_key: $ap}}')
  started=$(api POST /api/v1/repositories/wizard "$draft")
  wizard_id=$(jq -r .wizard_id <<<"$started")
  key=$(jq -r .recovery_key <<<"$started")
  repo_id=$(api POST "/api/v1/repositories/wizard/$wizard_id/commit" \
    "$(jq -n --arg p "$DEMO_REPO_PASSWORD" --arg k "${key##*-}" '{password: $p, recovery_key_last_group: $k}')" | jq -r .id)
  [ -n "$repo_id" ] && [ "$repo_id" != null ] || fail "the wizard did not return a repository id"
}

apply_policy() {
  if api GET /api/v1/policies | jq -e --arg n "$policy_name" 'any(.[]; .name == $n)' >/dev/null; then
    return 0
  fi
  api POST /api/v1/policies "$(jq -n --rawfile y /etc/demo/demo.yaml '{yaml: $y}')" >/dev/null
}

set_cleanup() {
  api PUT "/api/v1/repositories/$repo_id/gc/settings" \
    '{"enabled": true, "cron": "0 4 * * *", "timezone": "UTC", "pause": "24h", "repack_below_pct": 50, "max_repack": "1GiB"}' >/dev/null
}

issue_tokens() {
  local name created
  umask 077
  for name in $agents; do
    created=$(api POST /api/v1/join-tokens "$(jq -n --arg r "$repo_id" \
      '{repository_id: $r, labels: [{key: "demo", value: "agent"}]}')")
    jq -r .token <<<"$created" >"/seed/join-$name.token"
  done
}

online_hosts() {
  api GET /api/v1/hosts | jq '[.[] | select(.status == "online")] | length'
}

three_hosts() {
  [ "$(online_hosts)" -ge 3 ]
}

job_done() {
  local state
  state=$(api GET "/api/v1/jobs/$1" | jq -r .state)
  case "$state" in
    succeeded | warnings) return 0 ;;
    failed | cancelled | interrupted) fail "job $1 ended as $state" ;;
    *) return 1 ;;
  esac
}

first_backup() {
  wait_for "three online hosts" three_hosts
  local created id
  created=$(api POST "/api/v1/policies/$policy_name/run" '{}')
  for id in $(jq -r '.jobs[].id' <<<"$created"); do
    wait_for "backup job $id" job_done "$id"
  done
  echo "first backups finished"
}

wait_for "the server" curl -fs "$base/healthz"
login
repo_id=
if [ "$mode" = all ]; then
  wait_for "MinIO" curl -fs "$minio/minio/health/ready"
  api PUT /api/v1/account '{"email": "admin@example.com"}' >/dev/null
  create_repository
  apply_policy
  set_cleanup
  issue_tokens
  echo "seeded: repository $repo_id, policy $policy_name, join tokens in /seed"
else
  first_backup
fi
