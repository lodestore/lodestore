#!/usr/bin/env bash
# Signs the release binaries in DIST with minisign and writes a signed SHA256SUMS.
#
#   MINISIGN_KEY=... MINISIGN_PASSWORD=... scripts/release-sign.sh DIST VERSION
#   scripts/release-sign.sh --self-test
#
# Each binary gets a trusted comment "<product> VERSION <arch>". The server reads the
# comment of lodestore-agent-linux-<arch>.minisig and accepts exactly three words.
# RELEASE_PUB overrides the public key every signature is checked against.
set -euo pipefail
set +x
umask 077

root=$(cd "$(dirname "$0")/.." && pwd)
products=(lodestore-agent lodestore-server lode)
arches=(x86_64 aarch64)

die() {
  echo "release-sign: $*" >&2
  exit 1
}

valid_version() {
  [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]]
}

sign_file() {
  local key=$1 password=$2 comment=$3 file=$4
  printf '%s\n' "$password" | minisign -S -s "$key" -t "$comment" -m "$file" >/dev/null
}

sign_dist() {
  local dist=$1 version=$2 key=$3 password=$4 pub=$5
  local product arch file count=0 listed=()

  valid_version "$version" || die "bad version: $version"
  [ -d "$dist" ] || die "no such directory: $dist"

  for product in "${products[@]}"; do
    for arch in "${arches[@]}"; do
      file=$product-linux-$arch
      [ -f "$dist/$file" ] || die "missing $dist/$file"
      sign_file "$key" "$password" "$product $version $arch" "$dist/$file"
      listed+=("$file")
      count=$((count + 1))
    done
  done
  [ "$count" -eq 6 ] || die "expected 6 binaries, signed $count"

  (cd "$dist" && sha256sum "${listed[@]}" >SHA256SUMS)
  sign_file "$key" "$password" "lodestore release $version" "$dist/SHA256SUMS"

  for file in "${listed[@]}" SHA256SUMS; do
    [ -f "$dist/$file.minisig" ] || die "missing signature for $file"
    minisign -V -p "$pub" -m "$dist/$file" >/dev/null || die "signature of $file does not verify"
  done
}

self_test() {
  local dist product arch comment
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  dist=$tmp/dist
  mkdir "$dist"

  printf '%s\n%s\n' throwaway throwaway | minisign -G -p "$tmp/test.pub" -s "$tmp/test.key" >/dev/null
  for product in "${products[@]}"; do
    for arch in "${arches[@]}"; do
      printf '%s %s\n' "$product" "$arch" >"$dist/$product-linux-$arch"
    done
  done

  sign_dist "$dist" 0.0.0 "$tmp/test.key" throwaway "$tmp/test.pub"

  for arch in "${arches[@]}"; do
    comment=$(minisign -V -p "$tmp/test.pub" -m "$dist/lodestore-agent-linux-$arch" | sed -n 's/^Trusted comment: //p')
    [ "$comment" = "lodestore-agent 0.0.0 $arch" ] || die "self-test failed: comment for $arch is '$comment'"
  done
  for product in "${products[@]}"; do
    for arch in "${arches[@]}"; do
      [ -f "$dist/$product-linux-$arch.minisig" ] || die "self-test failed: no .minisig for $product $arch"
    done
  done
  [ -f "$dist/SHA256SUMS.minisig" ] || die "self-test failed: SHA256SUMS is not signed"
  (cd "$dist" && sha256sum --check --quiet SHA256SUMS) || die "self-test failed: SHA256SUMS does not match"

  rm -f "$dist/lode-linux-aarch64"
  if (sign_dist "$dist" 0.0.0 "$tmp/test.key" throwaway "$tmp/test.pub") 2>/dev/null; then
    die "self-test failed: a missing binary was accepted"
  fi
  echo "sign self-test ok"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit 0
fi

[ "$#" -eq 2 ] || die "usage: $0 DIST VERSION | --self-test"
[ -n "${MINISIGN_KEY:-}" ] || die "MINISIGN_KEY is not set"
[ -n "${MINISIGN_PASSWORD:-}" ] || die "MINISIGN_PASSWORD is not set"

keydir=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/sign.XXXXXX")
key=$keydir/minisign.key
cleanup() {
  if command -v shred >/dev/null 2>&1; then
    shred -u "$key" 2>/dev/null || true
  fi
  rm -rf "$keydir"
}
trap cleanup EXIT

printf '%s\n' "$MINISIGN_KEY" >"$key"
password=$MINISIGN_PASSWORD
unset MINISIGN_KEY MINISIGN_PASSWORD

sign_dist "$1" "$2" "$key" "$password" "${RELEASE_PUB:-$root/crates/lodestore-agent/keys/project.pub}"
echo "signed 6 binaries and SHA256SUMS for $2"
