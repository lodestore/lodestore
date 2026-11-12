#!/usr/bin/env bash
# Prints the release notes of VERSION: the body of the "## [VERSION]" section of CHANGELOG.md,
# without the heading, and the "## Verify this release" section with the commands that check
# the signatures, the build provenance and the SBOM of the release. With --commands it prints
# only those commands, without reading CHANGELOG.md: the release workflow runs them before it
# publishes, and cmp's them against the fence it cuts out of the rendered notes.
#
#   scripts/release-notes.sh VERSION [--image IMAGE] [--chart CHART] [--ref REF]
#   scripts/release-notes.sh --commands VERSION [--image IMAGE] [--chart CHART] [--ref REF]
#   scripts/release-notes.sh --self-test
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
self=$root/scripts/release-notes.sh

version_pattern='^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$'
image_pattern='^ghcr\.io/lodestore/[a-z0-9/_-]+$'
ref_pattern='^refs/(tags|heads)/[A-Za-z0-9._/-]+$'
issuer=https://token.actions.githubusercontent.com

die() {
  echo "release-notes: $*" >&2
  exit 1
}

usage() {
  echo "usage: $0 [--commands] VERSION [--image IMAGE] [--chart CHART] [--ref REF] | --self-test" >&2
  exit 2
}

changelog_body() {
  local version=$1 notes
  notes=$(awk -v heading="## [$version]" '
    index($0, heading) == 1 { inside = 1; next }
    inside && index($0, "## [") == 1 { exit }
    inside { print }
  ' "$root/CHANGELOG.md")

  if ! printf '%s\n' "$notes" | grep -q '[^[:space:]]'; then
    die "CHANGELOG.md has no notes for $version"
  fi
  printf '%s\n' "$notes"
}

# Failures inside the block end with false, not exit: under bash -e this stops a script, and
# in an interactive shell it does not close the terminal.
commands() {
  local version=$1 image=$2 chart=$3 ref=$4 key identity
  key=$(sed -n 2p "$root/crates/lodestore-agent/keys/project.pub")
  identity=https://github.com/lodestore/lodestore/.github/workflows/release.yml@$ref
  cat <<EOF
# The checksums must be signed by the project key as this release
[ "\$(minisign -Q -Vm SHA256SUMS -P $key)" = "lodestore release $version" ] || { echo "SHA256SUMS is not signed as lodestore release $version" >&2; false; }
sha256sum --check --ignore-missing SHA256SUMS

# The binaries must come from the release workflow, built from this tag
n=0
for f in lodestore-server-linux-x86_64 lodestore-server-linux-aarch64 lodestore-agent-linux-x86_64 lodestore-agent-linux-aarch64 lode-linux-x86_64 lode-linux-aarch64; do
  if [ -f "\$f" ]; then
    gh attestation verify "\$f" --repo lodestore/lodestore --bundle lodestore-$version.provenance.jsonl \\
      --cert-identity $identity \\
      --cert-oidc-issuer $issuer \\
      --source-ref $ref
    n=\$((n + 1))
  fi
done
[ "\$n" -gt 0 ] || { echo "no release binaries in this directory" >&2; false; }

# The image and the chart must be signed by the release workflow
cosign verify $image:$version \\
  --certificate-identity $identity \\
  --certificate-oidc-issuer $issuer
cosign verify $chart:$version \\
  --certificate-identity $identity \\
  --certificate-oidc-issuer $issuer

# The image must carry build provenance and an SBOM from the same workflow
gh attestation verify oci://$image:$version --repo lodestore/lodestore --bundle-from-oci \\
  --cert-identity $identity \\
  --cert-oidc-issuer $issuer \\
  --source-ref $ref
gh attestation verify oci://$image:$version --repo lodestore/lodestore --bundle-from-oci \\
  --predicate-type https://spdx.dev/Document/v2.3 \\
  --cert-identity $identity \\
  --cert-oidc-issuer $issuer \\
  --source-ref $ref
EOF
}

notes() {
  changelog_body "$1"
  cat <<EOF

## Verify this release

Download the files of this release into one directory and run these commands there. You need minisign, sha256sum, cosign 3 or newer and gh; you do not need to sign in to GitHub. Releases before 0.2.0 are not signed with cosign. The SBOM lists the dependencies declared in the lock files of the release commit (Rust crates and web interface packages), not the files in the image.

\`\`\`sh
EOF
  commands "$@"
  echo '```'
}

self_test() {
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  local key=RWR4L6XuZIijjGDS/pggnqvsOKeXTnAUspdDGzyF983eIpcnS+mKEREI
  local dry=(--image ghcr.io/lodestore/lodestore-dryrun --chart ghcr.io/lodestore/charts-dryrun/lodestore --ref refs/heads/feat/release-0.2.0)
  local variant flags ref rendered count

  for variant in default dry; do
    if [ "$variant" = dry ]; then
      flags=("${dry[@]}")
      ref=refs/heads/feat/release-0.2.0
    else
      flags=()
      ref=refs/tags/v0.1.0
    fi
    "$self" 0.1.0 ${flags[@]+"${flags[@]}"} >"$tmp/notes.md"
    "$self" --commands 0.1.0 ${flags[@]+"${flags[@]}"} >"$tmp/commands.sh"

    grep -q '^## Verify this release$' "$tmp/notes.md" || die "self-test failed: $variant notes have no verify section"
    changelog_body 0.1.0 >"$tmp/body.md"
    rendered=$(head -n "$(wc -l <"$tmp/body.md")" "$tmp/notes.md")
    [ "$rendered" = "$(cat "$tmp/body.md")" ] || die "self-test failed: $variant notes do not start with the changelog body"
    grep -q 'dependencies declared in the lock files of the release commit' "$tmp/notes.md" ||
      die "self-test failed: $variant notes do not describe the SBOM"

    awk '/^## Verify this release$/ { h = 1; next } h && /^```sh$/ && !on { on = 1; next } on && /^```$/ { exit } on { print }' "$tmp/notes.md" >"$tmp/fence.sh"
    cmp -s "$tmp/fence.sh" "$tmp/commands.sh" || die "self-test failed: $variant fence differs from --commands"

    grep -qF "$key" "$tmp/commands.sh" || die "self-test failed: $variant commands lack the project key"
    grep -qF 'lodestore release 0.1.0' "$tmp/commands.sh" || die "self-test failed: $variant commands lack the trusted comment"
    grep -qF "ghcr.io/lodestore/" "$tmp/commands.sh" || die "self-test failed: $variant commands lack the image"
    count=$(grep -cF "release.yml@$ref" "$tmp/commands.sh" || true)
    [ "$count" -ge 5 ] || die "self-test failed: $variant commands name the identity $count times"
    count=$(grep -cF -- "--source-ref $ref" "$tmp/commands.sh" || true)
    [ "$count" -eq 3 ] || die "self-test failed: $variant commands have --source-ref $count times"
    bash -n "$tmp/commands.sh" || die "self-test failed: $variant commands do not parse"
  done
  grep -qF 'ghcr.io/lodestore/lodestore:0.1.0' "$tmp/commands.sh" && die "self-test failed: dry-run commands name the release image"
  "$self" --commands 0.1.0 >"$tmp/commands.sh"
  grep -qF 'ghcr.io/lodestore/lodestore:0.1.0' "$tmp/commands.sh" || die "self-test failed: commands lack IMAGE:VERSION"
  grep -qF 'ghcr.io/lodestore/charts/lodestore:0.1.0' "$tmp/commands.sh" || die "self-test failed: commands lack CHART:VERSION"

  mkdir "$tmp/bin" "$tmp/empty" "$tmp/one"
  # shellcheck disable=SC2016
  printf '#!/bin/sh\nprintf "%%s\\n" "$STUB_COMMENT"\n' >"$tmp/bin/minisign"
  for stub in sha256sum gh cosign; do
    printf '#!/bin/sh\nexit 0\n' >"$tmp/bin/$stub"
  done
  chmod +x "$tmp/bin/"*
  : >"$tmp/one/lode-linux-x86_64"

  run_block() {
    (cd "$1" && STUB_COMMENT=$2 PATH="$tmp/bin:$PATH" bash -euo pipefail "$tmp/commands.sh") 2>"$tmp/stderr"
  }
  if run_block "$tmp/empty" 'lodestore release 0.1.0' >/dev/null; then
    die "self-test failed: the block passed in an empty directory"
  fi
  grep -q 'no release binaries' "$tmp/stderr" || die "self-test failed: the empty directory gave another error"
  run_block "$tmp/one" 'lodestore release 0.1.0' >/dev/null || die "self-test failed: the block failed with one binary"
  if run_block "$tmp/one" 'lodestore release 0.0.9' >/dev/null; then
    die "self-test failed: the block accepted the comment of another release"
  fi

  if "$self" --commands 0.1.0 --image 'evil;rm' 2>/dev/null; then die "self-test failed: accepted a bad image"; fi
  if "$self" --commands 0.1.0 --chart 'ghcr.io/other/x' 2>/dev/null; then die "self-test failed: accepted a bad chart"; fi
  if "$self" --commands 0.1.0 --ref refs/pull/1 2>/dev/null; then die "self-test failed: accepted a bad ref"; fi
  if "$self" --commands 0.1 2>/dev/null; then die "self-test failed: accepted a bad version"; fi
  echo "release-notes self-test ok"
}

mode=notes
case "${1:-}" in
  --self-test)
    self_test
    exit 0
    ;;
  --commands)
    mode=commands
    shift
    ;;
esac

[ "$#" -ge 1 ] || usage
version=$1
shift
[[ $version =~ $version_pattern ]] || die "bad version: $version"

image=ghcr.io/lodestore/lodestore
chart=ghcr.io/lodestore/charts/lodestore
ref=refs/tags/v$version
while [ "$#" -gt 0 ]; do
  [ "$#" -ge 2 ] || usage
  case $1 in
    --image) image=$2 ;;
    --chart) chart=$2 ;;
    --ref) ref=$2 ;;
    *) usage ;;
  esac
  shift 2
done
[[ $image =~ $image_pattern ]] || die "bad image: $image"
[[ $chart =~ $image_pattern ]] || die "bad chart: $chart"
[[ $ref =~ $ref_pattern ]] || die "bad ref: $ref"

if [ "$mode" = commands ]; then
  commands "$version" "$image" "$chart" "$ref"
else
  notes "$version" "$image" "$chart" "$ref"
fi
