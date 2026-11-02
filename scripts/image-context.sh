#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <dist-dir> <context-dir>" >&2
  exit 2
fi
dist=$1
ctx=$2

mkdir -p "$ctx/agents" "$ctx/rootfs/var/lib/lodestore"
found=0
for pair in x86_64:amd64 aarch64:arm64; do
  arch=${pair%%:*}
  docker_arch=${pair##*:}
  server="$dist/lodestore-server-linux-$arch"
  [ -f "$server" ] || continue
  mkdir -p "$ctx/linux-$docker_arch"
  cp "$server" "$ctx/linux-$docker_arch/lodestore-server"
  chmod 0755 "$ctx/linux-$docker_arch/lodestore-server"
  found=1
done
if [ "$found" = 0 ]; then
  echo "no lodestore-server-linux-* binary in $dist" >&2
  exit 1
fi

shopt -s nullglob
agents=("$dist"/lodestore-agent-linux-*)
if [ "${#agents[@]}" -eq 0 ]; then
  echo "no lodestore-agent-linux-* binary in $dist" >&2
  exit 1
fi
for agent in "${agents[@]}"; do
  cp "$agent" "$ctx/agents/"
  case "$agent" in
    *.minisig) chmod 0644 "$ctx/agents/$(basename "$agent")" ;;
    *) chmod 0755 "$ctx/agents/$(basename "$agent")" ;;
  esac
done
