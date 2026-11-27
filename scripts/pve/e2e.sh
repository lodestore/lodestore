#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# End-to-end test of Proxmox VE container backups on a one-node nested lab.
#
#   scripts/pve/e2e.sh
#
# Builds the server, the agent and lode, boots the lab with nested-pve.sh while
# they build, runs MinIO and the server on this machine and the agent on the
# node, and checks, in order:
#   - `lodestore-agent pve install` on a standalone node;
#   - a backup of a running container (snapshot mode) and of a stopped one;
#   - the tree of the stopped backup against a native `vzdump --dumpdir` archive
#     of the same container with the same vzdump.conf (paths, types, modes, owners);
#   - a whole restore as a new container: stopped, onboot 0, unprivileged, a new
#     MAC, the planted lxc.* key dropped and reported, owners, ACLs and
#     capabilities equal to the original, and a mount point that named the
#     volume of another container left that volume unchanged (sha256);
#   - a file restore into the node's restore folder, owners shifted by the id map;
#   - a manual `vzdump --storage lodestore` without a job token is refused;
#   - the configuration filter against the parser of Proxmox VE on a corpus of
#     hostile configurations (deploy/proxmox/t/check-filtered-config.pl);
#   - the recovery from the bucket alone: server and agent stopped, `lode
#     restore`, tar and `pct restore`, owners and ACLs equal to the original.
# The first failed check stops the run with a message; the lab is always torn down.
#
# Needs what nested-pve.sh needs, plus cargo, cargo-nextest, curl, jq and sha256sum.
# Logs go to E2E_LOG_DIR (default target/pve-e2e).
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
lab=$root/scripts/pve/nested-pve.sh
bin=$root/target/release
corpus=$root/target/guest-config-corpus.jsonl
logs=${E2E_LOG_DIR:-$root/target/pve-e2e}
work=$(mktemp -d)
bucket=pve-e2e
repo_name=pve-e2e
repo_password=pve-e2e-repository-password
admin_password=pve-e2e-admin-password
host=$(hostname -I | awk '{print $1}')
base=http://$host:8080
jar=$work/cookies
csrf=
minio_pid=
server_pid=
lab_pid=
host_id=
ct=101
victim=102
restored=201
recovered=301
storage=local-lvm
files=(/srv/e2e/owned.txt /srv/e2e/acl.txt /srv/e2e/capbin /srv/e2e/suid /data /data/f)
mkdir -p "$logs"

