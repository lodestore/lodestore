#!/usr/bin/env bash
# Runs the Linux tests in the OrbStack machine `lodestore-test` from a macOS host.
#
#   [PKG=lode-cli] [BINS=lodestore-agent] scripts/test-linux.sh [--root] [nextest-args...]
#
# Without --root the tests run as the machine's regular user. With --root they run
# as root and include the #[ignore]d tests that need CAP_CHOWN, CAP_MKNOD and friends.
# BINS names a package whose binary is built before the tests run (the install
# tests need the agent). Both modes share one build directory inside the machine.
# The machine is started for the run and stopped when it ends. On a Linux host
# without OrbStack (a CI runner) the tests run on the host itself, with sudo for
# --root.
#
# One-time setup:
#   orb create ubuntu lodestore-test
#   orb config set machine.lodestore-test.cpu 4
#   orb config set machine.lodestore-test.memory_mib 4096
#   orb run -m lodestore-test bash -c 'sudo apt-get install -y build-essential curl acl pkg-config &&
#     curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.0 &&
#     curl -LsSf https://get.nexte.st/latest/linux-arm | tar zxf - -C ~/.cargo/bin'
set -euo pipefail

root=0
if [ "${1:-}" = "--root" ]; then
  root=1
  shift
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
pkg=${PKG:-lodestore-format}
bins=${BINS:-}

if [ "$(uname -s)" = Linux ] && ! command -v orb >/dev/null 2>&1; then
  cd "$repo"
  [ -z "$bins" ] || cargo build --locked -p "$bins"
  if [ "$root" = 0 ]; then
    exec cargo nextest run --locked -p "$pkg" "$@"
  fi
  target=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
  trap 'sudo chown -R "$(id -u):$(id -g)" "$target"' EXIT
  sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME,CARGO_TARGET_DIR,LODE_GENERATE_FIXTURES,CI \
    env HOME=/root CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    cargo nextest run --locked -p "$pkg" --run-ignored all "$@"
  exit
fi

trap 'orb stop lodestore-test >/dev/null 2>&1' EXIT
orb run -m lodestore-test -w "$repo" bash -euo pipefail -c '
  root=$1 pkg=$2 gen=$3 bins=$4
  shift 4
  [ -z "$gen" ] || export LODE_GENERATE_FIXTURES=$gen
  . "$HOME/.cargo/env"
  export CARGO_TARGET_DIR=$HOME/lodestore-target CARGO_BUILD_JOBS=4
  [ -z "$bins" ] || cargo build --locked -p "$bins"
  if [ "$root" = 0 ]; then
    exec cargo nextest run --locked -p "$pkg" "$@"
  fi
  trap "sudo chown -R $(id -u):$(id -g) \"\$CARGO_TARGET_DIR\" \"\$HOME/.cargo\" \"\$HOME/.rustup\"" EXIT
  sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME,CARGO_TARGET_DIR,CARGO_BUILD_JOBS,LODE_GENERATE_FIXTURES \
    env HOME=/root CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    cargo nextest run --locked -p "$pkg" --run-ignored all "$@"
' bash "$root" "$pkg" "${LODE_GENERATE_FIXTURES:-}" "$bins" "$@"
