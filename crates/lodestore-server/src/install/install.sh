#!/bin/sh
set -eu

SERVER_URL='@@SERVER_URL@@'
VERSION='@@VERSION@@'
RELEASES_URL=${LODESTORE_RELEASES_URL:-https://github.com/lodestore/lodestore/releases/download}
BIN=/usr/local/bin/lodestore-agent
UNIT=/etc/systemd/system/lodestore-agent.service
CONF_DIR=/etc/lodestore
STATE_DIR=/var/lib/lodestore
ENV_FILE=$CONF_DIR/agent.env
IDENTITY=$STATE_DIR/identity.json
tmp=
backup=
was_active=0

die() {
  echo "error: $*" >&2
  exit 1
}

cleanup() {
  [ -z "$tmp" ] || rm -rf "$tmp"
  if [ -n "$backup" ] && [ -d "$backup" ]; then
    restore_identity
    [ "$was_active" = 0 ] || systemctl restart lodestore-agent </dev/null || true
  fi
}

usage() {
  cat <<'EOF'
Usage: curl --proto '=https' --tlsv1.2 -fsSL <server>/install.sh | sh -s -- --join <token> [options]

  --join <token>   register this host with a one-time join token
  --insecure       allow downloading the agent over plain HTTP (a network attacker
                   can then replace the agent binary and its checksum)
  --force          forget the local identity and register this host again
  --from-github    download the agent from GitHub releases instead of the server
  --uninstall      stop and remove the agent service and binary
  --purge          with --uninstall, also remove /etc/lodestore and /var/lib/lodestore
  --help           show this help

HTTPS_PROXY, HTTP_PROXY and NO_PROXY set in the environment of the installer
(for example "| HTTPS_PROXY=http://proxy:3128 sh -s -- --join <token>") are
saved to /etc/lodestore/agent.env and passed to the service.
EOF
}

agent_id() {
  sed -n 's/.*"agent_id" *: *"\([^"]*\)".*/\1/p' "$IDENTITY" | head -n 1
}

service_active() {
  systemctl is-active --quiet lodestore-agent </dev/null
}

detect_arch() {
  case $(uname -m) in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "unsupported architecture $(uname -m); the agent is built for x86_64 and aarch64" ;;
  esac
}

fetch() {
  if [ "$insecure" = 1 ]; then
    curl -fsSL -o "$2" "$1" || die "could not download $1"
  else
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o "$2" "$1" || die "could not download $1"
  fi
}

require_https() {
  case $1 in
    https://*) ;;
    *)
      [ "$insecure" = 1 ] || die "refusing to download over plain HTTP from $1; serve Lodestore over HTTPS, or pass --insecure on a network you trust"
      echo "warning: downloading over plain HTTP from $1; the agent binary and its checksum can be tampered with on the way" >&2
      ;;
  esac
}