cleanup() {
  local status=$?
  set +e
  if [ "$status" -ne 0 ]; then
    tail -n 60 "$logs/server.log" >&2 2>/dev/null
    "$lab" ssh 1 journalctl -u lodestore-agent --no-pager -n 120 >"$logs/agent.log" 2>&1
    tail -n 60 "$logs/agent.log" >&2
    cp "${PVE_WORKDIR:-$HOME/.cache/lodestore-nested-pve}/node1/serial.log" "$logs/" 2>/dev/null
  fi
  [ -z "$lab_pid" ] || kill "$lab_pid" 2>/dev/null
  [ -z "$server_pid" ] || kill "$server_pid" 2>/dev/null
  [ -z "$minio_pid" ] || kill "$minio_pid" 2>/dev/null
  "$lab" down
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

fail() {
  echo "pve e2e failed: $*" >&2
  exit 1
}

wait_for() {
  local what=$1
  shift
  for _ in $(seq "${WAIT:-60}"); do
    if "$@" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  fail "timed out waiting for $what"
}

random_hex() {
  head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'
}

b64url() {
  printf '%s' "$1" | base64 -w0 | tr '+/' '-_' | tr -d '='
}

api() {
  local method=$1 path=$2 body=${3:-}
  local args=(-sS --fail-with-body -X "$method" -b "$jar" -c "$jar" -H "x-csrf-token: $csrf")
  [ -z "$body" ] || args+=(-H 'content-type: application/json' --data "$body")
  curl "${args[@]}" "$base$path"
}

# Runs a fixed command on the node.
node() {
  "$lab" ssh 1 "$(printf '%q ' "$@")"
}

# Runs one of the node_* functions below on the node, shipped by `declare -f`.
on_node() {
  local fn=$1
  shift
  { declare -f "$fn"; printf '%s' "$fn"; printf ' %q' "$@"; echo; } | "$lab" ssh 1 bash -s
}

push() {
  "$lab" ssh 1 "cat >$(printf %q "$2") && chmod $3 $(printf %q "$2")" <"$1"
}

step() {
  local started=$SECONDS
  echo "== $1 (at ${SECONDS}s)"
  "$@"
  echo "== $1 done in $((SECONDS - started))s"
}

build() {
  "$lab" up --nodes 1 >"$logs/lab.log" 2>&1 &
  lab_pid=$!
  cargo build --release --locked -p lodestore-server -p lodestore-agent -p lode-cli
  rm -f "$corpus"
  LODE_GUEST_CONFIG_CORPUS=$corpus cargo nextest run --cargo-profile release -p lodestore-format \
    --run-ignored only -E 'test(guest_config_corpus_dump)'
  [ -s "$corpus" ] || fail "guest_config_corpus_dump wrote no corpus to $corpus"
  local status=0
  wait "$lab_pid" || status=$?
  lab_pid=
  tail -n 5 "$logs/lab.log"
  [ "$status" = 0 ] || { tail -n 80 "$logs/lab.log" >&2; fail "the lab did not come up"; }
}

fetch_pinned() {
  local file=$1 url=$2 sha=$3 dir=$root/target/test-bin
  mkdir -p "$dir"
  [ -f "$dir/$file" ] || { curl -fsSL -o "$dir/$file.tmp" "$url" && mv "$dir/$file.tmp" "$dir/$file"; }
  echo "$sha  $dir/$file" | sha256sum --check --status || { rm -f "$dir/$file"; fail "sha256 mismatch for $file"; }
  chmod 0755 "$dir/$file"
}

start_storage() {
  # shellcheck source=/dev/null
  . "$root/scripts/pins.env"
  local minio=minio.linux-amd64.$MINIO_RELEASE mc=mc.linux-amd64.$MC_RELEASE
  fetch_pinned "$minio" "https://github.com/minio/minio/releases/download/$MINIO_RELEASE/$minio" "$MINIO_SHA256_LINUX_AMD64"
  fetch_pinned "$mc" "https://github.com/minio/mc/releases/download/$MC_RELEASE/$mc" "$MC_SHA256_LINUX_AMD64"
  minio_user=$(random_hex 8)
  minio_password=$(random_hex 16)
  mkdir -p "$work/minio" "$work/mc"
  MINIO_ROOT_USER=$minio_user MINIO_ROOT_PASSWORD=$minio_password \
    "$root/target/test-bin/$minio" server "$work/minio" --address 0.0.0.0:9000 \
    --console-address 127.0.0.1:9001 >"$logs/minio.log" 2>&1 &
  minio_pid=$!
  wait_for minio curl -fsS http://127.0.0.1:9000/minio/health/ready
  mc_bin=$work/mc-run
  printf '#!/bin/sh\nexec %s --config-dir %s --no-color "$@"\n' "$root/target/test-bin/$mc" "$work/mc" >"$mc_bin"
  chmod 0755 "$mc_bin"
  wait_for "mc alias" "$mc_bin" alias set e2e http://127.0.0.1:9000 "$minio_user" "$minio_password"
  "$mc_bin" mb e2e/$bucket >/dev/null
  "$mc_bin" version enable e2e/$bucket >/dev/null
}

start_server() {
  mkdir -p "$work/agents" "$work/server"
  install -m 0755 "$bin/lodestore-agent" "$work/agents/lodestore-agent-linux-x86_64"
  LODESTORE_DATA_DIR=$work/server LODESTORE_PUBLIC_URL=$base \
    LODESTORE_GRPC_PUBLIC_ADDRESS=$host:8443 LODESTORE_AGENT_DIR=$work/agents \
    LODESTORE_ADMIN_USERNAME=admin LODESTORE_ADMIN_PASSWORD=$admin_password \
    "$bin/lodestore-server" >"$logs/server.log" 2>&1 &
  server_pid=$!
  wait_for /healthz curl -fsS "$base/healthz"
  api POST /api/v1/auth/login "$(jq -n --arg p "$admin_password" '{username: "admin", password: $p}')" >/dev/null
  csrf=$(api GET /api/v1/auth/session | jq -r .csrf_token)
  [ -n "$csrf" ] && [ "$csrf" != null ] || fail "no csrf token"

  local access secret commands draft started wizard key
  access=$(random_hex 8)
  secret=$(random_hex 16)
  commands=$(api GET "/api/v1/agent-policy?bucket=$bucket" | jq -r .minio_commands)
  commands=$(printf '%s\n' "$commands" | sed -e "s/ALIAS/e2e/g" -e "s/AGENT_ACCESS_KEY/$access/g" \
    -e "s/AGENT_SECRET_KEY/$secret/g" -e "s|^mc |$mc_bin |")
  (cd "$work" && bash -c "$commands") >/dev/null
  draft=$(jq -n --arg name "$repo_name" --arg bucket "$bucket" --arg endpoint "http://$host:9000" \
    --arg su "$minio_user" --arg sp "$minio_password" --arg au "$access" --arg ap "$secret" \
    '{name: $name,
      location: {backend: "s3", options: {bucket: $bucket, endpoint: $endpoint, path_style: "true"}},
      server_credentials: {access_key_id: $su, secret_access_key: $sp},
      agent_credentials: {access_key_id: $au, secret_access_key: $ap}}')
  started=$(api POST /api/v1/repositories/wizard "$draft")
  wizard=$(jq -r .wizard_id <<<"$started")
  key=$(jq -r .recovery_key <<<"$started")
  repo_id=$(api POST "/api/v1/repositories/wizard/$wizard/commit" \
    "$(jq -n --arg p "$repo_password" --arg k "${key##*-}" '{password: $p, recovery_key_last_group: $k}')" | jq -r .id)
  [ -n "$repo_id" ] && [ "$repo_id" != null ] || fail "the wizard did not return a repository id"
}

host_online() {
  api GET /api/v1/hosts | jq -e '[.[] | select(.status == "online")] | length > 0'
}

node_install_agent() {
  set -euo pipefail
  curl -fsSL "$1/install.sh" | sh -s -- --insecure --join "$2"
  lodestore-agent pve install --wait-seconds 300
  pvesm status | grep -q '^lodestore '
  test -f /etc/pve/lodestore/plugin/pve1
}

install_agent() {
  local token
  token=$(api POST /api/v1/join-tokens "$(jq -n --arg r "$repo_id" \
    '{repository_id: $r, labels: [{key: "agent", value: "pve-e2e"}]}')" | jq -r .token)
  on_node node_install_agent "$base" "$token"
  wait_for "the node to be online" host_online
  host_id=$(api GET /api/v1/hosts | jq -r '[.[] | select(.status == "online")][0].id')
}

node_setup_ct() {
  set -euo pipefail
  local ct=$1 victim=$2 template b
  template=$(pveam list local | awk '/debian-13-standard_.*_amd64\./ {print $1}' | sort -V | tail -n 1)
  ip link show vmbr0 >/dev/null 2>&1 || { ip link add vmbr0 type bridge; ip link set vmbr0 up; }
  printf 'exclude-path: /var/cache/?* *.tmp\n' >>/etc/vzdump.conf
  pct create "$ct" "$template" --hostname e2e --unprivileged 1 --rootfs local-lvm:2 \
    --mp0 local-lvm:1,mp=/data,backup=1 --net0 name=eth0,bridge=vmbr0,ip=192.168.77.101/24 --memory 256
  pct create "$victim" "$template" --hostname victim --unprivileged 1 --rootfs local-lvm:1 --memory 256
  echo 'lxc.apparmor.profile: unconfined' >>"/etc/pve/lxc/$ct.conf"
  pct start "$ct"
  for b in setfacl getfacl setcap getcap; do
    pct push "$ct" "$(command -v "$b")" "/usr/local/bin/$b" --perms 0755
  done
  pct exec "$ct" -- sh -ec '
    groupadd -g 1001 bob
    useradd -u 1000 alice
    mkdir -p /srv/e2e /var/cache/e2e
    echo owned >/srv/e2e/owned.txt
    chown 1000:1001 /srv/e2e/owned.txt
    chmod 0640 /srv/e2e/owned.txt
    echo acl >/srv/e2e/acl.txt
    setfacl -m u:1000:r,g:1001:rw /srv/e2e/acl.txt
    cp /bin/true /srv/e2e/capbin
    setcap cap_net_raw+ep /srv/e2e/capbin
    cp /bin/true /srv/e2e/suid
    chmod 4755 /srv/e2e/suid
    ln -s /etc/hostname /srv/e2e/link
    echo excluded >/var/cache/e2e/excluded
    echo excluded >/srv/e2e/scratch.tmp
    echo data >/data/f
    chown 1000:1000 /data/f
    setfacl -d -m u:1000:rwx /data
  '
}

setup_ct() {
  on_node node_setup_ct "$ct" "$victim"
}

job_done() {
  local job state
  job=$(api GET "/api/v1/jobs/$1")
  state=$(jq -r .state <<<"$job")
  case "$state" in
    succeeded | warnings) return 0 ;;
    failed | cancelled | interrupted)
      jq . <<<"$job" >&2
      fail "job $1 ended as $state"
      ;;
    *) return 1 ;;
  esac
}

