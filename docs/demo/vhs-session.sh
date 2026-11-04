#!/bin/bash
# Runs inside the recording machine. Sourced by the tape it defines the wrappers;
# run directly (`bash vhs-session.sh`) it does the steps the recording hides:
# the setup wizard, a join token, an API token and the policy.

vhs_dir=$HOME/.vhs

setup() {
  set -euo pipefail
  # shellcheck source=/dev/null
  . "$vhs_dir/env"
  : "${LODESTORE_HOST:?}" "${LODESTORE_VERSION:?}"
  local base=http://$LODESTORE_HOST:8080
  jar=$vhs_dir/cookies
  local csrf= bucket=quickstart repo_name=main data=/srv/documents
  local admin_password repo_password
  admin_password=$(random_hex 12)
  repo_password=$(random_hex 12)
  trap 'rm -f "$jar"' EXIT

  api() {
    local method=$1 path=$2 body=${3:-}
    if [ -n "$body" ]; then
      curl -sS --fail-with-body -X "$method" -b "$jar" -c "$jar" \
        -H "x-csrf-token: $csrf" -H 'content-type: application/json' --data "$body" "$base$path"
    else
      curl -sS --fail-with-body -X "$method" -b "$jar" -c "$jar" -H "x-csrf-token: $csrf" "$base$path"
    fi
  }

  for _ in $(seq 60); do
    curl -fsS "$base/healthz" >/dev/null 2>&1 && break
    sleep 1
  done
  curl -fsS "$base/healthz" >/dev/null

  local token
  token=$(docker logs lodestore 2>&1 | sed -n '/setup?token=/{s/.*setup?token=\([A-Za-z0-9_-]*\).*/\1/p;q;}')
  [ -n "$token" ] || { echo "vhs failed: no setup token in the server log" >&2; return 1; }
  api POST /api/v1/setup "$(jq -n --arg t "$token" --arg p "$admin_password" \
    '{token: $t, username: "admin", password: $p}')" >/dev/null
  csrf=$(api GET /api/v1/auth/session | jq -r .csrf_token)

  local agent_access agent_secret commands
  agent_access=$(random_hex 8)
  agent_secret=$(random_hex 16)
  commands=$(api GET "/api/v1/agent-policy?bucket=$bucket" | jq -r .minio_commands)
  commands=$(printf '%s\n' "$commands" | sed \
    -e "s/ALIAS/qs/g" \
    -e "s/AGENT_ACCESS_KEY/$agent_access/g" \
    -e "s/AGENT_SECRET_KEY/$agent_secret/g" \
    -e "s|^mc |$vhs_dir/mc |")
  (cd "$vhs_dir" && bash -c "$commands") >/dev/null

  local draft started wizard_id key repo_id
  draft=$(jq -n \
    --arg name "$repo_name" --arg bucket "$bucket" --arg endpoint "http://$LODESTORE_HOST:9000" \
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
    "$(jq -n --arg p "$repo_password" --arg k "${key##*-}" '{password: $p, recovery_key_last_group: $k}')" | jq -r .id)
  [ -n "$repo_id" ] && [ "$repo_id" != null ] || { echo "vhs failed: no repository id" >&2; return 1; }

  local join api_token
  join=$(api POST /api/v1/join-tokens "$(jq -n --arg r "$repo_id" \
    '{repository_id: $r, labels: [{key: "role", value: "demo"}]}')" | jq -r .token)
  api_token=$(api POST /api/v1/tokens '{"name":"recording","scopes":["hosts:read","policies:read","jobs:read","jobs:run","snapshots:read"],"expires_in_days":30}' | jq -r .token)

  sudo install -d -m 0755 "$data"
  sudo cp -r /usr/share/doc/. "$data/"
  local yaml
  yaml=$(printf 'apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: documents\nspec:\n  repository: %s\n  selector:\n    matchLabels:\n      role: demo\n  paths:\n    - %s\n' "$repo_name" "$data")
  api POST /api/v1/policies "$(jq -n --arg y "$yaml" '{yaml: $y}')" >/dev/null

  (
    umask 077
    {
      printf 'export JOIN_TOKEN=%s\n' "$join"
      printf 'export LODESTORE_SERVER=http://127.0.0.1:8080\n'
      printf 'export LODESTORE_TOKEN=%s\n' "$api_token"
    } >"$vhs_dir/session.env"
  )
}

random_hex() {
  head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  setup
else
  function vhs_setup {
    bash "$vhs_dir/session.sh" && . "$vhs_dir/session.env"
  }
  # The setup link is a secret: show the command, mask the token in its output.
  function grep {
    command grep "$@" | sed 's/\(token=\)[A-Za-z0-9_-]*/\1<one-time token>/'
  }
  function lode {
    if [ "$*" != "server policies run documents" ]; then
      command lode "$@"
      return
    fi
    local out rc=0
    out=$(command lode "$@") || rc=$?
    printf '%s\n' "$out"
    JOB=$(printf '%s\n' "$out" | awk '/^Queued job/ { print $3; exit }')
    return "$rc"
  }
fi
