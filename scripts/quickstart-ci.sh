#!/usr/bin/env bash
set -euo pipefail

usage='usage: quickstart-ci.sh <image> [--published <version>]'
image=${1:?$usage}
published=
if [ "${2:-}" = --published ]; then
  published=${3:?$usage}
  if [ "$image" != "ghcr.io/lodestore/lodestore:$published" ]; then
    echo "quickstart check failed: --published $published checks ghcr.io/lodestore/lodestore:$published, not $image" >&2
    exit 1
  fi
elif [ "$#" -ne 1 ]; then
  echo "$usage" >&2
  exit 2
fi
root=$(cd "$(dirname "$0")/.." && pwd)
page=$root/docs/src/content/docs/docs/quickstart.md
bin_dir=$root/target/test-bin
data_dir=/srv/lodestore-quickstart

# cleanup removes all of these by name, so it only runs when this run created them.
if docker container inspect lodestore >/dev/null 2>&1 || docker volume inspect lodestore-data >/dev/null 2>&1; then
  echo "quickstart check refused: a lodestore container or lodestore-data volume already exists" >&2
  exit 1
fi
for path in /etc/lodestore /var/lib/lodestore /etc/systemd/system/lodestore-agent.service \
  /usr/local/bin/lodestore-agent "$data_dir"; do
  if [ -e "$path" ]; then
    echo "quickstart check refused: $path already exists" >&2
    exit 1
  fi
done

workdir=$(mktemp -d)
bucket=lodestore-quickstart
alias_name=qs
admin_user="admin"
admin_password=quickstart-ci-password
repo_name=quickstart
repo_password=quickstart-ci-repo-password
budget=600
timings=()
minio_pid=
jar=$workdir/cookies
mc_wrapper=$workdir/mc

LODESTORE_HOST=$(hostname -I | awk '{print $1}')
LODESTORE_VERSION=${published:-ci}
export LODESTORE_HOST LODESTORE_VERSION
base=http://$LODESTORE_HOST:8080

cleanup() {
  status=$?
  set +e
  if [ "$status" -ne 0 ]; then
    docker logs lodestore >&2 2>&1
    sudo journalctl -u lodestore-agent --no-pager -n 80 >&2 2>&1
    systemctl is-system-running >&2 2>&1
  fi
  if [ -f "$workdir/install.sh" ]; then
    sudo sh "$workdir/install.sh" --uninstall --purge >/dev/null 2>&1
  fi
  [ -z "$minio_pid" ] || kill "$minio_pid" 2>/dev/null
  docker rm -f lodestore >/dev/null 2>&1
  docker volume rm lodestore-data >/dev/null 2>&1
  sudo rm -rf "$data_dir"
  rm -rf "$workdir"
  exit "$status"
}
trap cleanup EXIT

fail() {
  echo "quickstart check failed: $*" >&2
  exit 1
}

wait_for() {
  local what=$1
  shift
  for _ in $(seq "${WAIT:-60}"); do
    if "$@" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  fail "timed out waiting for $what"
}

