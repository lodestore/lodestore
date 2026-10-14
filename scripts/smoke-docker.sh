#!/usr/bin/env bash
set -euo pipefail

image=${1:?usage: smoke-docker.sh <image>}
suffix=$$
volume=lodestore-smoke-$suffix
name=lodestore-smoke-$suffix
base=http://127.0.0.1:18080
workdir=$(mktemp -d)

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    docker logs "$name" >&2 || true
  fi
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker volume rm "$volume" >/dev/null 2>&1 || true
  rm -rf "$workdir"
  exit "$status"
}
trap cleanup EXIT

fail() {
  echo "smoke check failed: $*" >&2
  exit 1
}

wait_for() {
  local what=$1
  shift
  for _ in $(seq 60); do
    if "$@" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  fail "timed out waiting for $what"
}

docker volume create "$volume" >/dev/null
docker run -d --name "$name" \
  -e LODESTORE_ADMIN_PASSWORD=smoke-test-password \
  -e LODESTORE_PUBLIC_URL=http://127.0.0.1:18080 \
  -e LODESTORE_GRPC_PUBLIC_ADDRESS=127.0.0.1:18443 \
  -p 18080:8080 -p 18443:8443 \
  -v "$volume:/var/lib/lodestore" \
  "$image" >/dev/null

wait_for /healthz curl -fsS "$base/healthz"

curl -fsS "$base/" | grep -qi '<html' || fail "GET / did not return HTML"

for arch in x86_64 aarch64; do
  sum=$(curl -fsS "$base/install/agent/$arch.sha256")
  echo "$sum" | grep -Eq '^[0-9a-f]{64}$' || fail "bad checksum for $arch: $sum"
done

curl -fsS "$base/install.sh" | grep -q "$base" || fail "install.sh does not mention $base"

user=$(docker inspect --format '{{.Config.User}}' "$name")
[ "$user" = "65532:65532" ] || fail "container user is $user"

healthy() {
  [ "$(docker inspect --format '{{.State.Health.Status}}' "$name")" = healthy ]
}
wait_for "healthcheck status healthy" healthy

if docker logs "$name" 2>&1 | grep -qi 'permission denied'; then
  fail "container logs contain permission denied"
fi

docker cp "$name:/usr/share/lodestore/agents" "$workdir/agents"
docker cp "$name:/usr/local/bin/lodestore-server" "$workdir/lodestore-server"
for bin in "$workdir"/agents/* "$workdir/lodestore-server"; do
  file "$bin" | grep -q 'statically linked' || fail "$bin is not statically linked"
done

echo "smoke ok"
