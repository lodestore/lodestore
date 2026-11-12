#!/bin/sh
set -euf

SERVER_URL='@@SERVER_URL@@'
VERSION='@@VERSION@@'
PROJECT_KEY='@@PROJECT_KEY@@'
RELEASES_URL=${LODESTORE_RELEASES_URL:-https://github.com/lodestore/lodestore/releases/download}
BIN=/usr/local/bin/lodestore-agent
UNIT=/etc/systemd/system/lodestore-agent.service
CONF_DIR=/etc/lodestore
STATE_DIR=/var/lib/lodestore
ENV_FILE=$CONF_DIR/agent.env
AGENT_YAML=$CONF_DIR/agent.yaml
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
  --insecure       allow downloading over plain HTTP;
                   anyone on the network can then get root on this host
  --force          forget the local identity and register this host again; the previous
                   agent id is kept when the server does not know it
  --from-github    download the agent from GitHub releases instead of the server; needs
                   minisign to check the release signature
  --trust-key <file>
                   trust this public key, both for checking the agent in this install and
                   for updates, for your own builds; the file name must end in .pub and be
                   different for each key, repeat the option for more keys
  --allow-in-place-restore
                   enable restores in place for this host: writes restore.allow_in_place: true
                   to /etc/lodestore/agent.yaml
  --uninstall      stop and remove the agent service and binary
  --purge          with --uninstall, also remove /etc/lodestore and /var/lib/lodestore
  --help           show this help

When minisign is installed, the installer checks the signature of the agent against the
Lodestore project key (key id 8CA38864EEA52F78) and any --trust-key, and refuses to
install an agent without a valid signature.

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

have_minisign() {
  command -v minisign >/dev/null 2>&1
}

signed_comment() {
  log=$tmp/minisign.log
  : >"$log"
  printf '# checked with the project key:\n' >>"$log"
  if comment=$(minisign -V -Q -P "$PROJECT_KEY" -m "$1" -x "$2" 2>>"$log"); then
    return 0
  fi
  old_ifs=$IFS
  IFS=$nl
  for key_file in $trust_keys; do
    printf '# checked with --trust-key %s:\n' "$key_file" >>"$log"
    if comment=$(minisign -V -Q -p "$key_file" -m "$1" -x "$2" 2>>"$log"); then
      IFS=$old_ifs
      return 0
    fi
  done
  IFS=$old_ifs
  while IFS= read -r line; do
    case $line in
      '# '*) printf '%s\n' "${line#'# '}" >&2 ;;
      *) printf 'minisign: %s\n' "$line" >&2 ;;
    esac
  done <<EOF
$(tr -d '\000-\010\013-\037\177' <"$log")
EOF
  return 1
}

agent_comment_ok() {
  printf '%s\n' "$comment" | awk -v a="$arch" 'NR == 1 && NF == 3 && $1 == "lodestore-agent" && $3 == a { ok = 1 } END { exit (ok ? 0 : 1) }'
}

fetch_signature() {
  rc=0
  if [ "$insecure" = 1 ]; then
    code=$(curl -fsSL -w '%{http_code}' -o "$2" "$1") || rc=$?
  else
    code=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -w '%{http_code}' -o "$2" "$1") || rc=$?
  fi
  [ "$rc" != 0 ] || return 0
  [ "$code" = 404 ] && die "$3; nothing was installed"
  status=
  [ "$code" = 000 ] || status=", HTTP $code"
  die "could not download $1 (curl exit $rc$status); nothing was installed"
}

