#!/usr/bin/env bash
# Checks that a release tag, the workspace version, the Helm chart, the quickstart page and
# CHANGELOG.md agree, and prints "version=" and "prerelease=" lines for $GITHUB_OUTPUT.
# With --unpublished it fails if the tag already has a release or a version tag in GHCR.
# With --channels it prints "latest=" and "minor=" lines: whether this release may move
# :latest and :X.Y. Both modes need GH_REPO and GH_TOKEN, --unpublished also GITHUB_ACTOR.
#
#   scripts/check-version.sh TAG
#   scripts/check-version.sh --unpublished TAG
#   scripts/check-version.sh --channels TAG
#   scripts/check-version.sh --self-test
set -euo pipefail

tag_pattern='^v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$'

die() {
  echo "check-version: $*" >&2
  exit 1
}

tag_ok() {
  [[ $1 =~ $tag_pattern ]]
}

# GHCR answers 404 for a missing tag, a missing package and a token that cannot see the
# package, so only a tag list that comes back proves a tag is absent.
# shortcut: reads the first 1000 tags, follow the Link header past about 200 releases.
ghcr_tags() {
  local repo=$1 token
  token=$(printf 'user = "%s:%s"\n' "$GITHUB_ACTOR" "$GH_TOKEN" |
    curl -fsS -K - "https://ghcr.io/token?service=ghcr.io&scope=repository:$repo:pull" |
    jq -r .token) || return 1
  printf 'header = "Authorization: Bearer %s"\n' "$token" |
    curl -fsS -K - "https://ghcr.io/v2/$repo/tags/list?n=1000" | jq -r '.tags[]?'
}

check_unpublished() {
  local tag=$1 version=${1#v} releases repo tags
  releases=$(gh api --paginate "repos/$GH_REPO/releases" --jq '.[].tag_name') ||
    die "cannot list the releases"
  if grep -Fxq "$tag" <<<"$releases"; then
    die "release $tag already exists, as a draft or published"
  fi
  for repo in lodestore/lodestore lodestore/charts/lodestore; do
    if tags=$(ghcr_tags "$repo"); then
      if grep -Fxq "$version" <<<"$tags"; then
        die "ghcr.io/$repo:$version already exists"
      fi
    elif [ -n "$releases" ]; then
      # Before the first release the packages do not exist yet. A leftover draft counts as
      # a release here: delete it if nothing was ever published.
      die "cannot read the tags of ghcr.io/$repo"
    fi
  done
}

is_highest() {
  if [ "$(sort -V | tail -n 1)" = "$1" ]; then echo true; else echo false; fi
}

# Moving tags never go back: :latest follows the highest stable release, :X.Y the highest
# stable release of its line. A prerelease moves neither.
print_channels() {
  local version=${1#v} published stable
  case "$version" in
    *-*)
      printf 'latest=false\nminor=false\n'
      return
      ;;
  esac
  published=$(gh api --paginate "repos/$GH_REPO/releases" \
    --jq '.[] | select((.draft or .prerelease) | not) | .tag_name') ||
    die "cannot list the releases"
  stable=$(printf '%s\nv%s\n' "$published" "$version" | sed -n 's/^v//p')
  echo "latest=$(is_highest "$version" <<<"$stable")"
  echo "minor=$(awk -v line="${version%.*}." 'index($0, line) == 1' <<<"$stable" | is_highest "$version")"
}

self_test_channels() {
  local GH_REPO=o/r stub
  gh() {
    jq -r "$5" <<<"$stub"
  }
  stable() {
    jq -n '$ARGS.positional | map({tag_name: ., draft: false, prerelease: false})' --args "$@"
  }
  expect() {
    local got
    stub=$3
    got=$(print_channels "$1" | tr '\n' ' ')
    [ "$got" = "$2 " ] || die "self-test failed: $1 printed '$got', expected '$2'"
  }
  local unfinished='[{"tag_name":"v0.9.0","draft":true,"prerelease":false},
    {"tag_name":"v0.8.0-rc.1","draft":false,"prerelease":true},
    {"tag_name":"v0.1.9","draft":false,"prerelease":false}]'

  expect v0.1.0 'latest=true minor=true' '[]'
  expect v0.2.0 'latest=true minor=true' "$(stable v0.1.0)"
  expect v0.1.1 'latest=false minor=true' "$(stable v0.2.0 v0.1.0)"
  expect v0.1.1 'latest=false minor=false' "$(stable v0.1.5 v0.2.0)"
  expect v0.1.10 'latest=true minor=true' "$unfinished"
  expect v0.1.2 'latest=false minor=true' "$(stable v10.1.5 v0.1.1)"
  expect v0.1.1-rc.1 'latest=false minor=false' '[]'
  unset -f gh stable expect
}

