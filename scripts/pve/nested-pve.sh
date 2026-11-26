#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Builds and runs a nested Proxmox VE 9 lab (one or two nodes) for the Proxmox
# spikes and end-to-end tests. Needs a Linux host with a usable /dev/kvm.
#
#   scripts/pve/nested-pve.sh up [--nodes 1|2] [--no-join] [--dry-run]
#   scripts/pve/nested-pve.sh ssh N [command...]
#   scripts/pve/nested-pve.sh down
#
# `up` boots Debian 13 cloud images under qemu-system-x86_64 -enable-kvm, installs
# proxmox-ve from the no-subscription repository, forms a cluster with --nodes 2
# (--no-join creates it on pve1 and leaves `pvecm add` on pve2 to the caller),
# adds a directory storage `dirstore` and an LVM-thin storage `local-lvm`, and
# downloads the newest Debian 13 CT template on every node. `--dry-run` prints the
# steps without running them. `ssh N` opens a root shell on node N. `down` stops the
# VMs and deletes their disks; the downloaded base image stays cached.
#
# Node N is pveN: ssh on 127.0.0.1:$((2220 + N)), cluster network 10.10.10.N/24 on
# a QEMU multicast socket (the host's `lo` must have the MULTICAST flag:
# `sudo ip link set lo multicast on`).
#
# Environment:
#   PVE_WORKDIR          disks, seeds, logs (default ~/.cache/lodestore-nested-pve)
#   PVE_SSH_KEY          public key allowed on the nodes (default ~/.ssh/id_ed25519.pub)
#   PVE_NODE_MEMORY_MIB  memory per node (default 6144)
#   PVE_NODE_CPUS        vCPUs per node (default 2)
set -euo pipefail

DEBIAN_IMAGE_URL=https://cloud.debian.org/images/cloud/trixie/20261123-2671/debian-13-genericcloud-amd64-20261123-2671.qcow2
DEBIAN_IMAGE_SHA256=6b220af8e98d6f8c47fe64f4a13291cedc8ebd675a6c893797ef2d546c83dfbb
PVE_KEYRING_URL=https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg
PVE_KEYRING_SHA256=136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45
PVE_REPO_URL=http://download.proxmox.com/debian/pve
CLUSTER_NAME=lodestore-lab
CLUSTER_NET=10.10.10
CLUSTER_MCAST=230.0.0.1:1234

workdir=${PVE_WORKDIR:-$HOME/.cache/lodestore-nested-pve}
ssh_pub=${PVE_SSH_KEY:-$HOME/.ssh/id_ed25519.pub}
memory=${PVE_NODE_MEMORY_MIB:-6144}
cpus=${PVE_NODE_CPUS:-2}
dry_run=0

