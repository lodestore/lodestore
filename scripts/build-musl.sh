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

if [ -z "${SOURCE_DATE_EPOCH:-}" ] && git rev-parse --git-dir >/dev/null 2>&1; then
  SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)
  export SOURCE_DATE_EPOCH
fi
cargo_home=${CARGO_HOME:-$HOME/.cargo}
sysroot=$(rustc --print sysroot)
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$PWD=/build --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$sysroot=/rustc"
export CFLAGS="${CFLAGS:+$CFLAGS }-ffile-prefix-map=$PWD=/build -ffile-prefix-map=$cargo_home=/cargo"

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
    cargo zigbuild --release --locked --target "$target" -p lode-cli
    cp "target/$target/release/lode" "$out/lode-linux-$arch"
  fi
done

shopt -s nullglob
status=0
for bin in "$out"/lodestore-*-linux-* "$out"/lode-linux-*; do
  if file "$bin" | grep -q 'statically linked'; then
    echo "ok: $bin"
  else
    echo "not statically linked: $bin" >&2
    file "$bin" >&2
    status=1
  fi
done
exit "$status"
