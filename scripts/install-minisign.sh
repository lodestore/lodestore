#!/usr/bin/env bash
set -euo pipefail

version=0.12
sha256=9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSLo "$tmp/minisign.tar.gz" "https://github.com/jedisct1/minisign/releases/download/$version/minisign-$version-linux.tar.gz"
echo "$sha256  $tmp/minisign.tar.gz" | sha256sum --check
tar -xzf "$tmp/minisign.tar.gz" -C "$tmp"
sudo install -m 0755 "$tmp/minisign-linux/$(uname -m)/minisign" /usr/local/bin/minisign
minisign -v
