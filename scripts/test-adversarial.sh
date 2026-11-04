#!/usr/bin/env bash
# Runs the compromised-agent suite: every test named adv_*.
#
#   scripts/test-adversarial.sh [nextest-args...]
set -euo pipefail

cd "$(dirname "$0")/.."

exec cargo nextest run --workspace --profile adversarial "$@"
