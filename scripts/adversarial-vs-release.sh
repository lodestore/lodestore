#!/usr/bin/env bash
# Proves that every compromised-agent scenario (tests named adv_*) fails
# against a release: a scenario that passes there, or that does not compile
# there, proves nothing about the fix.
#
#   scripts/adversarial-vs-release.sh [TAG]      (default: v0.1.0)
#
# The current adv_*.rs files and the helper modules they share (adv_support,
# gc_support, common) are copied into a detached worktree of TAG, built and
# run there with the default nextest profile. The run exits non-zero if any
# scenario passes, is not run, or does not compile on TAG.
#
# Heavy: it builds the whole workspace a second time. Run it by hand at the
# end of a phase, not in CI. The worktree and its target/ are removed on exit.
set -euo pipefail

TAG="${1:-v0.1.0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/lode-adv-vs-release.XXXXXX")"
WT="$WORK/tree"
cleanup() {
    git -C "$ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
    rm -rf "$WORK"
    git -C "$ROOT" worktree prune
}
trap cleanup EXIT

git worktree add --detach "$WT" "$TAG" >/dev/null

for tests in crates/*/tests; do
    [ -d "$tests" ] || continue
    for support in adv_support gc_support common; do
        if [ -d "$tests/$support" ]; then
            mkdir -p "$WT/$tests"
            rm -rf "${WT:?}/$tests/$support"
            cp -R "$tests/$support" "$WT/$tests/$support"
        fi
    done
    for file in "$tests"/adv_*.rs; do
        [ -f "$file" ] || continue
        mkdir -p "$WT/$tests"
        cp "$file" "$WT/$file"
    done
done

TABLE="$WORK/table"
BAD=0
: >"$TABLE"

for file in crates/*/tests/adv_*.rs; do
    [ -f "$file" ] || continue
    crate="$(echo "$file" | cut -d/ -f2)"
    stem="$(basename "$file" .rs)"
    names="$(grep -oE 'fn adv_[a-z0-9_]+' "$file" | sed 's/^fn //')"
    log="$WORK/$stem.log"
    status=0
    (
        cd "$WT"
        NO_COLOR=1 cargo nextest run -p "$crate" --test "$stem" --no-fail-fast \
            --color never -E 'test(/^adv_/)'
    ) >"$log" 2>&1 || status=$?
    echo "== $stem: exit $status"
    grep -A1 'panicked at' "$log" | cut -c1-300 | head -n 24 || true
    if [ "$status" -ne 0 ] && grep -q 'could not compile' "$log"; then
        echo "$stem | (all) | does not compile on $TAG" >>"$TABLE"
        BAD=1
        tail -n 30 "$log"
        continue
    fi
    for name in $names; do
        result="$(grep -E "^ +(PASS|FAIL|TIMEOUT|SIG[A-Z]+|ABORT|LEAK[A-Z-]*) .*::${stem} ${name} *\$" "$log" \
            | head -n 1 | awk '{print $1}' || true)"
        case "$result" in
            "") result="NOT RUN"; BAD=1; tail -n 15 "$log" ;;
            PASS) BAD=1 ;;
        esac
        echo "$stem::$name | $result on $TAG" >>"$TABLE"
    done
done

echo
echo "test | result on $TAG"
echo "-----|-----"
cat "$TABLE"

if [ ! -s "$TABLE" ]; then
    echo "no adv_ scenario found" >&2
    exit 1
fi
if [ "$BAD" -ne 0 ]; then
    echo >&2
    echo "FAILED: every scenario must FAIL on $TAG (see PASS, NOT RUN or does not compile above)" >&2
    exit 1
fi
echo
echo "OK: every scenario fails on $TAG"
