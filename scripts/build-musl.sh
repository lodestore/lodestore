#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <out-dir> [--server] [<target>...]" >&2
  exit 2
}

[ "$#" -ge 1 ] || usage
out=$1
shift

server=0
targets=()
for arg in "$@"; do
  case "$arg" in
    --server) server=1 ;;
    *) targets+=("$arg") ;;
  esac
done
if [ "${#targets[@]}" -eq 0 ]; then
  targets=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
fi
mkdir -p "$out"

for target in "${targets[@]}"; do
  case "$target" in
    x86_64-unknown-linux-musl) arch=x86_64 ;;
    aarch64-unknown-linux-musl) arch=aarch64 ;;
    *) echo "unsupported target: $target" >&2; exit 2 ;;
  esac
  cargo zigbuild --release --locked --target "$target" -p lodestore-agent
  cp "target/$target/release/lodestore-agent" "$out/lodestore-agent-linux-$arch"
  if [ "$server" = 1 ]; then
    cargo zigbuild --release --locked --target "$target" -p lodestore-server --features embed-ui
    cp "target/$target/release/lodestore-server" "$out/lodestore-server-linux-$arch"
  fi
done

status=0
for bin in "$out"/lodestore-*-linux-*; do
  if file "$bin" | grep -q 'statically linked'; then
    echo "ok: $bin"
  else
    echo "not statically linked: $bin" >&2
    file "$bin" >&2
    status=1
  fi
done
exit "$status"