download_agent() {
  name=lodestore-agent-linux-$arch
  if [ "$github" = 1 ]; then
    have_minisign || die "--from-github checks the release signature and needs minisign; install it (apt install minisign, or dnf install minisign from EPEL) or take it from https://github.com/jedisct1/minisign/releases, then run the installer again; nothing was installed"
    base=$RELEASES_URL/v$VERSION
    fetch "$base/$name" "$tmp/agent"
    fetch "$base/SHA256SUMS" "$tmp/sums"
    fetch_signature "$base/SHA256SUMS.minisig" "$tmp/sums.minisig" "the release v$VERSION has no SHA256SUMS.minisig"
    signed_comment "$tmp/sums" "$tmp/sums.minisig" || die "SHA256SUMS of v$VERSION does not verify with the project key or any --trust-key; nothing was installed"
    [ "$comment" = "lodestore release $VERSION" ] || die "SHA256SUMS is signed for \"$comment\", not lodestore release $VERSION; nothing was installed"
    expected=$(awk -v f="$name" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/sums")
    [ -n "$expected" ] || die "SHA256SUMS has no entry for $name; nothing was installed"
  else
    fetch "$SERVER_URL/install/agent/$arch" "$tmp/agent"
    fetch "$SERVER_URL/install/agent/$arch.sha256" "$tmp/sum"
    read -r expected _ <"$tmp/sum" || true
    [ -n "$expected" ] || die "the server returned an empty checksum"
  fi
  actual=$(sha256sum "$tmp/agent" | cut -d ' ' -f 1)
  [ "$actual" = "$expected" ] || die "checksum mismatch for $name: expected $expected, got $actual; nothing was installed"
  [ "$github" = 0 ] || return 0
  if have_minisign; then
    fetch_signature "$SERVER_URL/install/agent/$arch.minisig" "$tmp/agent.minisig" "the server has no signature for $name; minisign is installed, so the installer needs one"
    signed_comment "$tmp/agent" "$tmp/agent.minisig" || die "the signature of $name does not verify with the project key or any --trust-key; nothing was installed"
    agent_comment_ok || die "the signature of $name is for \"$comment\", not lodestore-agent for $arch; nothing was installed"
  else
    echo "note: minisign is not installed, so the agent was checked only against the checksum from the server" >&2
  fi
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

in_place_enabled() {
  grep -q '^ *allow_in_place: *true' "$AGENT_YAML"
}

write_agent_yaml() {
  [ "$allow_in_place" = 1 ] && [ ! -e "$AGENT_YAML" ] || return 0
  (
    umask 077
    printf 'restore:\n  allow_in_place: true\n' >"$AGENT_YAML.new"
  )
  chmod 0600 "$AGENT_YAML.new"
  chown root:root "$AGENT_YAML.new"
  mv "$AGENT_YAML.new" "$AGENT_YAML"
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
RuntimeDirectory=lodestore
RuntimeDirectoryMode=0755
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
LockPersonality=yes
RestrictRealtime=yes
ProtectClock=yes
ProtectHostname=yes
ProtectKernelLogs=yes
SystemCallArchitectures=native

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

snapshot_trust_keys() {
  [ -n "$trust_keys" ] || return 0
  snapshot=
  n=0
  old_ifs=$IFS
  IFS=$nl
  for file in $trust_keys; do
    n=$((n + 1))
    mkdir -p "$tmp/trusted-keys/$n"
    copy=$tmp/trusted-keys/$n/$(basename -- "$file")
    cp -- "$file" "$copy" || die "could not read the --trust-key file $file"
    snapshot=$snapshot$copy$nl
  done
  IFS=$old_ifs
  trust_keys=$snapshot
}

install_trust_keys() {
  [ -n "$trust_keys" ] || return 0
  install -d -m 0755 -o root -g root "$CONF_DIR/trusted-keys"
  old_ifs=$IFS
  IFS=$nl
  for file in $trust_keys; do
    install -m 0644 -o root -g root -- "$file" "$CONF_DIR/trusted-keys/$(basename -- "$file")"
  done
  IFS=$old_ifs
}

clear_pending_update() {
  systemctl stop 'lodestore-agent-update-*.timer' 'lodestore-agent-update-*.service' </dev/null >/dev/null 2>&1 || true
  rm -f "$STATE_DIR/update/pending.json" "$STATE_DIR/update/pending.rollback" "$BIN.prev"
}

uninstall() {
  systemctl disable --now lodestore-agent </dev/null >/dev/null 2>&1 || true
  rm -f "$UNIT" "$BIN" "$BIN.prev"
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
  allow_in_place=0
  trust_keys=
  trust_names=
  nl='
'
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
      --allow-in-place-restore) allow_in_place=1; shift ;;
      --trust-key)
        [ $# -ge 2 ] || die "--trust-key needs a readable .pub file"
        case $2 in
          *.pub) [ -f "$2" ] && [ -r "$2" ] || die "--trust-key needs a readable .pub file: $2" ;;
          *) die "--trust-key needs a readable .pub file: $2" ;;
        esac
        base=$(basename -- "$2")
        case $nl$trust_names in *"$nl$base$nl"*) die "two --trust-key files are named $base; rename one" ;; esac
        trust_names=$trust_names$base$nl
        trust_keys=$trust_keys$2$nl
        shift 2
        ;;
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
  if [ "$allow_in_place" = 1 ] && [ -e "$AGENT_YAML" ] && ! in_place_enabled; then
    die "$AGENT_YAML already exists; add \"restore:\" with \"allow_in_place: true\" to it by hand"
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
  snapshot_trust_keys
  download_agent

  install -d -m 0700 "$CONF_DIR" "$STATE_DIR"
  getent group lodestore >/dev/null 2>&1 || groupadd --system lodestore
  [ -d "$CONF_DIR/hooks.d" ] || install -d -m 0700 -o root -g root "$CONF_DIR/hooks.d"
  install -m 0755 "$tmp/agent" "$BIN.new"
  clear_pending_update
  mv "$BIN.new" "$BIN"
  install_trust_keys
  write_agent_yaml
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

  prev=
  if [ "$registered" = 1 ]; then
    prev=$(agent_id)
    ! service_active || was_active=1
    systemctl stop lodestore-agent </dev/null || true
    stash_identity
  fi

  set -- enroll
  [ -z "$prev" ] || set -- enroll --previous-agent-id "$prev"
  if ! LODESTORE_JOIN_TOKEN="$token" "$BIN" "$@" </dev/null; then
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