wait_job() {
  local job=$1 state
  for _ in $(seq 450); do
    state=$(api GET "/api/v1/jobs/$job" | jq -r .state)
    case "$state" in
      succeeded | warnings) return 0 ;;
      failed | cancelled | interrupted) job_done "$job" ;;
    esac
    sleep 2
  done
  fail "job $job did not end within 15 minutes"
}

create_policy() {
  local yaml
  yaml=$(printf 'apiVersion: lodestore.io/v1alpha2\nkind: BackupPolicy\nmetadata:\n  name: pve-e2e\nspec:\n  repository: %s\n  selector:\n    matchLabels:\n      agent: pve-e2e\n  source:\n    type: proxmox-ct\n    vmids: [%s]\n' "$repo_name" "$ct")
  api POST /api/v1/policies "$(jq -n --arg y "$yaml" '{yaml: $y}')" >/dev/null
}

# Runs the policy and prints the snapshot id. The agent restarts after the job
# is queued, so its first scan lists the container for the job at once.
run_backup() {
  local job snapshot
  job=$(api POST /api/v1/policies/pve-e2e/run '{}' | jq -r '.jobs[0].id')
  node systemctl restart lodestore-agent
  wait_job "$job" >&2
  snapshot=$(api GET "/api/v1/jobs/$job" | jq -r .snapshot_id)
  [ -n "$snapshot" ] && [ "$snapshot" != null ] || fail "backup job $job has no snapshot"
  printf '%s' "$snapshot"
}