usage() {
  sed -n '3,26p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

die() {
  echo "nested-pve: $*" >&2
  exit 1
}

run() {
  if [ "$dry_run" = 1 ]; then
    printf '+ %s\n' "$*"
  else
    "$@"
  fi
}

ssh_args() {
  local n=$1
  local args=(-p "$((2220 + n))" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
    -o LogLevel=ERROR -o ConnectTimeout=5 -o ServerAliveInterval=15)
  [ ! -f "${ssh_pub%.pub}" ] || args+=(-i "${ssh_pub%.pub}")
  printf '%s\n' "${args[@]}" root@127.0.0.1
}

node_ssh() {
  local n=$1
  shift
  local args
  mapfile -t args < <(ssh_args "$n")
  # shellcheck disable=SC2029
  ssh "${args[@]}" "$@"
}

# Runs a fixed command on node N.
on_node() {
  local n=$1
  shift
  if [ "$dry_run" = 1 ]; then
    printf '+ [pve%s] %s\n' "$n" "$*"
  else
    node_ssh "$n" "$@"
  fi
}

# Runs one of the remote_* functions below on node N, shipped by `declare -f`.
on_node_fn() {
  local n=$1 fn=$2
  shift 2
  if [ "$dry_run" = 1 ]; then
    printf '+ [pve%s] %s %s\n' "$n" "$fn" "$*"
    return
  fi
  { declare -f "$fn"; printf '%s "$@"\n' "$fn"; } | node_ssh "$n" bash -s -- "$@"
}

boot_id() {
  node_ssh "$1" cat /proc/sys/kernel/random/boot_id 2>/dev/null || true
}

wait_ssh() {
  local n=$1 old=${2:-} deadline=$((SECONDS + 600)) id
  [ "$dry_run" = 0 ] || { printf '+ wait for ssh on pve%s\n' "$n"; return; }
  while [ "$SECONDS" -lt "$deadline" ]; do
    id=$(boot_id "$n")
    if [ -n "$id" ] && [ "$id" != "$old" ]; then
      return
    fi
    sleep 5
  done
  die "pve$n did not come up within 600 s; see $workdir/node$n/serial.log"
}

reboot_node() {
  local n=$1 old
  if [ "$dry_run" = 1 ]; then
    printf '+ [pve%s] systemctl reboot\n' "$n"
    return
  fi
  old=$(boot_id "$n")
  node_ssh "$n" systemctl reboot || true
  wait_ssh "$n" "$old"
}

remote_stage1() {
  set -euo pipefail
  local keyring_url=$1 keyring_sha=$2 repo_url=$3
  local keyring=/usr/share/keyrings/proxmox-archive-keyring.gpg
  export DEBIAN_FRONTEND=noninteractive
  echo "grub-pc grub-pc/install_devices multiselect /dev/vda" | debconf-set-selections
  apt-get update -q
  apt-get install -yq curl ca-certificates
  curl -fsSL "$keyring_url" -o "$keyring"
  echo "$keyring_sha  $keyring" | sha256sum -c -
  printf 'Types: deb\nURIs: %s\nSuites: trixie\nComponents: pve-no-subscription\nSigned-By: %s\n' \
    "$repo_url" "$keyring" >/etc/apt/sources.list.d/pve-install-repo.sources
  apt-get update -q
  apt-get full-upgrade -yq
  apt-get install -yq proxmox-default-kernel
}

remote_stage2() {
  set -euo pipefail
  export DEBIAN_FRONTEND=noninteractive
  echo "postfix postfix/main_mailer_type select No configuration" | debconf-set-selections
  apt-get install -yq proxmox-ve postfix open-iscsi chrony libnbd-bin acl attr
  rm -f /etc/apt/sources.list.d/pve-enterprise.sources /etc/apt/sources.list.d/ceph.sources
  local old
  # shellcheck disable=SC2016
  mapfile -t old < <(dpkg-query -W -f '${Package}\n' 'linux-image-*cloud*' 2>/dev/null || true)
  [ "${#old[@]}" = 0 ] || apt-get purge -yq "${old[@]}"
  apt-get purge -yq os-prober || true
  update-grub
}

remote_stage3() {
  set -euo pipefail
  pvcreate -ffy /dev/vdb
  vgcreate pve /dev/vdb
  lvcreate -y -l 90%FREE -T pve/data
  mkdir -p /srv/dirstore
  pveam update
  local template
  template=$(pveam available --section system | awk '$2 ~ /^debian-13-standard_.*_amd64\./ {print $2}' | sort -V | tail -n 1)
  [ -n "$template" ] || { echo "no debian-13-standard template in pveam available" >&2; exit 1; }
  pveam download local "$template"
  pveversion -v
}

seed_node() {
  local n=$1 nodes=$2 dir=$workdir/node$1 hosts i
  hosts="127.0.0.1 localhost"
  for i in $(seq 1 "$nodes"); do
    hosts+="\\n$CLUSTER_NET.$i pve$i.lodestore.test pve$i"
  done
  cat >"$dir/user-data" <<EOF
#cloud-config
hostname: pve$n
fqdn: pve$n.lodestore.test
manage_etc_hosts: false
disable_root: false
ssh_pwauth: false
users:
  - name: root
    ssh_authorized_keys:
      - $(cat "$ssh_pub")
      - $(cat "$workdir/cluster_key.pub")
write_files:
  - path: /etc/hosts
    content: "$hosts\\n"
  - path: /root/.ssh/id_ed25519
    permissions: "0600"
    encoding: b64
    content: $(base64 -w0 "$workdir/cluster_key")
EOF
  cat >"$dir/network-config" <<EOF
version: 2
ethernets:
  wan:
    match:
      macaddress: "52:54:00:00:00:0$n"
    dhcp4: true
  lan:
    match:
      macaddress: "52:54:00:00:01:0$n"
    addresses: [$CLUSTER_NET.$n/24]
EOF
  cloud-localds -N "$dir/network-config" "$dir/seed.iso" "$dir/user-data"
}

start_node() {
  local n=$1 dir=$workdir/node$1
  run qemu-system-x86_64 -name "pve$n" -machine q35,accel=kvm -enable-kvm -cpu host \
    -smp "$cpus" -m "$memory" \
    -drive "file=$dir/os.qcow2,if=virtio,format=qcow2" \
    -drive "file=$dir/data.qcow2,if=virtio,format=qcow2" \
    -drive "file=$dir/seed.iso,if=virtio,format=raw,readonly=on" \
    -netdev "user,id=wan,hostfwd=tcp:127.0.0.1:$((2220 + n))-:22" \
    -device "virtio-net-pci,netdev=wan,mac=52:54:00:00:00:0$n" \
    -netdev "socket,id=lan,mcast=$CLUSTER_MCAST,localaddr=127.0.0.1" \
    -device "virtio-net-pci,netdev=lan,mac=52:54:00:00:01:0$n" \
    -display none -serial "file:$dir/serial.log" \
    -pidfile "$dir/qemu.pid" -daemonize
}

install_node() {
  local n=$1
  on_node_fn "$n" remote_stage1 "$PVE_KEYRING_URL" "$PVE_KEYRING_SHA256" "$PVE_REPO_URL"
  reboot_node "$n"
  on_node_fn "$n" remote_stage2
  reboot_node "$n"
  on_node_fn "$n" remote_stage3
}

cmd_up() {
  local nodes=1 join=1 n pids=() status=0
  while [ $# -gt 0 ]; do
    case $1 in
      --nodes) nodes=${2:-}; shift 2 ;;
      --no-join) join=0; shift ;;
      --dry-run) dry_run=1; shift ;;
      *) usage ;;
    esac
  done
  case $nodes in 1 | 2) ;; *) usage ;; esac

  if [ "$dry_run" = 0 ]; then
    [ "$(uname -s)" = Linux ] || die "needs a Linux host with KVM"
    [ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm is missing or not accessible"
    for n in qemu-system-x86_64 qemu-img cloud-localds curl sha256sum ssh ssh-keygen base64; do
      command -v "$n" >/dev/null || die "missing $n"
    done
    [ -f "$ssh_pub" ] || die "no public key at $ssh_pub (set PVE_SSH_KEY)"
    ip link show lo | grep -q MULTICAST || die "lo has no MULTICAST flag: sudo ip link set lo multicast on"
  fi

  run mkdir -p "$workdir"
  local base=$workdir/${DEBIAN_IMAGE_URL##*/}
  if [ "$dry_run" = 1 ] || [ ! -f "$base" ]; then
    run curl -fSL --retry 3 -o "$base.part" "$DEBIAN_IMAGE_URL"
    run mv "$base.part" "$base"
  fi
  if [ "$dry_run" = 1 ]; then
    printf '+ sha256sum -c (%s  %s)\n' "$DEBIAN_IMAGE_SHA256" "$base"
  else
    echo "$DEBIAN_IMAGE_SHA256  $base" | sha256sum -c - || { rm -f "$base"; die "bad sha256 for $base"; }
  fi
  [ -f "$workdir/cluster_key" ] || run ssh-keygen -q -t ed25519 -N '' -C nested-pve -f "$workdir/cluster_key"

  for n in $(seq 1 "$nodes"); do
    run mkdir -p "$workdir/node$n"
    run qemu-img create -q -f qcow2 -F qcow2 -b "$base" "$workdir/node$n/os.qcow2" 32G
    run qemu-img create -q -f qcow2 "$workdir/node$n/data.qcow2" 16G
    if [ "$dry_run" = 1 ]; then
      printf '+ cloud-localds %s/node%s/seed.iso (hostname pve%s, %s.%s/24)\n' "$workdir" "$n" "$n" "$CLUSTER_NET" "$n"
    else
      seed_node "$n" "$nodes"
    fi
    start_node "$n"
  done
  for n in $(seq 1 "$nodes"); do
    wait_ssh "$n"
    echo "pve$n: ssh up after ${SECONDS}s"
  done

  if [ "$dry_run" = 1 ]; then
    for n in $(seq 1 "$nodes"); do install_node "$n"; done
  else
    for n in $(seq 1 "$nodes"); do
      install_node "$n" > >(sed "s/^/[pve$n] /") 2>&1 &
      pids+=("$!")
    done
    for n in "${pids[@]}"; do wait "$n" || status=1; done
    [ "$status" = 0 ] || die "installing proxmox-ve failed"
  fi
  echo "proxmox-ve installed after ${SECONDS}s"

  if [ "$nodes" = 2 ]; then
    on_node 1 pvecm create "$CLUSTER_NAME" --link0 "$CLUSTER_NET.1"
    on_node 2 "ssh-keyscan -t ed25519 $CLUSTER_NET.1 >>/root/.ssh/known_hosts"
    if [ "$join" = 1 ]; then
      on_node 2 pvecm add "$CLUSTER_NET.1" --link0 "$CLUSTER_NET.2" --use_ssh 1
      on_node 1 pvecm status
    else
      echo "pve2 not joined; run: pvecm add $CLUSTER_NET.1 --link0 $CLUSTER_NET.2 --use_ssh 1"
    fi
  fi
  on_node 1 pvesm add lvmthin local-lvm --vgname pve --thinpool data --content rootdir,images
  on_node 1 pvesm add dir dirstore --path /srv/dirstore --content rootdir,images
  on_node 1 pvesm status
  echo "nested PVE ready after ${SECONDS}s"
}

cmd_ssh() {
  local n=${1:-}
  [ -n "$n" ] || usage
  shift
  local args
  mapfile -t args < <(ssh_args "$n")
  exec ssh "${args[@]}" "$@"
}

cmd_down() {
  local pidfile pid
  for pidfile in "$workdir"/node*/qemu.pid; do
    [ -f "$pidfile" ] || continue
    pid=$(cat "$pidfile")
    kill "$pid" 2>/dev/null || true
    while kill -0 "$pid" 2>/dev/null; do sleep 1; done
  done
  rm -rf "$workdir"/node* "$workdir"/cluster_key "$workdir"/cluster_key.pub
}

case ${1:-} in
  up) shift; cmd_up "$@" ;;
  ssh) shift; cmd_ssh "$@" ;;
  down) cmd_down ;;
  *) usage ;;
esac
