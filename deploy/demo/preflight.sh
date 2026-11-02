#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
memory_budget_mib=2560

fail() {
  echo "preflight failed: $*" >&2
  exit 1
}

problems=0

problem() {
  echo "preflight failed: $*" >&2
  problems=1
}

unsafe() {
  [ "${ALLOW_UNSAFE:-}" = 1 ]
}

envfile=${COMPOSE_ENV_FILES:-.env}
envfile=${envfile%%,*}

env_value() {
  grep -E "^$1=" "$envfile" | tail -n 1 | cut -d= -f2-
}

[ -f "$envfile" ] || fail "no $envfile; copy env.example to .env and fill it in"
if grep -Ev '^[[:space:]]*(#|$)' "$envfile" | grep -q CHANGE_ME; then
  fail "$envfile still contains CHANGE_ME values"
fi

[ -f secrets/unlock_key ] || fail "no secrets/unlock_key; create it as in the README"
grep -Eqx '[0-9a-f]{64}' secrets/unlock_key || fail "secrets/unlock_key must be 64 hex characters"

command -v docker >/dev/null || fail "docker is not installed"
command -v jq >/dev/null || fail "jq is not installed"
docker info >/dev/null 2>&1 || fail "the Docker daemon does not answer"

config=$(docker compose --profile seed config --format json)

binds=$(jq '[.services[].volumes // [] | .[] | select(.type == "bind")] | length' <<<"$config")
[ "$binds" = 0 ] || fail "the compose file has $binds bind mounts of host paths"
if grep -q 'docker.sock' <<<"$config"; then
  fail "the compose file mounts the Docker socket"
fi
published=$(jq '[.services[].ports // [] | .[]] | length' <<<"$config")
[ "$published" = 1 ] || fail "expected exactly one published port, found $published"
bound=$(jq -r '[.services[].ports // [] | .[] | .host_ip] | first' <<<"$config")
[ "$bound" = 127.0.0.1 ] || fail "the published port is bound to '$bound', not 127.0.0.1"
unhardened=$(jq '[.services[] | select(((.cap_drop // []) | index("ALL") | not) or ((.security_opt // []) | index("no-new-privileges:true") | not))] | length' <<<"$config")
[ "$unhardened" = 0 ] || fail "$unhardened services lack cap_drop ALL or no-new-privileges"
memory_mib=$(jq '[.services | to_entries[] | select(.key != "seed") | .value.mem_limit | tonumber] | add / 1048576 | floor' <<<"$config")
[ "$memory_mib" -le "$memory_budget_mib" ] || fail "memory limits add up to ${memory_mib} MiB, over the ${memory_budget_mib} MiB budget"

security=$(docker info --format '{{.SecurityOptions}}')
case "$security" in
  *rootless* | *userns*) ;;
  *)
    if ! unsafe; then
      problem "the Docker daemon is neither rootless nor running with userns-remap ($security)."
      {
        echo "Set it up first (see README), or set ALLOW_UNSAFE=1 for a local trial on a machine that holds nothing else."
        if [ "$(uname -s)" = Linux ]; then
          command -v newuidmap >/dev/null || echo "  missing: newuidmap (package uidmap)"
          command -v rootlesskit >/dev/null || echo "  missing: rootlesskit"
          [ -e /usr/bin/dockerd-rootless-setuptool.sh ] || command -v dockerd-rootless-setuptool.sh >/dev/null || echo "  missing: dockerd-rootless-setuptool.sh (package docker-ce-rootless-extras)"
          [ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null)" = cgroup2fs ] || echo "  cgroup v2 is not mounted"
          if [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)" = 1 ]; then
            echo "  kernel.apparmor_restrict_unprivileged_userns=1: rootlesskit needs an AppArmor profile that allows userns"
          fi
        fi
      } >&2
    else
      echo "warning: ALLOW_UNSAFE=1, the daemon is not rootless or userns-remapped" >&2
    fi
    ;;
esac

case "$security" in
  *rootless*)
    delegated=/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.controllers
    if [ -r "$delegated" ]; then
      for controller in cpu memory pids; do
        grep -qw "$controller" "$delegated" || fail "cgroup controller $controller is not delegated to this user (see README)"
      done
    fi
    ;;
esac

port=${DEMO_PORT:-$(env_value DEMO_PORT || true)}
port=${port:-18080}
if [ -z "$(docker compose ps -q lodestore 2>/dev/null)" ] && command -v ss >/dev/null; then
  if ss -ltn "sport = :$port" 2>/dev/null | grep -q LISTEN; then
    fail "127.0.0.1:$port is already in use; pick another DEMO_PORT"
  fi
fi

limit=${DEMO_DISK_LIMIT_GB:-$(env_value DEMO_DISK_LIMIT_GB || true)}
limit=${limit:-15}
if ! docker image inspect "$(jq -r '.services.minio.image' <<<"$config")" >/dev/null 2>&1; then
  fail "the minio image is not built yet; run docker compose build first"
fi
size=$(docker compose run --rm -T --no-deps --entrypoint df minio -BG --output=size /data | tail -n 1 | tr -dc '0-9')
[ -n "$size" ] || fail "could not read the size of the file system under /data"
if [ "$size" -gt "$limit" ]; then
  if unsafe; then
    echo "warning: ALLOW_UNSAFE=1, the file system under the data is ${size} GB, over DEMO_DISK_LIMIT_GB=${limit}" >&2
  else
    problem "the file system under the data is ${size} GB, over DEMO_DISK_LIMIT_GB=${limit}; put the Docker data-root on a file system of at most ${limit} GB (see README)"
  fi
fi

[ "$problems" = 0 ] || exit 1
echo "preflight ok: data file system ${size} GB, limit ${limit} GB, memory limits ${memory_mib} MiB"
