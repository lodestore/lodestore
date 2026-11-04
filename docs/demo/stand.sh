#!/bin/bash
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
out=$here/.out
env_file=$out/stand.env
project=lodestore-site-stand
port=18180

fail() {
  echo "stand failed: $*" >&2
  exit 1
}

dc() {
  docker compose \
    --project-directory "$root/deploy/demo" \
    -f "$root/deploy/demo/compose.yaml" \
    -f "$here/compose.stand.yaml" \
    -p "$project" \
    --env-file "$env_file" \
    "$@"
}

random_hex() {
  openssl rand -hex "$1"
}

configure() {
  mkdir -p "$out"
  if [ ! -f "$env_file" ]; then
    (
      umask 077
      cat >"$env_file" <<EOF
LODESTORE_VERSION=0.1.0
LODESTORE_DEMO_MODE=false
DEMO_PORT=$port
DEMO_PUBLIC_URL=http://127.0.0.1:$port
DEMO_ADMIN_USERNAME=demo-admin
DEMO_ADMIN_PASSWORD=$(random_hex 24)
DEMO_REPO_PASSWORD=$(random_hex 24)
MINIO_ROOT_USER=$(random_hex 24)
MINIO_ROOT_PASSWORD=$(random_hex 24)
EOF
    )
  fi
  if [ ! -f "$out/unlock_key" ]; then
    random_hex 32 >"$out/unlock_key"
    chmod 0444 "$out/unlock_key"
  fi
}

# The steps of "First start" in deploy/demo/README.md up to the first seed.
# preflight.sh is left out on purpose: it checks the requirements of a public
# host (rootless daemon, disk size), and this stand is local and loopback-only.
prepare() {
  command -v docker >/dev/null || fail "docker is required"
  configure
  dc build
  dc up -d lodestore minio
}

seed_stand() {
  prepare
  dc --profile seed run --rm seed
  dc up -d
  dc --profile seed run --rm seed first-backup
  bash "$here/seed-extra.sh"
}

down() {
  dc --profile seed --profile addhost down -v --remove-orphans
}

mc_script() {
  dc --profile seed run --rm -T --no-deps --entrypoint bash seed -s
}

addhost_start() {
  dc --profile addhost up -d agent-4
}

addhost_token() {
  [ -n "${1:-}" ] || fail "addhost-token needs the join token"
  dc --profile seed run --rm -T --no-deps -e JOIN_TOKEN="$1" --entrypoint bash seed \
    -c 'umask 077; printf "%s\n" "$JOIN_TOKEN" >/seed/join-demo-app-01.token'
}

render() {
  trap down EXIT
  cd "$here"
  prepare
  node capture.ts wizard
  seed_stand
  node capture.ts fleet
  node capture.ts clips light
  down
  seed_stand
  node capture.ts clips dark
}

case "${1:-}" in
  up) prepare ;;
  seed) seed_stand ;;
  down) down ;;
  mc) mc_script ;;
  addhost-start) addhost_start ;;
  addhost-token) addhost_token "${2:-}" ;;
  render) render ;;
  *) echo "usage: stand.sh up|seed|down|mc|addhost-start|addhost-token|render" >&2; exit 2 ;;
esac
