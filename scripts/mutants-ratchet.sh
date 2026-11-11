#!/usr/bin/env bash
# Ratchet over cargo-mutants results. A survivor is identified by its name: the missed.txt line
# without the ":line:col" after the path, so a shift of the code does not change it. The baseline
# pins, per group, the number of mutants and the multiset of survivor names; one new name fails
# even when the count does not grow.
#
#   scripts/mutants-ratchet.sh check <baseline.json> <group> <mutants.out dir>...
#   scripts/mutants-ratchet.sh --skips <baseline.json>
#   scripts/mutants-ratchet.sh --lower-only <baseline.json> <base-ref>
#   scripts/mutants-ratchet.sh --self-test
#
# Baseline: {"skips": N|null, "groups": {"<group>": null | {"total": N, "survivors": ["<path>: <description>", ...]}}}
# A null value means no baseline yet: results are printed and the exit code stays 0.
# Exit codes: 0 ok, 1 ratchet violated, 2 bad usage or missing input.
# check and --skips read the tree at $MUTANTS_ROOT (default: the repository of this script).
# --lower-only reads the baseline of <base-ref> from the git repository in the current directory.
set -euo pipefail
export LC_ALL=C

self=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
root=${MUTANTS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
deletion_re='^(crates/lodestore-format/src/gc/|crates/lodestore-format/src/locks\.rs|crates/lodestore-server/src/maintenance/gc/|crates/lodestore-policy/src/retention\.rs)'

die() { echo "$*" >&2; exit 2; }

names_of() {
  local dir
  for dir in "$@"; do
    [ -f "$dir/missed.txt" ] && sed -E 's/^([^:]+):[0-9]+(:[0-9]+)?:/\1:/' "$dir/missed.txt"
  done | sort
  return 0
}

sum_field() {
  local field=$1
  shift
  local files=() dir
  for dir in "$@"; do files+=("$dir/outcomes.json"); done
  jq -s --arg f "$field" 'map(.[$f] // 0) | add' "${files[@]}"
}

check() {
  local baseline=$1 group=$2
  shift 2
  [ -f "$baseline" ] || die "no baseline file: $baseline"
  [ "$#" -gt 0 ] || die "no mutants.out directories given"
  local dir
  for dir in "$@"; do
    [ -f "$dir/outcomes.json" ] || die "no outcomes.json in $dir"
  done
  jq -e --arg g "$group" '.groups | has($g)' "$baseline" >/dev/null || die "group $group is not in $baseline"

  local total caught missed timeout unviable
  total=$(sum_field total_mutants "$@")
  caught=$(sum_field caught "$@")
  missed=$(sum_field missed "$@")
  timeout=$(sum_field timeout "$@")
  unviable=$(sum_field unviable "$@")
  local tmp
  tmp=$(mktemp -d)
  names_of "$@" >"$tmp/now"
  echo "mutants $group: total $total, caught $caught, missed $missed, timeout $timeout, unviable $unviable"
  if [ -s "$tmp/now" ]; then
    echo "survivors of $group:"
    sed 's/^/  /' "$tmp/now"
  fi

  if [ "$(jq -r --arg g "$group" '.groups[$g] == null' "$baseline")" = true ]; then
    echo "mutants $group: $(wc -l <"$tmp/now" | tr -d ' ') survivors of $total, no baseline yet"
    rm -rf "$tmp"
    return 0
  fi

  local bad=0 base_total
  base_total=$(jq -r --arg g "$group" '.groups[$g].total' "$baseline")
  jq -r --arg g "$group" '.groups[$g].survivors[]?' "$baseline" | sort >"$tmp/base"

  if grep -Eq "$deletion_re" "$tmp/now"; then
    echo "survivors in deletion-decision code (threshold 0):" >&2
    grep -E "$deletion_re" "$tmp/now" | sed 's/^/  /' >&2
    bad=1
  fi
  if [ "$total" -lt "$base_total" ]; then
    echo "mutants went missing: $total < $base_total; a shard is missing, the run was narrowed, or code was removed — then lower total in the baseline" >&2
    bad=1
  fi
  if [ -n "$(comm -13 "$tmp/base" "$tmp/now")" ]; then
    echo "new survivors not in the baseline:" >&2
    comm -13 "$tmp/base" "$tmp/now" | sed 's/^/  /' >&2
    bad=1
  fi
  comm -23 "$tmp/base" "$tmp/now" | sed 's/^/remove from baseline: /'
  [ "$total" -gt "$base_total" ] && echo "raise total in the baseline: $total > $base_total"
  rm -rf "$tmp"
  return "$bad"
}

skips() {
  local baseline=$1
  [ -f "$baseline" ] || die "no baseline file: $baseline"
  local bad=0 count=0 hits file line prev
  hits=$(grep -rnE '^[[:space:]]*#\[.*mutants::skip' --include='*.rs' "$root"/crates/*/src 2>/dev/null | sort || true)
  while IFS=: read -r file line _; do
    [ -n "$file" ] || continue
    count=$((count + 1))
    prev=$(sed -n "$((line - 1))p" "$file")
    if ! printf '%s\n' "$prev" | grep -Eq '^[[:space:]]*//'; then
      echo "mutants::skip without a reason comment above: ${file#"$root"/}:$line" >&2
      bad=1
    fi
  done <<<"$hits"
  if [ -f "$root/.cargo/mutants.toml" ] && grep -Eq '^[[:space:]]*exclude_' "$root/.cargo/mutants.toml"; then
    echo "exclude_* in .cargo/mutants.toml: skip a mutant with #[mutants::skip] and a reason instead" >&2
    bad=1
  fi
  local pinned
  pinned=$(jq -r '.skips // "null"' "$baseline")
  if [ "$pinned" = null ]; then
    echo "mutants::skip count: $count, no baseline yet"
  elif [ "$count" -gt "$pinned" ]; then
    echo "mutants::skip count grew: $count > $pinned" >&2
    bad=1
  else
    echo "mutants::skip count: $count (baseline $pinned)"
    [ "$count" -lt "$pinned" ] && echo "lower skips in the baseline: $count < $pinned"
  fi
  return "$bad"
}

lower_only() {
  local file=$1 base_ref=$2 tmp old bad=0 group
  [ -f "$file" ] || die "no baseline file: $file"
  tmp=$(mktemp -d)
  if ! git show "$base_ref:$file" >"$tmp/old.json" 2>/dev/null; then
    echo "no baseline on $base_ref"
    rm -rf "$tmp"
    return 0
  fi
  old="$tmp/old.json"
  for group in $(jq -r '.groups | keys[]' "$file"); do
    local old_total new_total
    old_total=$(jq -r --arg g "$group" '.groups[$g].total // "null"' "$old")
    new_total=$(jq -r --arg g "$group" '.groups[$g].total // "null"' "$file")
    jq -r --arg g "$group" '.groups[$g].survivors[]?' "$old" | sort >"$tmp/old.names"
    jq -r --arg g "$group" '.groups[$g].survivors[]?' "$file" | sort >"$tmp/new.names"
    if grep -Eq "$deletion_re" "$tmp/new.names"; then
      echo "$group: survivors in deletion-decision code may not be baselined:" >&2
      grep -E "$deletion_re" "$tmp/new.names" | sed 's/^/  /' >&2
      bad=1
    fi
    if [ "$old_total" = null ]; then
      continue
    fi
    if [ "$new_total" = null ]; then
      echo "$group: baseline went from a number to null" >&2
      bad=1
      continue
    fi
    if [ -n "$(comm -13 "$tmp/old.names" "$tmp/new.names")" ]; then
      echo "$group: new survivors added to the baseline:" >&2
      comm -13 "$tmp/old.names" "$tmp/new.names" | sed 's/^/  /' >&2
      bad=1
    fi
    if [ "$new_total" -lt "$old_total" ]; then
      echo "total of $group lowered from $old_total to $new_total"
    fi
  done
  local old_skips new_skips
  old_skips=$(jq -r '.skips // "null"' "$old")
  new_skips=$(jq -r '.skips // "null"' "$file")
  if [ "$old_skips" != null ]; then
    if [ "$new_skips" = null ] || [ "$new_skips" -gt "$old_skips" ]; then
      echo "skips went from $old_skips to $new_skips" >&2
      bad=1
    fi
  fi
  rm -rf "$tmp"
  return "$bad"
}

self_test() {
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  local rc out
  expect() {
    local want=$1 label=$2
    shift 2
    rc=0
    out=$("$@" 2>&1) || rc=$?
    if [ "$rc" -ne "$want" ]; then
      echo "self-test failed: $label: expected exit $want, got $rc" >&2
      echo "$out" >&2
      exit 1
    fi
  }
  expect_out() {
    printf '%s\n' "$out" | grep -Fq -- "$1" || { echo "self-test failed: output lacks: $1" >&2; echo "$out" >&2; exit 1; }
  }
  mkout() {
    local dir=$1 total=$2 caught=$3 missed=$4
    shift 4
    mkdir -p "$dir"
    printf '{"total_mutants": %s, "caught": %s, "missed": %s, "timeout": 0, "unviable": 1}\n' "$total" "$caught" "$missed" >"$dir/outcomes.json"
    : >"$dir/missed.txt"
    local line
    for line in "$@"; do printf '%s\n' "$line" >>"$dir/missed.txt"; done
  }
  local a='crates/x/src/a.rs:10:5: replace f -> bool with true'
  local b='crates/x/src/b.rs:20:7: replace g -> u8 with 0'
  local c='crates/x/src/c.rs:30:9: delete ! in h'
  local ak='crates/x/src/a.rs: replace f -> bool with true'
  local bk='crates/x/src/b.rs: replace g -> u8 with 0'
  local d='crates/lodestore-format/src/gc/mod.rs:5:1: replace sweep with ()'

  echo '{"skips": null, "groups": {"g": null}}' >"$tmp/null.json"
  mkout "$tmp/r1" 10 7 2 "$a" "$b"
  expect 0 "null baseline" "$self" check "$tmp/null.json" g "$tmp/r1"
  expect_out "no baseline yet"
  expect 2 "unknown group" "$self" check "$tmp/null.json" nope "$tmp/r1"
  expect 2 "missing outcomes" "$self" check "$tmp/null.json" g "$tmp/none"

  printf '{"skips": 0, "groups": {"g": {"total": 10, "survivors": ["%s", "%s"]}}}\n' "$ak" "$bk" >"$tmp/base.json"
  expect 0 "same composition" "$self" check "$tmp/base.json" g "$tmp/r1"
  mkout "$tmp/r2" 10 7 2 "$a" "$c"
  expect 1 "one survivor swapped for another" "$self" check "$tmp/base.json" g "$tmp/r2"
  expect_out "crates/x/src/c.rs: delete ! in h"
  mkout "$tmp/r3" 10 8 1 "$a"
  expect 0 "fewer survivors" "$self" check "$tmp/base.json" g "$tmp/r3"
  expect_out "remove from baseline: $bk"

  printf '{"skips": 0, "groups": {"g": {"total": 10, "survivors": ["%s"]}}}\n' "$ak" >"$tmp/dup.json"
  mkout "$tmp/r4" 10 7 2 "$a" "$a"
  expect 1 "repeated name counted" "$self" check "$tmp/dup.json" g "$tmp/r4"

  mkout "$tmp/del" 10 8 1 "$d"
  printf '{"skips": 0, "groups": {"g": {"total": 10, "survivors": ["crates/lodestore-format/src/gc/mod.rs: replace sweep with ()"]}}}\n' >"$tmp/delbase.json"
  expect 1 "survivor in deletion code" "$self" check "$tmp/delbase.json" g "$tmp/del"
  expect 0 "survivor in deletion code, null baseline" "$self" check "$tmp/null.json" g "$tmp/del"

  mkout "$tmp/s1" 4 3 1 "$a"
  mkout "$tmp/s2" 4 3 1 "$b"
  expect 1 "shards sum below total" "$self" check "$tmp/base.json" g "$tmp/s1" "$tmp/s2"
  expect_out "mutants went missing: 8 < 10"
  mkout "$tmp/s3" 6 5 1 "$b"
  mkout "$tmp/s4" 6 5 1 "$a"
  expect 0 "shards sum above total" "$self" check "$tmp/base.json" g "$tmp/s3" "$tmp/s4"
  expect_out "raise total in the baseline: 12 > 10"
  expect 1 "one shard only" "$self" check "$tmp/base.json" g "$tmp/s3"

  local tree=$tmp/tree
  mkdir -p "$tree/crates/x/src" "$tree/.cargo"
  : >"$tree/.cargo/mutants.toml"
  printf '// reason\n#[mutants::skip]\nfn f() {}\n' >"$tree/crates/x/src/ok.rs"
  expect 0 "skip with a reason, null" env MUTANTS_ROOT="$tree" "$self" --skips "$tmp/null.json"
  printf '{"skips": 1, "groups": {}}\n' >"$tmp/skips1.json"
  expect 0 "skip within the pinned count" env MUTANTS_ROOT="$tree" "$self" --skips "$tmp/skips1.json"
  printf '{"skips": 0, "groups": {}}\n' >"$tmp/skips0.json"
  expect 1 "skip count above the baseline" env MUTANTS_ROOT="$tree" "$self" --skips "$tmp/skips0.json"
  printf 'fn g() {}\n#[mutants::skip]\nfn h() {}\n' >"$tree/crates/x/src/bad.rs"
  expect 1 "skip without a comment" env MUTANTS_ROOT="$tree" "$self" --skips "$tmp/null.json"
  expect_out "bad.rs:2"
  rm "$tree/crates/x/src/bad.rs"
  printf 'exclude_re = ["foo"]\n' >"$tree/.cargo/mutants.toml"
  expect 1 "exclude_re in the config" env MUTANTS_ROOT="$tree" "$self" --skips "$tmp/null.json"

  local repo=$tmp/repo
  mkdir -p "$repo/.github"
  (
    cd "$repo"
    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
    export GIT_AUTHOR_NAME=Ada GIT_AUTHOR_EMAIL=ada@example.com GIT_COMMITTER_NAME=Ada GIT_COMMITTER_EMAIL=ada@example.com
    git init -q -b main .
    f=.github/mutants-baseline.json
    commit_with() {
      printf '%s\n' "$1" >"$f"
      git add "$f"
      git commit -q -m "$2"
    }
    lo() { "$self" --lower-only "$f" main; }
    want() {
      local code=$1 label=$2 got=0
      lo >/dev/null 2>&1 || got=$?
      [ "$got" -eq "$code" ] || { echo "self-test failed: lower-only: $label: expected exit $code, got $got" >&2; exit 1; }
    }
    commit_with "{\"skips\": 2, \"groups\": {\"g\": {\"total\": 10, \"survivors\": [\"$ak\"]}, \"h\": null}}" base
    printf '{"skips": 2, "groups": {"g": {"total": 10, "survivors": ["%s"]}, "h": null}}\n' "$ak" >"$f"
    want 0 "unchanged"
    printf '{"skips": 2, "groups": {"g": {"total": 10, "survivors": ["%s", "%s"]}, "h": null}}\n' "$ak" "$bk" >"$f"
    want 1 "new name"
    printf '{"skips": 2, "groups": {"g": {"total": 9, "survivors": ["%s"]}, "h": null}}\n' "$ak" >"$f"
    want 0 "total lowered"
    lo 2>&1 | grep -Fq 'total of g lowered from 10 to 9' || { echo "self-test failed: lowered total not printed" >&2; exit 1; }
    printf '{"skips": 3, "groups": {"g": {"total": 10, "survivors": ["%s"]}, "h": null}}\n' "$ak" >"$f"
    want 1 "skips grew"
    printf '{"skips": null, "groups": {"g": {"total": 10, "survivors": ["%s"]}, "h": null}}\n' "$ak" >"$f"
    want 1 "skips to null"
    printf '{"skips": 2, "groups": {"g": null, "h": null}}\n' >"$f"
    want 1 "number to null"
    printf '{"skips": 2, "groups": {"g": {"total": 10, "survivors": ["%s"]}, "h": {"total": 4, "survivors": []}}}\n' "$ak" >"$f"
    want 0 "first baseline of a null group"
    printf '{"skips": 2, "groups": {"g": {"total": 10, "survivors": ["%s", "crates/lodestore-policy/src/retention.rs: replace x with ()"]}, "h": null}}\n' "$ak" >"$f"
    want 1 "deletion survivor"
  )
  echo "self-test ok"
}

case "${1:-}" in
  check) shift; [ "$#" -ge 3 ] || die "usage: $0 check <baseline.json> <group> <mutants.out dir>..."; check "$@" ;;
  --skips) shift; [ "$#" -eq 1 ] || die "usage: $0 --skips <baseline.json>"; skips "$@" ;;
  --lower-only) shift; [ "$#" -eq 2 ] || die "usage: $0 --lower-only <baseline.json> <base-ref>"; lower_only "$@" ;;
  --self-test) self_test ;;
  *) die "usage: $0 check|--skips|--lower-only|--self-test" ;;
esac