self_test_unpublished() {
  local GH_REPO=o/r GH_TOKEN=t GITHUB_ACTOR=a
  local stub_releases stub_status stub_image stub_chart stub_denied
  gh() {
    jq -r "$5" <<<"$stub_releases"
    return "$stub_status"
  }
  curl() {
    cat >/dev/null
    case "${!#}" in
      *ghcr.io/token*) echo '{"token":"t"}' ;;
      *"$stub_denied"*) return 22 ;;
      */charts/*) echo "$stub_chart" ;;
      *) echo "$stub_image" ;;
    esac
  }
  given() {
    stub_releases=$1 stub_image=$2 stub_chart=$3 stub_denied=${4:-nothing} stub_status=${5:-0}
  }
  refused() {
    if (check_unpublished v0.1.0) 2>/dev/null; then
      die "self-test failed: published check accepted: $1"
    fi
  }
  accepted() {
    (check_unpublished v0.1.0) || die "self-test failed: published check refused: $1"
  }
  local none='{"tags":[]}' old='[{"tag_name":"v0.0.9","draft":false,"prerelease":false}]'

  given '[]' "$none" "$none" lodestore
  accepted "no release and no packages yet"
  given "$old" '{"tags":["0.0.9"]}' '{"tags":["0.0.9"]}'
  accepted "a new version"
  given '[{"tag_name":"v0.1.0","draft":false,"prerelease":false}]' "$none" "$none"
  refused "a published release"
  given '[{"tag_name":"v0.1.0","draft":true,"prerelease":false}]' "$none" "$none"
  refused "a draft release"
  given '[]' '{"tags":["0.1.0"]}' "$none"
  refused "an image tag"
  given '[]' "$none" '{"tags":["0.1.0"]}'
  refused "a chart tag"
  given "$old" "$none" "$none" charts
  refused "unreadable tags after a release"
  given '[]' "$none" "$none" nothing 1
  refused "unreadable releases"
  unset -f gh curl given refused accepted
}

if [ "${1:-}" = "--self-test" ]; then
  self_test_unpublished
  self_test_channels
  for bad in 'v1.2.3;id' '1.2.3' 'v1.2' 'v1.2.3-rc.1;id' 'v1.2.3-beta.1' 'v1.2.3 ' $'v1.2.3\nx' ''; do
    if tag_ok "$bad"; then
      die "self-test failed: accepted '$bad'"
    fi
  done
  for good in v0.1.0 v0.1.0-rc.1 v10.20.30; do
    tag_ok "$good" || die "self-test failed: rejected '$good'"
  done
  echo "check-version self-test ok"
  exit 0
fi

case "${1:-}" in
  --unpublished | --channels)
    [ "$#" -eq 2 ] || die "usage: $0 $1 TAG"
    tag_ok "$2" || die "tag does not look like vX.Y.Z or vX.Y.Z-rc.N"
    if [ "$1" = --unpublished ]; then check_unpublished "$2"; else print_channels "$2"; fi
    exit 0
    ;;
esac

[ "$#" -eq 1 ] || die "usage: $0 TAG | --unpublished TAG | --channels TAG | --self-test"
tag=$1
tag_ok "$tag" || die "tag does not look like vX.Y.Z or vX.Y.Z-rc.N"
version=${tag#v}

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

crate=$(cargo metadata --locked --no-deps --format-version 1 |
  jq -r '.packages[] | select(.name == "lodestore-server") | .version')
chart_version=$(sed -n 's/^version: *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' deploy/helm/lodestore/Chart.yaml)
chart_app=$(sed -n 's/^appVersion: *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' deploy/helm/lodestore/Chart.yaml)
page=$(sed -n 's/^export LODESTORE_VERSION=//p' docs/src/content/docs/docs/quickstart.md)

[ "$crate" = "$version" ] || die "lodestore-server is $crate, tag is $version"
[ "$chart_version" = "$version" ] || die "Chart.yaml version is '$chart_version', tag is $version"
[ "$chart_app" = "$version" ] || die "Chart.yaml appVersion is '$chart_app', tag is $version"
[ "$page" = "$version" ] || die "quickstart.md LODESTORE_VERSION is '$page', tag is $version"

awk -v heading="## [$version]" 'index($0, heading) == 1 { found = 1 } END { exit !found }' CHANGELOG.md ||
  die "CHANGELOG.md has no section '## [$version]'"

case "$version" in
  *-*) prerelease=true ;;
  *) prerelease=false ;;
esac
echo "version=$version"
echo "prerelease=$prerelease"