download_agent() {
  name=lodestore-agent-linux-$arch
  if [ "$github" = 1 ]; then
    base=$RELEASES_URL/v$VERSION
    fetch "$base/$name" "$tmp/agent"
    fetch "$base/SHA256SUMS" "$tmp/sums"
    expected=$(awk -v f="$name" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/sums")
    [ -n "$expected" ] || die "SHA256SUMS has no entry for $name"
  else
    fetch "$SERVER_URL/install/agent/$arch" "$tmp/agent"
    fetch "$SERVER_URL/install/agent/$arch.sha256" "$tmp/sum"
    read -r expected _ <"$tmp/sum" || true
    [ -n "$expected" ] || die "the server returned an empty checksum"
  fi
  actual=$(sha256sum "$tmp/agent" | cut -d ' ' -f 1)
  [ "$actual" = "$expected" ] || die "checksum mismatch for $name: expected $expected, got $actual; nothing was installed"
}

write_env_file() {
  found=0
  for v in HTTPS_PROXY HTTP_PROXY NO_PROXY https_proxy http_proxy no_proxy; do
    eval "val=\${$v:-}"
    [ -z "$val" ] || found=1
  done
  [ "$found" = 1 ] || return 0
  (
    umask 077
    : >"$ENV_FILE.new"
    for v in HTTPS_PROXY HTTP_PROXY NO_PROXY https_proxy http_proxy no_proxy; do
      eval "val=\${$v:-}"
      [ -z "$val" ] && continue
      esc=$(printf '%s' "$val" | sed 's/[\\"]/\\&/g')
      printf '%s="%s"\n' "$v" "$esc" >>"$ENV_FILE.new"
    done
  )
  chmod 0600 "$ENV_FILE.new"
  mv "$ENV_FILE.new" "$ENV_FILE"
}

write_unit() {
  cat >"$UNIT.new" <<'EOF'
[Unit]
Description=Lodestore backup agent
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=-/etc/lodestore/agent.env
ExecStart=/usr/local/bin/lodestore-agent run
Restart=always
RestartSec=5
RestartPreventExitStatus=77
NoNewPrivileges=yes
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes

[Install]
WantedBy=multi-user.target
EOF
  chmod 0644 "$UNIT.new"
  mv "$UNIT.new" "$UNIT"
}

STATE_FILES="identity.json agent.key agent.crt agent.key.new agent.crt.new ca.pem writer-keys s3-credentials.json repository.json"

stash_identity() {
  backup=$STATE_DIR/.old-$$
  mkdir -m 0700 "$backup"
  for f in $STATE_FILES; do
    [ ! -e "$STATE_DIR/$f" ] || mv "$STATE_DIR/$f" "$backup/$f"
  done
  [ ! -e "$CONF_DIR/agent.json" ] || mv "$CONF_DIR/agent.json" "$backup/agent.json"
}

restore_identity() {
  for f in $STATE_FILES; do
    rm -rf "${STATE_DIR:?}/$f"
    [ ! -e "$backup/$f" ] || mv "$backup/$f" "$STATE_DIR/$f"
  done
  [ ! -e "$backup/agent.json" ] || mv "$backup/agent.json" "$CONF_DIR/agent.json"
  rmdir "$backup"
}

uninstall() {
  systemctl disable --now lodestore-agent </dev/null >/dev/null 2>&1 || true
  rm -f "$UNIT" "$BIN"
  systemctl daemon-reload </dev/null || true
  if [ "$purge" = 1 ]; then
    rm -rf "$CONF_DIR" "$STATE_DIR"
  fi
  echo "lodestore-agent was removed"
}

main() {
  token=
  force=0
  purge=0
  github=0
  remove=0
  insecure=0
  while [ $# -gt 0 ]; do
    case $1 in
      --join)
        [ $# -ge 2 ] || die "--join needs a token"
        token=$2
        shift 2
        ;;
      --force) force=1; shift ;;
      --uninstall) remove=1; shift ;;
      --purge) purge=1; shift ;;
      --from-github) github=1; shift ;;
      --insecure) insecure=1; shift ;;
      --help | -h) usage; exit 0 ;;
      *) usage >&2; die "unknown option $1" ;;
    esac
  done

  if [ "$remove" = 0 ]; then
    if [ "$github" = 1 ]; then require_https "$RELEASES_URL"; else require_https "$SERVER_URL"; fi
  fi
  [ "$(uname -s)" = Linux ] || die "Linux is required"
  [ "$(id -u)" = 0 ] || die "run the installer as root"
  if [ "$remove" = 1 ]; then
    uninstall
    exit 0
  fi
  [ -d /run/systemd/system ] || die "systemd is required"
  detect_arch

  registered=0
  [ ! -f "$IDENTITY" ] || registered=1
  if [ "$registered" = 0 ] || [ "$force" = 1 ]; then
    [ -n "$token" ] || die "a join token is required: --join <token>"
  fi

  tmp=$(mktemp -d)
  trap cleanup EXIT
  trap 'exit 1' INT TERM HUP
  download_agent

  install -d -m 0700 "$CONF_DIR" "$STATE_DIR"
  install -m 0755 "$tmp/agent" "$BIN.new"
  mv "$BIN.new" "$BIN"
  write_env_file
  write_unit
  systemctl daemon-reload </dev/null

  if [ "$registered" = 1 ] && [ "$force" = 0 ]; then
    if service_active; then
      systemctl restart lodestore-agent </dev/null
    fi
    echo "already registered as $(agent_id); the agent binary was updated. Use --force to register this host again."
    exit 0
  fi

  if [ "$registered" = 1 ]; then
    ! service_active || was_active=1
    systemctl stop lodestore-agent </dev/null || true
    stash_identity
  fi

  if ! LODESTORE_JOIN_TOKEN="$token" "$BIN" enroll </dev/null; then
    [ -z "$backup" ] || die "registration failed; the previous registration is restored, fix the problem and run the installer again"
    die "registration failed; the agent binary is installed, fix the problem and run the installer again"
  fi
  [ -z "$backup" ] || rm -rf "$backup"
  backup=
  systemctl enable lodestore-agent </dev/null
  systemctl restart lodestore-agent </dev/null
  echo "lodestore-agent is running as $(agent_id)"
}

main "$@"