backup_running() {
  create_policy
  snap_running=$(run_backup)
  echo "snapshot of the running container: $snap_running"
}

node_plant_victim() {
  set -euo pipefail
  local ct=$1 victim=$2 size
  pct stop "$ct"
  size=$(pct config "$victim" | sed -n 's/^rootfs: .*size=\([^,]*\).*/\1/p')
  echo "mp1: local-lvm:vm-$victim-disk-0,mp=/victim,backup=0,size=$size" >>"/etc/pve/lxc/$ct.conf"
}

node_victim_sum() {
  set -euo pipefail
  lvchange -ay -K "pve/vm-$1-disk-0" >/dev/null
  sha256sum "/dev/pve/vm-$1-disk-0" | cut -d ' ' -f 1
}

# `tar -tv` of a native vzdump archive as "kind mode uid gid path" lines, the
# two files vzdump adds left out.
node_native_list() {
  set -euo pipefail
  local ct=$1 tar
  rm -rf /var/tmp/native
  mkdir -p /var/tmp/native
  vzdump "$ct" --dumpdir /var/tmp/native --mode snapshot --compress 0 --remove 0 >/var/tmp/native.log 2>&1 ||
    { cat /var/tmp/native.log >&2; exit 1; }
  tar=$(ls /var/tmp/native/*.tar)
  tar --numeric-owner --quoting-style=literal -tvf "$tar" | awk '
    function perm(p,  m, c) {
      m = 0
      if (substr(p, 2, 1) == "r") m += 256
      if (substr(p, 3, 1) == "w") m += 128
      c = substr(p, 4, 1); if (c == "x" || c == "s") m += 64; if (c == "s" || c == "S") m += 2048
      if (substr(p, 5, 1) == "r") m += 32
      if (substr(p, 6, 1) == "w") m += 16
      c = substr(p, 7, 1); if (c == "x" || c == "s") m += 8; if (c == "s" || c == "S") m += 1024
      if (substr(p, 8, 1) == "r") m += 4
      if (substr(p, 9, 1) == "w") m += 2
      c = substr(p, 10, 1); if (c == "x" || c == "t") m += 1; if (c == "t" || c == "T") m += 512
      return m
    }
    {
      t = substr($1, 1, 1)
      kind = (t == "d") ? "dir" : (t == "-" || t == "h") ? "file" : (t == "l") ? "symlink" : "other"
      split($2, owner, "/")
      path = $0
      sub(/^[^ ]+ +[^ ]+ +[^ ]+ +[^ ]+ +[^ ]+ /, "", path)
      if (t == "l") sub(/ -> .*$/, "", path)
      if (t == "h") sub(/ link to .*$/, "", path)
      sub(/^\.\//, "", path)
      sub(/\/$/, "", path)
      if (path == "" || path == "etc/vzdump/pct.conf" || path == "etc/vzdump/pct.fw") next
      print kind, perm($1), owner[1], owner[2], path
    }'
}

# The tree below /fs of a snapshot through the browser API, in the same form.
tree_list() {
  local snapshot=$1 dir=$2 rel=$3 offset=0 page total kind mode uid gid name
  while :; do
    page=$(api GET "/api/v1/snapshots/$snapshot/entries?path=$(b64url "$dir")&offset=$offset")
    while IFS=$'\t' read -r kind mode uid gid name; do
      echo "$kind $mode $uid $gid $rel$name"
      [ "$kind" != dir ] || tree_list "$snapshot" "$dir/$name" "$rel$name/"
    done < <(jq -r '.entries[] | [.kind, (.mode % 4096), .uid, .gid, .name] | @tsv' <<<"$page")
    total=$(jq -r .total <<<"$page")
    offset=$((offset + 200))
    [ "$offset" -lt "$total" ] || return 0
  done
}

backup_stopped_and_native() {
  on_node node_plant_victim "$ct" "$victim"
  snap_stopped=$(run_backup)
  echo "snapshot of the stopped container: $snap_stopped"
  on_node node_native_list "$ct" | LC_ALL=C sort >"$logs/native.list"
  node sed -i '/^mp1: /d' "/etc/pve/lxc/$ct.conf"
  victim_sum=$(on_node node_victim_sum "$victim")
  echo "victim volume sha256 before the restore: $victim_sum"
}

compare_native() {
  tree_list "$snap_stopped" /fs "" | LC_ALL=C sort >"$logs/snapshot.list"
  echo "native archive: $(wc -l <"$logs/native.list") paths, snapshot: $(wc -l <"$logs/snapshot.list") paths"
  diff -u "$logs/native.list" "$logs/snapshot.list" ||
    fail "the snapshot tree differs from the native vzdump archive (lines: - native, + snapshot)"
  grep -qx 'file 420 1000 1000 data/f' "$logs/snapshot.list" || fail "the mount point mp0 (backup=1) is not in the snapshot"
  ! grep -Eq ' (var/cache/e2e|srv/e2e/scratch\.tmp|victim)(/|$)' "$logs/snapshot.list" ||
    fail "an excluded path or the backup=0 mount point is in the snapshot"
}

node_ct_facts() {
  set -euo pipefail
  local ct=$1
  shift
  pct exec "$ct" -- stat -c '%u %g %a %n' "$@"
  pct exec "$ct" -- getfacl -n -p "$@" 2>/dev/null
  pct exec "$ct" -- getcap /srv/e2e/capbin
}

# Starts both containers and compares owners, modes, ACLs and capabilities.
compare_ct() {
  local copy=$1 original copied
  node pct start "$ct" 2>/dev/null || true
  node pct start "$copy"
  original=$(on_node node_ct_facts "$ct" "${files[@]}")
  copied=$(on_node node_ct_facts "$copy" "${files[@]}")
  printf '%s\n' "$original"
  [ "$original" = "$copied" ] || {
    diff <(printf '%s\n' "$original") <(printf '%s\n' "$copied") >&2
    fail "owners, modes, ACLs or capabilities in CT $copy differ from CT $ct"
  }
}

restore_whole() {
  local job result config hw_old hw_new
  job=$(api POST /api/v1/restores/guest "$(jq -n --arg s "$snap_stopped" --arg h "$host_id" \
    --arg st "$storage" --argjson v "$restored" \
    '{snapshot_id: $s, target_host_id: $h, storage: $st, vmid: $v}')" | jq -r .job_id)
  wait_job "$job"
  result=$(api GET "/api/v1/jobs/$job")
  jq '.restore_result.guest' <<<"$result"
  jq -e '.restore_result.guest.dropped | any(.key == "lxc.apparmor.profile")' <<<"$result" >/dev/null ||
    fail "the job report does not list the dropped lxc.apparmor.profile"
  [ "$(node pct status "$restored")" = "status: stopped" ] || fail "CT $restored is not stopped after the restore"
  config=$(node pct config "$restored")
  printf '%s\n' "$config"
  grep -qx 'onboot: 0' <<<"$config" || fail "CT $restored has no onboot: 0"
  grep -qx 'unprivileged: 1' <<<"$config" || fail "CT $restored is not unprivileged"
  ! grep -q '^lxc\.' <<<"$config" || fail "CT $restored kept an lxc.* key"
  grep -q "^mp1: $storage:vm-$restored-disk-[0-9]*,mp=/victim," <<<"$config" ||
    fail "CT $restored has no volume of its own for mp1"
  hw_old=$(node pct config "$ct" | sed -n 's/^net0: .*hwaddr=\([^,]*\).*/\1/p')
  hw_new=$(sed -n 's/^net0: .*hwaddr=\([^,]*\).*/\1/p' <<<"$config")
  [ -n "$hw_new" ] && [ "$hw_new" != "$hw_old" ] || fail "CT $restored kept the MAC address $hw_old"
  [ "$(on_node node_victim_sum "$victim")" = "$victim_sum" ] ||
    fail "the volume of CT $victim changed during the restore"
  echo "victim volume unchanged"
  compare_ct "$restored"
}

restore_file() {
  local job dest=/var/lib/lodestore/restore/ct-file owner
  job=$(api POST /api/v1/restores "$(jq -n --arg s "$snap_running" --arg i "$(b64url /fs/srv/e2e/owned.txt)" \
    --arg h "$host_id" --arg d "$dest" \
    '{snapshot_id: $s, items: [$i], target_host_id: $h, destination: $d, in_place: false,
      conflict: "skip_existing", restore_owners: true}')" | jq -r .job_id)
  wait_job "$job"
  owner=$(node find "$dest" -name owned.txt -exec stat -c '%u %g' {} +)
  [ "$owner" = "101000 101001" ] || fail "the restored file is owned by '$owner', expected '101000 101001'"
}

refuse_manual_vzdump() {
  local out status=0
  out=$(node vzdump "$ct" --storage lodestore 2>&1) || status=$?
  [ "$status" != 0 ] || fail "vzdump --storage lodestore without a job token succeeded"
  grep -q 'backups to this storage are started by Lodestore; start this backup from Lodestore' <<<"$out" ||
    { printf '%s\n' "$out" >&2; fail "vzdump without a job token failed without the expected message"; }
}

filter_differential() {
  push "$corpus" /root/corpus.jsonl 0600
  push "$root/deploy/proxmox/t/check-filtered-config.pl" /root/check-filtered-config.pl 0755
  node perl /root/check-filtered-config.pl /root/corpus.jsonl
}

node_recover() {
  set -euo pipefail
  local snapshot=$1 vmid=$2 dir=/root/ct-recover tar
  tar=/var/lib/vz/dump/vzdump-lxc-$vmid-2026_01_01-00_00_00.tar
  # shellcheck source=/dev/null
  . /root/recover.env
  rm -rf "$dir"
  install -d -m 0700 "$dir"
  /root/lode restore "$snapshot" --target "$dir"
  mkdir -p "$dir/fs/etc/vzdump"
  grep -Ev '^(lxc\.|hookscript:|dev[0-9]+:)' "$dir/config/pct.conf" >"$dir/fs/etc/vzdump/pct.conf"
  tar --numeric-owner --acls --xattrs -cpf "$tar" -C "$dir/fs" .
  pct restore "$vmid" "$tar" --storage local-lvm --unprivileged 1 --unique 1
}

recover_from_bucket() {
  kill "$server_pid"
  wait "$server_pid" 2>/dev/null || true
  server_pid=
  node systemctl stop lodestore-agent
  push "$bin/lode" /root/lode 0755
  printf '%s' "$repo_password" >"$work/repository-key"
  push "$work/repository-key" /root/repository-key 0600
  printf 'export AWS_ACCESS_KEY_ID=%q AWS_SECRET_ACCESS_KEY=%q AWS_REGION=us-east-1\nexport LODE_REPOSITORY=%q LODE_PASSWORD_FILE=/root/repository-key\n' \
    "$minio_user" "$minio_password" "s3:http://$host:9000/$bucket" >"$work/recover.env"
  push "$work/recover.env" /root/recover.env 0600
  on_node node_recover "$snap_running" "$recovered"
  compare_ct "$recovered"
}

SECONDS=0
for name in build start_storage start_server install_agent setup_ct backup_running \
  backup_stopped_and_native compare_native restore_whole restore_file refuse_manual_vzdump \
  filter_differential recover_from_bucket; do
  step "$name"
done
echo "pve e2e ok in ${SECONDS}s"
