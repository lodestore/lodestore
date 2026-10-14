#!/usr/bin/env bash
# Runs the tests against PostgreSQL 16 in Docker as well as SQLite.
#
#   scripts/test-postgres.sh [nextest-args...]
#
# The container `lodestore-test-pg` is started when it is not running and is left
# running for the next invocation. Each test creates its own database in it.
set -euo pipefail

name=lodestore-test-pg

if ! docker ps --format '{{.Names}}' | grep -qx "$name"; then
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --rm --name "$name" \
    -e POSTGRES_PASSWORD=lodestore \
    -p 127.0.0.1:55432:5432 \
    postgres:16 >/dev/null
fi

until docker exec "$name" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1; do
  sleep 0.5
done

export LODESTORE_TEST_POSTGRES_URL=postgres://postgres:lodestore@127.0.0.1:55432/postgres
export LODESTORE_TEST_REQUIRE_POSTGRES=1

cd "$(dirname "$0")/.."
exec cargo nextest run "$@"