extract() {
  local block
  block=$(awk -v name="$1" '
    $0 == "<!-- ci:" name " -->" { want = 1; next }
    want && /^```/ { if (inside) exit; inside = 1; next }
    want && inside { print }
  ' "$page")
  [ -n "$block" ] || fail "no code block marked ci:$1 in $page"
  printf '%s\n' "$block"
}

api() {
  local method=$1 path=$2 body=${3:-}
  if [ -n "$body" ]; then
    curl -sS --fail-with-body -X "$method" -b "$jar" -c "$jar" \
      -H "x-csrf-token: ${csrf:-}" -H 'content-type: application/json' \
      --data "$body" "$base$path"
  else
    curl -sS --fail-with-body -X "$method" -b "$jar" -c "$jar" \
      -H "x-csrf-token: ${csrf:-}" "$base$path"
  fi
}

random_hex() {
  head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'
}

b64url() {
  printf '%s' "$1" | base64 -w0 | tr '+/' '-_' | tr -d '='
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

fetch_pinned() {
  local file=$1 url=$2 sha=$3
  mkdir -p "$bin_dir"
  if [ ! -f "$bin_dir/$file" ]; then
    curl -fsSL -o "$bin_dir/$file.tmp" "$url"
    mv "$bin_dir/$file.tmp" "$bin_dir/$file"
  fi
  echo "$sha  $bin_dir/$file" | sha256sum --check --status || {
    rm -f "$bin_dir/$file"
    fail "sha256 mismatch for $file"
  }
  chmod 0755 "$bin_dir/$file"
}

start_storage() {
  # shellcheck source=/dev/null
  . "$root/scripts/pins.env"
  local arch minio_sha mc_sha
  case $(uname -m) in
    x86_64) arch=amd64; minio_sha=$MINIO_SHA256_LINUX_AMD64; mc_sha=$MC_SHA256_LINUX_AMD64 ;;
    aarch64) arch=arm64; minio_sha=$MINIO_SHA256_LINUX_ARM64; mc_sha=$MC_SHA256_LINUX_ARM64 ;;
    *) fail "no pinned MinIO binary for $(uname -m)" ;;
  esac
  local minio_file=minio.linux-$arch.$MINIO_RELEASE
  local mc_file=mc.linux-$arch.$MC_RELEASE
  fetch_pinned "$minio_file" "https://github.com/minio/minio/releases/download/$MINIO_RELEASE/$minio_file" "$minio_sha"
  fetch_pinned "$mc_file" "https://github.com/minio/mc/releases/download/$MC_RELEASE/$mc_file" "$mc_sha"

  minio_root_user=$(random_hex 8)
  minio_root_password=$(random_hex 16)
  mkdir -p "$workdir/minio-data" "$workdir/mc-config"
  MINIO_ROOT_USER=$minio_root_user MINIO_ROOT_PASSWORD=$minio_root_password \
    "$bin_dir/$minio_file" server "$workdir/minio-data" \
    --address 0.0.0.0:9000 --console-address 127.0.0.1:9001 >"$workdir/minio.log" 2>&1 &
  minio_pid=$!
  wait_for minio curl -fsS http://127.0.0.1:9000/minio/health/ready

  printf '#!/bin/sh\nexec %s --config-dir %s --no-color "$@"\n' "$bin_dir/$mc_file" "$workdir/mc-config" >"$mc_wrapper"
  chmod 0755 "$mc_wrapper"
  wait_for "mc alias" "$mc_wrapper" alias set "$alias_name" http://127.0.0.1:9000 "$minio_root_user" "$minio_root_password"
  "$mc_wrapper" mb "$alias_name/$bucket" >/dev/null
}

run_server() {
  [ -n "$published" ] || docker tag "$image" "ghcr.io/lodestore/lodestore:$LODESTORE_VERSION"
  local block
  block=$(extract run-server)
  bash -c "$block" >/dev/null
  wait_for /healthz curl -fsS "$base/healthz"
}

wizard() {
  local setup_log token block
  block=$(extract setup-link)
  setup_log=$(bash -c "$block")
  token=$(printf '%s\n' "$setup_log" | sed -n 's/.*setup?token=\([A-Za-z0-9_-]*\).*/\1/p' | head -n 1)
  [ -n "$token" ] || fail "no setup token in the server log"
  api POST /api/v1/setup "$(jq -n --arg t "$token" --arg u "$admin_user" --arg p "$admin_password" \
    '{token: $t, username: $u, password: $p}')" >/dev/null
  csrf=$(api GET /api/v1/auth/session | jq -r .csrf_token)
  [ -n "$csrf" ] && [ "$csrf" != null ] || fail "no csrf token"

  local agent_access agent_secret commands
  agent_access=$(random_hex 8)
  agent_secret=$(random_hex 16)
  commands=$(api GET "/api/v1/agent-policy?bucket=$bucket" | jq -r .minio_commands)
  commands=$(printf '%s\n' "$commands" | sed \
    -e "s/ALIAS/$alias_name/g" \
    -e "s/AGENT_ACCESS_KEY/$agent_access/g" \
    -e "s/AGENT_SECRET_KEY/$agent_secret/g" \
    -e "s|^mc |$mc_wrapper |")
  (cd "$workdir" && bash -c "$commands") >/dev/null

  local draft started wizard_id key
  draft=$(jq -n \
    --arg name "$repo_name" --arg bucket "$bucket" --arg endpoint "http://$LODESTORE_HOST:9000" \
    --arg su "$minio_root_user" --arg sp "$minio_root_password" \
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
  [ -n "$repo_id" ] && [ "$repo_id" != null ] || fail "the wizard did not return a repository id"
}

host_online() {
  api GET /api/v1/hosts | jq -e '[.[] | select(.status == "online")] | length > 0'
}

add_host() {
  local created block
  created=$(api POST /api/v1/join-tokens "$(jq -n --arg r "$repo_id" \
    '{repository_id: $r, labels: [{key: "agent", value: "quickstart"}]}')")
  JOIN_TOKEN=$(jq -r .token <<<"$created")
  export JOIN_TOKEN
  curl -fsSL "$base/install.sh" -o "$workdir/install.sh"
  block=$(extract install-agent)
  [ -z "$published" ] || block="$block --from-github"
  bash -c "$block"
  echo "systemd: $(systemctl is-system-running || true), lodestore-agent: $(systemctl is-active lodestore-agent)"
  wait_for "the host to be online" host_online
  host_id=$(api GET /api/v1/hosts | jq -r '[.[] | select(.status == "online")][0].id')
}

first_backup() {
  sudo install -d -m 0755 -o "$(id -u)" "$data_dir" "$data_dir/data"
  mkdir -p "$data_dir/data/docs/nested/deeper" "$data_dir/data/logs"
  printf 'alpha\n' >"$data_dir/data/docs/a.txt"
  printf 'bravo\n' >"$data_dir/data/docs/nested/deeper/b.txt"
  printf 'charlie\n' >"$data_dir/data/docs/file with space.txt"
  head -c 8388608 /dev/urandom >"$data_dir/data/logs/random.bin"
  local yaml created
  yaml=$(printf 'apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: quickstart\nspec:\n  repository: %s\n  selector:\n    matchLabels:\n      agent: quickstart\n  paths:\n    - %s/data\n' "$repo_name" "$data_dir")
  api POST /api/v1/policies "$(jq -n --arg y "$yaml" '{yaml: $y}')" >/dev/null
  created=$(api POST /api/v1/policies/quickstart/run '{}')
  backup_job=$(jq -r '.jobs[0].id' <<<"$created")
  WAIT=180 wait_for "the backup job" job_done "$backup_job"
  snapshot_id=$(api GET "/api/v1/jobs/$backup_job" | jq -r .snapshot_id)
  [ -n "$snapshot_id" ] && [ "$snapshot_id" != null ] || fail "the backup job has no snapshot"
}

restore() {
  local body created
  body=$(jq -n --arg s "$snapshot_id" --arg i "$(b64url "$data_dir/data")" --arg h "$host_id" --arg d /var/lib/lodestore/restore/quickstart \
    '{snapshot_id: $s, items: [$i], target_host_id: $h, destination: $d, in_place: false,
      conflict: "skip_existing", restore_owners: true}')
  created=$(api POST /api/v1/restores "$body")
  restore_job=$(jq -r .job_id <<<"$created")
  WAIT=180 wait_for "the restore job" job_done "$restore_job"
}

compare_restored() {
  local restored=$1$data_dir/data
  if ! sudo test -f "$restored/docs/a.txt"; then
    sudo find "$1" | head -40 >&2
    fail "restored files are not at their original absolute path below $1"
  fi
  sudo diff -r "$data_dir/data" "$restored" || fail "restored files differ from the originals"
  local original_sums restored_sums
  original_sums=$(cd "$data_dir/data" && find . -type f -print0 | sort -z | xargs -0 sha256sum)
  restored_sums=$(sudo sh -c 'cd "$1" && find . -type f -print0 | sort -z | xargs -0 sha256sum' sh "$restored")
  [ "$original_sums" = "$restored_sums" ] || fail "checksums differ after restore"
}

verify_bytes() {
  compare_restored /var/lib/lodestore/restore/quickstart
}

prepare_lode() {
  local src=${LODE_BIN:-$root/dist/lode-linux-x86_64}
  if [ -n "$published" ]; then
    local url=https://github.com/lodestore/lodestore/releases/download/v$published expected
    curl -fsSL -o "$workdir/lode.download" "$url/lode-linux-x86_64"
    curl -fsSL -o "$workdir/SHA256SUMS" "$url/SHA256SUMS"
    expected=$(awk '$2 == "lode-linux-x86_64" || $2 == "*lode-linux-x86_64" { print $1; exit }' "$workdir/SHA256SUMS")
    [ -n "$expected" ] || fail "SHA256SUMS has no entry for lode-linux-x86_64"
    echo "$expected  $workdir/lode.download" | sha256sum --check --status || fail "sha256 mismatch for lode-linux-x86_64"
    src=$workdir/lode.download
  fi
  [ -f "$src" ] || fail "no lode binary at $src"
  mkdir -p "$workdir/bin"
  install -m 0755 "$src" "$workdir/bin/lode"
}

restore_without_server() {
  prepare_lode
  docker stop lodestore >/dev/null
  printf '%s' "$repo_password" >"$workdir/repository-password"
  local block
  block=$(extract restore-without-server)
  block=${block//your-server-access-key/$minio_root_user}
  block=${block//your-server-secret-key/$minio_root_password}
  (
    cd "$workdir"
    export PATH="$workdir/bin:$PATH" S3_ENDPOINT="http://$LODESTORE_HOST:9000" S3_BUCKET="$bucket"
    bash -c "$block"
  )
  compare_restored "$workdir/restored"
}

step() {
  local started=$SECONDS
  echo "== $1"
  "$1"
  timings+=("$1 $((SECONDS - started))s")
  [ "$SECONDS" -le "$budget" ] || fail "over budget: ${SECONDS}s"
}

SECONDS=0
for name in start_storage run_server wizard add_host first_backup restore verify_bytes restore_without_server; do
  step "$name"
done
printf 'step %s\n' "${timings[@]}"
echo "quickstart ok in ${SECONDS}s"
