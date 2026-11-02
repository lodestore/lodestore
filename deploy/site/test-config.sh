#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "site config failed: $*" >&2
  exit 1
}

name=lodestore-site-test
serve=0
if [ "${1:-}" = "--stop" ]; then
  docker rm -f "$name" >/dev/null 2>&1 || true
  exit 0
fi
if [ "${1:-}" = "--serve" ]; then
  serve=1
  shift
fi
[ $# -eq 1 ] || fail "usage: $0 [--serve] <dist> | $0 --stop"
[ -f "$1/index.html" ] || fail "$1 has no index.html, build the site first"

here=$(cd "$(dirname "$0")" && pwd)
dist=$(cd "$1" && pwd)
tmp=$(mktemp -d)
trap 'docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT

openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj '/CN=lodestore.dev' \
  -keyout "$tmp/privkey.pem" -out "$tmp/fullchain.pem" >/dev/null 2>&1 ||
  fail "could not create a test certificate"
chmod 644 "$tmp/privkey.pem" "$tmp/fullchain.pem"

docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" \
  -p 127.0.0.1:18081:80 -p 127.0.0.1:18443:443 \
  -v "$here/nginx.conf:/etc/nginx/conf.d/lodestore.dev.conf:ro" \
  -v "$here/lodestore-site-headers.conf:/etc/nginx/snippets/lodestore-site-headers.conf:ro" \
  -v "$tmp:/etc/letsencrypt/live/lodestore.dev:ro" \
  -v "$dist:/opt/lodestore.dev/public:ro" \
  nginx:1.24-alpine >/dev/null || fail "could not start the nginx container"

docker exec "$name" nginx -t >/dev/null 2>&1 || {
  docker exec "$name" nginx -t >&2 || true
  fail "nginx -t rejected the config"
}

curl_site() {
  curl -sk --resolve lodestore.dev:18443:127.0.0.1 --resolve www.lodestore.dev:18443:127.0.0.1 "$@"
}

ready=0
for _ in $(seq 1 30); do
  if curl_site -o /dev/null https://lodestore.dev:18443/; then
    ready=1
    break
  fi
  sleep 0.5
done
[ "$ready" = 1 ] || fail "nginx did not answer on 18443"

status() { curl_site -o /dev/null -w '%{http_code}' "$@"; }

has_header() {
  grep -qi "^$2:" <<<"$1" || fail "$3: missing header $2"
}

home=$(curl_site -D - -o /dev/null https://lodestore.dev:18443/)
[ "$(status https://lodestore.dev:18443/)" = 200 ] || fail "/ is not 200"
for h in Content-Security-Policy X-Content-Type-Options Referrer-Policy Permissions-Policy Strict-Transport-Security; do
  has_header "$home" "$h" "/"
done

css=$(cd "$dist" && find _astro -name '*.css' | sort | head -n 1)
[ -n "$css" ] || fail "no _astro/*.css in $dist"
asset=$(curl_site -D - -o /dev/null "https://lodestore.dev:18443/$css")
grep -qi '^cache-control:.*immutable' <<<"$asset" || fail "$css is not immutable"
has_header "$asset" Content-Security-Policy "$css"

zipped=$(curl_site -H 'Accept-Encoding: gzip' -D - -o /dev/null "https://lodestore.dev:18443/$css")
grep -qi '^content-encoding: gzip' <<<"$zipped" || fail "$css is not gzip-compressed"

slash=$(curl_site -D - -o /dev/null https://lodestore.dev:18443/docs/quickstart)
grep -q ' 301' <<<"$slash" || fail "/docs/quickstart is not a 301"
grep -qiE '^location: .*/docs/quickstart/\r?$' <<<"$slash" || fail "/docs/quickstart does not point to /docs/quickstart/"

[ "$(status https://lodestore.dev:18443/no-such-page/)" = 404 ] || fail "a missing page is not 404"
curl_site https://lodestore.dev:18443/no-such-page/ | cmp -s - "$dist/404.html" || fail "404 body is not 404.html"

www=$(curl_site -D - -o /dev/null https://www.lodestore.dev:18443/)
grep -q ' 301' <<<"$www" || fail "www is not a 301"
grep -qiE '^location: https://lodestore.dev/\r?$' <<<"$www" || fail "www does not redirect to https://lodestore.dev/"

plain=$(curl -s -H 'Host: lodestore.dev' -D - -o /dev/null http://127.0.0.1:18081/)
grep -q ' 301' <<<"$plain" || fail "http is not a 301"
grep -qiE '^location: https://lodestore.dev/\r?$' <<<"$plain" || fail "http does not redirect to https"

echo "site config ok"

if [ "$serve" = 1 ]; then
  trap 'rm -rf "$tmp"' EXIT
  trap 'docker rm -f "$name" >/dev/null 2>&1 || true; exit 0' INT TERM
  echo "serving https://lodestore.dev:18443/"
  while docker ps -q -f "name=^${name}$" | grep -q .; do sleep 1; done
fi
