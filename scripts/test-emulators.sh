#!/usr/bin/env bash
# Runs the tests that need the storage emulators (fake-gcs-server, Azurite, sshd).
#
#   scripts/test-emulators.sh [nextest-args...]
#
# Needs Docker (sshd), Node 22+ (Azurite) and network access on the first run.
set -euo pipefail

cd "$(dirname "$0")/.."
exec cargo nextest run --workspace --profile ci --run-ignored only -E 'binary(emulators)' "$@"
