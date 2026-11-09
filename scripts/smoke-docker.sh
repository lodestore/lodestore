#!/usr/bin/env bash
set -euo pipefail

image=${1:?usage: smoke-docker.sh <image> [--signed]}
signed=0
case "${2:-}" in
  "") ;;
  --signed) signed=1 ;;
  *) echo "usage: smoke-docker.sh <image> [--signed]" >&2; exit 2 ;;
esac
root=$(cd "$(dirname "$0")/.." && pwd)
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

curl -fsS "$base/" -o "$workdir/index.html"
grep -qi '<html' "$workdir/index.html" || fail "GET / did not return HTML"

for arch in x86_64 aarch64; do
  sum=$(curl -fsS "$base/install/agent/$arch.sha256")
  echo "$sum" | grep -Eq '^[0-9a-f]{64}$' || fail "bad checksum for $arch: $sum"
done

curl -fsS "$base/install.sh" -o "$workdir/install.sh"
grep -q "$base" "$workdir/install.sh" || fail "install.sh does not mention $base"

user=$(docker inspect --format '{{.Config.User}}' "$name")
[ "$user" = "65532:65532" ] || fail "container user is $user"

healthy() {
  [ "$(docker inspect --format '{{.State.Health.Status}}' "$name")" = healthy ]
}
wait_for "healthcheck status healthy" healthy

docker logs "$name" >"$workdir/container.log" 2>&1
if grep -qi 'permission denied' "$workdir/container.log"; then
  fail "container logs contain permission denied"
fi

docker cp "$name:/usr/share/lodestore/agents" "$workdir/agents"
docker cp "$name:/usr/local/bin/lodestore-server" "$workdir/lodestore-server"
for bin in "$workdir"/agents/* "$workdir/lodestore-server"; do
  case "$bin" in *.minisig) continue ;; esac
  file "$bin" | grep -q 'statically linked' || fail "$bin is not statically linked"
done

if [ "$signed" = 1 ]; then
  for arch in x86_64 aarch64; do
    bin=$workdir/agents/lodestore-agent-linux-$arch
    [ -f "$bin.minisig" ] || fail "no signature next to the $arch agent"
    minisign -V -p "$root/crates/lodestore-agent/keys/project.pub" -m "$bin" >/dev/null ||
      fail "signature of the $arch agent does not verify"
    curl -fsS "$base/install/agent/$arch.minisig" -o "$workdir/served-$arch.minisig"
    cmp -s "$workdir/served-$arch.minisig" "$bin.minisig" ||
      fail "the served signature of the $arch agent differs from the image"
  done
  project_key=$(sed -n 2p "$root/crates/lodestore-agent/keys/project.pub")
  grep -qF "PROJECT_KEY='$project_key'" "$workdir/install.sh" ||
    fail "install.sh does not carry the project key"
fi

echo "smoke ok"
