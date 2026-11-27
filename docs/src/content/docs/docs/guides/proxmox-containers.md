---
title: Proxmox VE containers
description: Back up Proxmox VE containers with the Lodestore agent, restore a whole container or single files, and recover a container from the bucket alone.
---

The agent on a Proxmox VE node backs up the containers (CT) of that node through vzdump, straight into the repository of the host. A container snapshot is a tree of files, so it deduplicates with every other snapshot in the repository, and you can restore it as a container on any node of the cluster, restore single files from it, or recover it with `lode` when the server is gone. The decisions behind this page are in [ADR 0008](/docs/adr/0008-proxmox-integration/).

## Requirements

- Proxmox VE 9 with pve-manager 9.2.33 or newer on every node of the cluster.
- The Lodestore agent on every node, installed as on any host ([Add a host](/docs/quickstart/#add-a-host)).
- A [backup policy](/docs/guides/policies/#source-what-a-policy-backs-up) of type `proxmox-ct` that selects the nodes.

Virtual machines are not backed up yet; see the [roadmap](/roadmap/).

## Install the plugin

vzdump and `pct restore` reach Lodestore through a storage plugin. Run this as root on every node, after the agent is installed there:

```sh
lodestore-agent pve install
```

The command installs the plugin into `/usr/share/perl5`, reloads the Proxmox services and writes a marker for the node into `/etc/pve/lodestore/plugin/`. Then it waits until every node of the cluster has the same plugin and adds the storage `lodestore` once for the cluster. It prints one line per node (`ready`, `waiting` or `outdated (...)`) while it waits. Run it on all nodes at the same time, or one after another within `--wait-seconds` (default 600).

| Code | Meaning |
|------|---------|
| 0 | The plugin is on every node and the storage exists |
| 1 | A file, a command or `storage.cfg` failed; the message says which |
| 2 | Refused: not root, not a Proxmox VE node, or pve-manager older than 9.2.33 |
| 3 | Not every node got ready in time; the message lists the nodes where the command still has to run |

`--allow-guest-in-place` also allows replacing an existing container on this node (see [Replace a container](#replace-a-container)). All flags are in the [command reference](/docs/reference/cli/#proxmox-ve-node).

Install the plugin on a new node **before** you join it with `pvecm add`. A node without the plugin drops the `lodestore` section of the shared `storage.cfg` the first time it writes that file, for the whole cluster. The server raises the alert **Proxmox plugin missing** (`pve_plugin_missing`) for such a node. Run `lodestore-agent pve install` again on a node when that alert names it, after an agent update, and when a job failed with `protocol_mismatch`.

## Choose the containers

A policy of type `proxmox-ct` selects containers by VMID, pool or tag. A container that matches any of the three is backed up:

```yaml
apiVersion: lodestore.io/v1alpha2
kind: BackupPolicy
metadata:
  name: containers
spec:
  repository: main
  selector:
    matchLabels:
      cluster: pve
  schedule:
    cron: "0 2 * * *"
  source:
    type: proxmox-ct
    pool: production
    tags: [backup]
```

The server makes one job per container. It expands the selector against the latest list of containers when it hands the jobs out, so a container that joins the pool is backed up at the next run. Containers of one node are backed up one after another, as vzdump allows. A container that moved to another node is backed up there: the job on the old node ends with `guest_not_local`, and the server queues it on the node that lists the container, when that node is a host of the policy. Other reasons a job does not start (`source_unknown`, `guest_plugin_missing`) are in [Jobs that do not start](/docs/guides/policies/#jobs-that-do-not-start).

The host page has a Sources tab with every container the agent found, the policies that select it, and **Not protected** for those no policy selects. A container that was backed up and that no policy selects any more raises **Guest not protected** (`guest_unprotected`) after an hour; see [Alerts](/docs/guides/alerts/#guest-not-protected).

## What a backup contains

The agent starts vzdump itself, as `vzdump {vmid} --storage lodestore --mode snapshot --remove 0 --protected 0`, and reads the container while vzdump holds it:

- **Mode.** A running container on a storage with snapshots (LVM-thin, ZFS, Ceph RBD) is read from a snapshot while it keeps running. On a storage without snapshots vzdump falls back to suspend mode: it copies the container to its temporary directory (`tmpdir` of `vzdump.conf`, `/var/tmp` by default), pauses it for a final sync and reads the copy, so that folder needs room for the container. A stopped container is read in stop mode. The job records the mode.
- **Volumes.** The root file system and every mount point with `backup=1`, as vzdump backs them up. Mount points with `backup=0`, bind mounts of node folders and devices are never read.
- **Excludes.** `exclude-path` in `/etc/vzdump.conf` and the standard excludes of vzdump (`/tmp/?*`, `/var/tmp/?*`, `/var/run/?*.pid`) apply with the same meaning as in vzdump's tar: a pattern that starts with `/` is anchored at the root of the container, any other pattern matches at every depth, and `?*` keeps the folder and leaves out what is in it. Like vzdump, the agent also leaves out sockets and, for an unprivileged container or any container with an id map, every `lost+found`; a privileged container without `lxc.idmap` keeps it. The set of files equals that of a native vzdump archive of the same container.
- **Owners.** Owners, permissions, POSIX ACLs and file capabilities are stored as the container sees them, so uid 0 is root of the container, not 100000. The id map of an unprivileged container is stored with the snapshot.
- **Configuration.** The snapshot holds the configuration of the container (`/config/pct.conf`) and its firewall rules (`/config/pct.fw`) next to the files (`/fs`).

Each backup reads every file of the container; data that did not change is deduplicated and is not uploaded again.

Of the other keys of `vzdump.conf`, `tmpdir` sets the place of the suspend copy and `script` (a hook script) runs as for any vzdump job. `--protected 0` overrides `protected: 1`, which would fail every Lodestore backup. `bwlimit` is not applied to Lodestore backups yet. `--remove 0` keeps vzdump from pruning: Lodestore retention decides what is kept.

### Container identity

Lodestore follows a container by the `meta uid` that Proxmox VE gives it, not by its VMID, so a new container on a reused VMID starts a new history. A container created before pve-manager 9.2.33 and not changed since then has no uid yet. Change any setting of such a container once, for example its description, before its first backup. Otherwise Proxmox VE gives it a uid at its next change, Lodestore sees a new container from then on, and the old history raises **Source no longer selected** (`source_instance_gone`) once.

## Backups started in Proxmox VE

Only Lodestore starts backups to the `lodestore` storage, with a token for each job. A backup started from the Proxmox VE web interface, the API, a job under Datacenter → Backup or `vzdump --storage lodestore` by hand fails with:

```
backups to this storage are started by Lodestore; start this backup from Lodestore
```

Schedules and retention live in the Lodestore policy. The plugin also refuses to delete or prune ("deletion is managed by the Lodestore server").

## Restore a container

Open **Restore**, choose a container snapshot and **Restore container**. Choose the node, the storage for the volumes (one that holds `rootdir`) and the VMID; the dialog suggests the next free one. The new container:

- stays stopped and does not start on boot (`onboot: 0`);
- gets new MAC addresses and a new identity;
- runs unprivileged;
- is not added to HA.

Proxmox VE allocates new volumes on the chosen storage for the root file system and every mount point, with the sizes from the configuration, and copies the files into them. A configuration never refers to an existing volume, so a restore cannot write to the disks of another container. A snapshot of a privileged container is restored unprivileged.

### What the configuration keeps

As root, `pct restore` applies whatever the configuration says, including raw LXC settings and host folders. The agent therefore restores only the keys that configure the container from the inside (`hostname`, `cores`, `memory`, `swap`, `nameserver`, `description`, `tags`, mount points of volumes, network interfaces and the like) and drops the rest. The dialog lists every key that will not be restored with its reason before you start, and the job lists what the agent dropped:

| Reason | What is dropped | Why |
|--------|-----------------|-----|
| `hookscript` | `hookscript` | Runs a script as root on the node |
| `lxc` | every `lxc.*` key and `env` | Raw LXC settings can give root on the node |
| `bind_mount` | a mount point of a node folder | Bind mount of a node folder |
| `device` | a device mount point, `devN` | Passes a node device into the container |
| `privileged` | `unprivileged: 0` | Would change the container's privileges |
| `features` | every feature except `nesting=1` | Container features that reach the node |
| `args` | `args` | Extra arguments for the container runtime |
| `network` | an interface on a bridge the node does not have, or a setting Lodestore does not restore | The interface would point elsewhere |
| `firewall` | `pct.fw` | Firewall rules are not restored |
| `unknown` | a key Lodestore does not know | Only known keys are restored |
| `section` | snapshot sections and pending changes | Not part of the container |

Mount points are read as Proxmox VE reads them, so a line that Proxmox VE would treat as a bind mount or a device is dropped, however it is written. A network interface keeps its bridge, VLAN tag, addresses, gateway, MTU and rate when the node has that bridge (a Linux bridge or an applied SDN vnet); it gets a new MAC address and the firewall flag off. The firewall rules of the snapshot (`pct.fw`) are not restored: add the rules again in Proxmox VE.

### Replace a container

**Replace an existing container** restores a snapshot over a container in place. It needs:

- `restore.allow_guest_in_place: true` in the node's own `agent.yaml`, set with `lodestore-agent pve install --allow-guest-in-place` on that node; the server cannot turn it on;
- the scope `restore:guest-in-place` for an API token, or your password entered in the last 15 minutes in the web interface;
- a stopped container that is not an HA resource;
- a snapshot that the server signed.

The container keeps its VMID, MAC addresses, firewall flag and firewall rules. Every volume of the container is destroyed and allocated again, including mount points with `backup=0` and unused volumes that the snapshot does not hold; the dialog lists those volumes before you confirm. A snapshot of another container, or of a container without a uid, needs one more confirmation. Right before `pct restore` runs, the agent checks the container again and refuses with `guest_target_changed` if it is not the one you confirmed. The confirmation is valid for 15 minutes; after that the job fails with `restore_expired` and you confirm again. A container with Proxmox VE snapshots is not replaced, because `pct restore --force` would delete them: the job fails with `guest_target_has_snapshots`; remove the snapshots or restore to a new VMID. If any file of the Lodestore snapshot cannot be read or written while it is staged, nothing is handed to `pct restore` and the job fails with `restore_incomplete`, so a replace never leaves a partial container.

### Staging space

The agent first writes the files of the snapshot into a staging folder on the node, and `pct restore` copies them from there into the new volumes. The folder is `restore.guest_staging_dir` in `agent.yaml`, `/var/lib/lodestore/pve-staging` by default, one subfolder of mode 0700 per restore, removed when the restore ends ([Restoring on the host](/docs/install/binary/#restoring-on-the-host)). Its file system needs room for the whole container, plus a reserve of 5 % of the file system and at least 2 GiB; with less, the restore stops with `restore_no_space` and removes what it wrote. For large containers, point `guest_staging_dir` at a separate disk.

The Proxmox VE web interface does not offer a restore from the `lodestore` storage. Restore from Lodestore.

## Restore files

A container snapshot restores files like any other snapshot: choose the files in **Restore**, a node or another host, and a folder below its restore folders (`/var/lib/lodestore/restore` by default). Files are never restored into a container in place. With owners, the owners are shifted by the container's id map, so a file of uid 1000 in the container belongs to 101000 on the node by default: `cp -a` into the root file system of the container on the node then keeps the owners the container expects. Setuid and setgid bits and file capabilities are dropped, and device nodes, FIFOs and sockets are not created. The restore stops with `restore_no_space` before the file system has less than 5 % and 2 GiB free. **Download** gives the same files as an archive.

## Without the server

The bucket and the repository password or recovery key are enough to get a container back, with `lode` and Proxmox VE alone. Run this as root on a Proxmox VE node:

1. Restore the snapshot with `lode` into a folder that only root can read, as in [Restore without the server](/docs/recipes/restore-without-server/):

   ```sh
   install -d -m 0700 /root/ct-recover
   lode snapshots --source-type proxmox-ct
   lode restore SNAPSHOT --target /root/ct-recover
   ```

   The files keep the owners of the container's view: uid 0 of the container is root of this machine, and setuid files stay setuid. Keep the folder closed.

2. Check `/root/ct-recover/config/pct.conf` against the table in [What the configuration keeps](#what-the-configuration-keeps): remove `lxc.*`, `hookscript`, bind mounts, devices and anything else you do not want on the node. Then copy it into the tree:

   ```sh
   cp /root/ct-recover/config/pct.conf /root/ct-recover/fs/etc/vzdump/pct.conf
   ```

3. Pack the tree as a vzdump archive, with the owners as numbers:

   ```sh
   tar --numeric-owner --acls --xattrs -cpf /var/lib/vz/dump/vzdump-lxc-VMID-2026_01_01-00_00_00.tar -C /root/ct-recover/fs .
   ```

4. Restore it as a new container on a storage of the node:

   ```sh
   pct restore VMID /var/lib/vz/dump/vzdump-lxc-VMID-2026_01_01-00_00_00.tar --storage local-lvm --unprivileged 1 --unique 1
   ```

Proxmox VE unpacks the archive in the namespace of the new container, so the files get back their owners there. Remove `/root/ct-recover` and the archive afterwards. The end-to-end test on a nested Proxmox VE runs these four steps with the server and the agent stopped and compares owners, modes and ACLs in the new container with the original.

## Alerts

- **Guest not protected** (`guest_unprotected`): a container that was backed up is selected by no policy for an hour. See [Guest not protected](/docs/guides/alerts/#guest-not-protected).
- **Proxmox plugin missing** (`pve_plugin_missing`): a node of a cluster that a container policy covers has no plugin, or a plugin that does not match its agent. See [Proxmox plugin missing](/docs/guides/alerts/#proxmox-plugin-missing).
- A failed container backup raises **Backup failed** as any job does.

## Trust

A Proxmox VE cluster is one zone of trust: root on any node can change `/etc/pve`, and through it every node. Lodestore keeps the repository safe from any node, compromised or not: an agent cannot delete backup data. The agent accepts the plugin's requests only from root, with the token of a job it started, for the container of that job. Details are in the [threat model](/docs/security/threat-model/#proxmox-ve-nodes).

## Remove Lodestore from a cluster

There is no uninstall command yet. To remove the plugin:

1. Remove the container policies, or their selectors for this cluster, and wait for running container jobs to end.
2. On one node, remove the storage: `pvesm remove lodestore`.
3. On every node, remove the plugin and reload the services:

   ```sh
   rm /usr/share/perl5/PVE/Storage/Custom/LodestorePlugin.pm /usr/share/perl5/PVE/BackupProvider/Plugin/Lodestore.pm
   systemctl reload-or-restart pvedaemon pveproxy pvestatd pvescheduler
   ```

4. Remove the markers: `rm -r /etc/pve/lodestore`.

The snapshots stay in the repository and restore with `lode` as above.
