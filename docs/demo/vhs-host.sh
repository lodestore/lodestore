#!/bin/bash
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
out=$here/.out
media=$root/docs/src/assets/media
machine=lodestore-test
version=0.1.0

fail() {
  echo "vhs failed: $*" >&2
  exit 1
}

# Every command runs in the machine as its regular user, which has sudo.
in_machine() {
  orb run -m "$machine" "$@"
}

wait_for_systemd() {
  for _ in $(seq 60); do
    case $(in_machine systemctl is-system-running 2>/dev/null || true) in
      running | degraded) return 0 ;;
    esac
    sleep 2
  done
  fail "systemd in $machine did not come up"
}

# Removes everything a recording leaves in the machine, and what an earlier
# install test left in it, so the agent installs as a new host.
wipe() {
  in_machine bash -s <<'EOF'
set +e
sudo docker rm -f lodestore >/dev/null 2>&1
sudo docker volume rm lodestore-data >/dev/null 2>&1
sudo systemctl stop lodestore-vhs-minio >/dev/null 2>&1
sudo systemctl reset-failed lodestore-vhs-minio >/dev/null 2>&1
sudo systemctl disable --now lodestore-agent >/dev/null 2>&1
sudo rm -f /etc/systemd/system/lodestore-agent.service /usr/local/bin/lodestore-agent /usr/local/bin/lodestore-agent.prev
sudo systemctl daemon-reload
sudo rm -rf /etc/lodestore /var/lib/lodestore /srv/documents "$HOME/.vhs"
exit 0
EOF
}

prepare() {
  command -v orb >/dev/null || fail "OrbStack is required"
  command -v vhs >/dev/null || fail "vhs is required (brew install vhs)"
  orb start "$machine" >/dev/null
  wait_for_systemd
  wipe

  # shellcheck source=/dev/null
  . "$root/scripts/pins.env"
  in_machine env VERSION="$version" MINIO_RELEASE="$MINIO_RELEASE" MC_RELEASE="$MC_RELEASE" \
    MINIO_SHA256="$MINIO_SHA256_LINUX_ARM64" MC_SHA256="$MC_SHA256_LINUX_ARM64" \
    MINIO_SHA256_X86="$MINIO_SHA256_LINUX_AMD64" MC_SHA256_X86="$MC_SHA256_LINUX_AMD64" \
    bash -s <<'EOF' || fail "preparing $machine"
set -euo pipefail
case $(uname -m) in
  aarch64) arch=arm64; lode_arch=aarch64 ;;
  x86_64) arch=amd64; lode_arch=x86_64; MINIO_SHA256=$MINIO_SHA256_X86; MC_SHA256=$MC_SHA256_X86 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

if ! command -v docker >/dev/null || ! command -v jq >/dev/null; then
  sudo apt-get update -qq
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io jq curl >/dev/null
fi
sudo systemctl enable --now docker >/dev/null 2>&1
sudo usermod -aG docker "$USER"
sudo docker pull -q "ghcr.io/lodestore/lodestore:$VERSION" >/dev/null

dir=$HOME/.vhs
mkdir -p "$dir"
fetch() {
  local url=$1 file=$2 sha=$3
  curl -fsSL -o "$file" "$url"
  echo "$sha  $file" | sha256sum --check --status || { echo "sha256 mismatch for $file" >&2; exit 1; }
  chmod 0755 "$file"
}

release=https://github.com/lodestore/lodestore/releases/download/v$VERSION
curl -fsSL -o "$dir/SHA256SUMS" "$release/SHA256SUMS"
sha=$(awk -v n="lode-linux-$lode_arch" '$2 == n || $2 == "*" n { print $1; exit }' "$dir/SHA256SUMS")
[ -n "$sha" ] || { echo "SHA256SUMS has no entry for lode-linux-$lode_arch" >&2; exit 1; }
fetch "$release/lode-linux-$lode_arch" "$dir/lode" "$sha"
sudo install -m 0755 "$dir/lode" /usr/local/bin/lode

fetch "https://github.com/minio/minio/releases/download/$MINIO_RELEASE/minio.linux-$arch.$MINIO_RELEASE" "$dir/minio" "$MINIO_SHA256"
fetch "https://github.com/minio/mc/releases/download/$MC_RELEASE/mc.linux-$arch.$MC_RELEASE" "$dir/mc-bin" "$MC_SHA256"
printf '#!/bin/sh\nexec %s --config-dir %s --no-color "$@"\n' "$dir/mc-bin" "$dir/mc-config" >"$dir/mc"
chmod 0755 "$dir/mc"

user=$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')
password=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
(
  umask 077
  printf 'MINIO_ROOT_USER=%s\nMINIO_ROOT_PASSWORD=%s\n' "$user" "$password" >"$dir/env"
)
mkdir -p "$dir/minio-data"
sudo systemd-run --quiet --unit=lodestore-vhs-minio --uid="$(id -u)" \
  --setenv=MINIO_ROOT_USER="$user" --setenv=MINIO_ROOT_PASSWORD="$password" \
  "$dir/minio" server "$dir/minio-data" --address 0.0.0.0:9000 --console-address 127.0.0.1:9001
for _ in $(seq 30); do
  curl -fsS http://127.0.0.1:9000/minio/health/ready >/dev/null 2>&1 && break
  sleep 1
done
for _ in $(seq 30); do
  "$dir/mc" alias set qs http://127.0.0.1:9000 "$user" "$password" >/dev/null 2>&1 && break
  sleep 1
done
"$dir/mc" mb qs/quickstart >/dev/null
EOF

  in_machine bash -c 'cat >"$HOME/.vhs/session.sh"' <"$here/vhs-session.sh" ||
    fail "copying vhs-session.sh into $machine"
}

cleanup() {
  wipe || true
  orb stop "$machine" >/dev/null 2>&1 || true
}

record() {
  trap cleanup EXIT
  mkdir -p "$out" "$media"
  cd "$here"
  prepare
  vhs tapes/install-first-backup.tape
  cp "$out/install-first-backup.gif" "$media/install-first-backup.gif"
}

case "${1:-}" in
  prepare) prepare ;;
  cleanup) cleanup ;;
  record) record ;;
  *) echo "usage: vhs-host.sh prepare|cleanup|record" >&2; exit 2 ;;
esac
