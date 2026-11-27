---
title: "ADR 0008: Proxmox VE integration"
description: "How Lodestore backs up and restores Proxmox VE containers through the Backup Provider API: the job start rule, the socket protocol, the container layout and metadata, identity, restore, the config boundary, the cluster install, the job model and recovery from the bucket alone."
---
Status: Accepted
Date: 2026-11-26

## Context

Phase 25 backs up Proxmox VE containers (CT) and restores them whole or file by file; phase 26 does the same for virtual machines. Proxmox VE 9 offers a third party one supported way to take guest backups: the Backup Provider API of `pve-storage`, reached through a storage plugin. The decisions below were made on the sources of `pve-storage`, `pve-container`, `pve-manager`, `pve-guest-common` and `pve-common`, and checked on a nested two-node Proxmox VE 9.3 lab (see "Verified behaviour"). A statement about Proxmox VE that the lab did not observe is marked as an assumption, together with the test that checks it.

The container layout in a snapshot, its metadata keys and the public names (the `agent.yaml` keys, the installer flag, the alert names) are one-way: they end up in buckets, in user configuration and in external alerting rules. They are fixed here before any code writes them. [ADR 0007](/docs/adr/0007-source-model/) defines the instance keys used here (`proxmox-ct/<cluster>/<uuid>`); where it says that a container has no identity of its own, this ADR amends it (see "Identity").

## Decision

### Mechanism

The agent talks to Proxmox VE through a storage plugin whose `plugindata` has `features => { 'backup-provider' => 1 }`. vzdump and `pct restore` look up a backup provider only through such a plugin, so the plugin is required even though it stores nothing itself. Its content type is `backup` only. A container is backed up with the `directory` mechanism (the only one Proxmox VE offers for a container) and restored with the `directory` mechanism. A virtual machine uses `nbd` (phase 26), which Proxmox VE offers only with fleecing enabled for the job.

The plugin is two Perl files: `/usr/share/perl5/PVE/Storage/Custom/LodestorePlugin.pm` (the storage plugin) and `/usr/share/perl5/PVE/BackupProvider/Plugin/Lodestore.pm` (the provider). The provider does not live in `Custom/`: the loader tries to register every module there as a storage plugin. Nothing in `backup_container` forks: a forked child there writes into the result channel of vzdump. The constructor follows the current signature, `new_backup_provider($class, $scfg, $storeid, $log_function)`; the upstream example still passes a bandwidth limit, which `pve-storage` no longer does.

### API range

`api()` returns the running `APIVER` when it lies in 11..17 and 17 otherwise. On the lab (`APIVER` 17, `APIAGE` 8) the loader accepted versions 9 to 17, printed "implementing an older storage API, an upgrade is recommended" for 9 to 16, and refused 8 ("API version too old") and 18 ("implements an API version newer than current"); a refused plugin makes every node skip the storage section. 11 is the first version with backup providers. The upper bound is raised only after the provider is tested on the newer API.

### Job start rule

Only Lodestore starts a backup to this storage (D-01). The agent spawns the CLI, without a shell:

```
vzdump {vmid} --storage lodestore --mode snapshot --remove 0 --protected 0
```

with `LODESTORE_JOB_TOKEN` in its environment, a fresh 32-byte random token per guest. The CLI runs the job in a forked worker that keeps the environment, and the provider's `job_init` refuses a job without a valid token:

```
backups to this storage are started by Lodestore; start this backup from Lodestore
```

A job from the GUI or the API (through `pveproxy` and `pvedaemon`) and a scheduled job from Datacenter → Backup (`pvescheduler`) carry no token and fail with this text in their task log. A `pvesh create /nodes/{node}/vzdump` run as root also keeps the environment of its caller, because `pvesh` runs the API handler in its own process; the agent does not use it.

`--protected 0` overrides `protected: 1` from the node's `vzdump.conf`; without it a node that marks every backup protected (common with PBS) fails every Lodestore backup, because the plugin does not support the attribute. Of the other keys of `vzdump.conf`, `exclude-path` and the standard excludes are applied by the agent (see "CT walk"), `bwlimit` arrives as `bandwidth-limit` in bytes per second and is applied to the agent's reads, `tmpdir` is the place of the copy in suspend mode, and `script` (a hook) runs as for any vzdump job. `--remove 0` keeps vzdump from pruning: deletion is the server's (see "Install and cluster").

### Socket protocol

The provider talks to the agent over the agent's local socket in JSON lines. The operations are `pve_job_check`, `pve_ct_open`, `pve_ct_data`, `pve_ct_finish`, `pve_restore_config`, `pve_restore_open` and `pve_restore_close`. Each one requires a peer with uid 0, the job token (compared in constant time) and, for a backup, a peer pid that descends from the process the agent spawned. Every request carries `v`, the provider protocol version; a version outside the range the agent supports is refused with `protocol_mismatch`. The first line of a connection is read with a timeout, and `pve_*` sessions have a connection limit of their own, so idle connections cannot starve the provider.

One connection serves one guest. `backup_container_prepare` runs as root in the vzdump worker and sends `pve_ct_open{v, token, vmid, info}`; the agent records the peer pid. `backup_container` runs in a child of that worker, inside the container's user namespace (uid 100000 for the default map), and sends `pve_ct_data{config, excludes}` over the inherited, already checked connection. `backup_cleanup` sends `pve_ct_finish{success, error}`; the agent commits the snapshot only on `success=1` and answers with the archive size.

### CT walk

The agent reads the container through `/proc/{pid}/root/{directory}`, where `pid` is the peer of `pve_ct_open` and `directory` comes from `$info`. In snapshot and stop mode vzdump mounts the snapshot in a mount namespace of its own: the path `/mnt/vzsnap0` is empty in the agent's namespace and holds the container only through the peer's `/proc/{pid}/root`. Each entry of `$info->{sources}` (`./` for the root, `./{mp}` for each mount point that is backed up) is walked with the existing walker.

The mode is read from the line `backup mode: {mode}` that vzdump prints and is checked against the shape of `$info`: snapshot and stop read `/mnt/vzsnap0`, suspend reads a copy under the vzdump temporary directory. A mismatch fails the job.

Ownership is stored in the container's view. Root in the initial namespace sees host ids (100000 + n) in `st_uid`/`st_gid`, in POSIX ACL xattrs and in the root id of version 3 file capabilities; the agent translates all three with the container's id map, the same translation the kernel gives a reader inside the container. An id outside the map becomes the overflow id 65534 and is counted in the job report. A restore onto the node translates back the same way; an id outside the map becomes the node id of the container's 65534, never the node's own 65534, and where the map has no 65534 either an ACL entry is left out and an owner is set to the node's 65534, both reported as not applied. Owner names are left empty: the names on the node belong to the node, not the container.

`exclude-path` from `vzdump.conf` and the standard excludes (`/tmp/?*`, `/var/tmp/?*`, `/var/run/?*.pid`) arrive in `$exclude_patterns` and are applied as vzdump applies them (D-19): a pattern that starts with `/` is anchored at the root of the container, `?*` matches the entries of a directory and everything under them, not the directory. Like vzdump the walk also skips sockets and, for a container with an id map, every entry named `lost+found` (vzdump adds `--exclude=lost+found` only when its `tar` runs in the container's user namespace, and excludes the name unanchored, so at the root of each volume and at any depth below; a privileged container without `lxc.idmap` keeps it), so the set of paths equals that of a native vzdump archive of the same container (see "Verified behaviour"); the differential test of the end-to-end suite compares the two. The snapshot is committed only in `backup_cleanup(success=1)`; when anything fails after `backup_container` returned, vzdump calls it with `success=0` and the error (observed with a failing `--protected 1`).

### CT layout and metadata

This is a one-way decision (D-02). A container snapshot has the tree:

- `/fs`: the root of the container, with every backed-up mount point at its path inside the container;
- `/config/pct.conf`: the configuration as vzdump passed it to `backup_container`;
- `/config/pct.fw`: the firewall configuration, when there is one.

`Snapshot.paths` is `["/fs"]`. The set of volumes is written in `/config/pct.conf` (D-03). The snapshot metadata carries the keys from "Names":

- `ct_layout`: `1`.
- `ct_mode`: `snapshot`, `suspend` or `stop`, the mode vzdump applied. A stopped container is always backed up in `stop` mode, whatever mode was asked for.
- `ct_unprivileged`: `0` or `1`.
- `ct_idmap`: the canonical id map, present for an unprivileged container and for a privileged one with `lxc.idmap` lines. It is built as Proxmox VE builds it: the `lxc.idmap` lines when there are any, otherwise `u:0:100000:65536,g:0:100000:65536` for an unprivileged container. The form is `u:{ct}:{host}:{count}` and `g:{ct}:{host}:{count}` entries separated by commas, `u` entries before `g` entries, each group in ascending order of `ct`, at most 256 bytes. A container whose map is longer is not backed up; the job fails with `ct_idmap_unsupported`.

`ct-vectors.json` freezes the layout and the keys; a change needs a new ADR.

### Identity

The minimum is pve-manager 9.2.33 on every node of the cluster (D-17). From that version every container carries `meta: uid=…,rtime=…` (and `ctime` when Proxmox VE created it), vzdump passes `guest-uid`, `guest-rtime` and `cluster-id` in `$info`, and `pct restore` refuses to overwrite a container with another uid unless `--allow-identity-mismatch` is set.

The instance of a container is (VMID, `meta uid`), from the first scan. `source-ids.json` keeps the recorded `meta uid` next to the UUID of the instance; an empty string means the container had none. Scan and backup apply one rule: any difference between the recorded and the current uid, including "was empty, is now set" and "was set, is now gone", is a new instance. The next scan gives it a new UUID, so the old chain never adopts another container's data, and a backup of a job that still names the old key fails with `guest_identity_changed` and writes no snapshot. When both are empty the backup goes ahead: with every node at 9.2.33 or later, each container that is created or restored gets a uid at once, so a container without one is old and has not been changed since the upgrade, and a new container on the same VMID always differs from it.

The one exception is an entry in the old format (`{"ct/200": "uuid"}`, written by an agent before phase 25, when there were no container snapshots yet): it takes the current uid once without a new UUID. The docs advise changing any setting of an old container once before its first backup; otherwise its first change later starts a new instance and `source_instance_gone` for the old one.

A uid stays the same across an offline migration to another node of the cluster (with `rtime`); a live migration of a container was not observed. An old container without `meta` gets a uid with its next configuration change (with a new `rtime` and no `ctime`), and keeps it on later changes.

### Restore

A whole container is restored with the `directory` mechanism from a staging directory (D-05 as refined by D-16; a `tar` stream is impossible, because the `tar` mechanism of `pct restore` requires a regular file and reads it twice, by the sources of `pve-container`). The staging directory is `restore.guest_staging_dir/{restore_id}` (by default `/var/lib/lodestore/pve-staging/{restore_id}`), mode 0700, holding a `.lock` the agent keeps locked while the restore runs and the snapshot in `data/` beside it, so no entry of the snapshot can replace the lock; a path inside `/etc/pve` or `/var/lib/pve-cluster` is refused. The existing restore writer fills it in the container's view and does not create device nodes, except the 0:0 character devices that are overlayfs whiteouts (nested container storage needs them, or deleted files come back). It writes against a budget: before the first write and every 64 MiB it checks the free space of the target file system with `fstatvfs` and stops with `restore_no_space`, removing the staging directory, when less than max(5 %, 2 GiB) would remain. `pct restore` starts only from a complete copy: an entry that failed (a corrupt chunk, a rejected path, a conflict) or a staged `/fs` without `/etc` ends the job with `restore_incomplete`; warnings such as skipped device nodes do not.

A new container:

```
pct restore {vmid} lodestore:backup/restore/{restore_id} --unique 1 --start 0 --unprivileged 1 --onboot 0 --storage {st}
```

without `--ha-managed`. Proxmox VE allocates new volumes on `{st}` from the sizes in the configuration, copies the staging tree into them inside the container's user namespace, gives new MAC addresses and a new uid. Options on the command line win over the configuration, so `--unprivileged 1 --onboot 0` hold even if the filter let something through. Proxmox VE refuses to restore a privileged container from an external archive.

Replacing an existing container uses `--force 1` without `--unique`; the container keeps its VMID and, with `--allow-identity-mismatch 1`, its uid. Its MAC addresses come from the configuration the provider returns, so the filter writes the target's `hwaddr` into it. `--force` destroys every volume of the target, including mount points with `backup=0` and `unusedN` volumes that the snapshot cannot contain; the server lists them in the confirmation. It also deletes the target's Proxmox VE snapshots and the volumes only they use, which the confirmation cannot show, so the agent refuses to replace a container whose configuration has a snapshot section (`guest_target_has_snapshots`), both when the restore starts and right before `pct` runs. The staging directory is always removed.

A replacement is bound to the target the user confirmed:

- When the user confirms, the server puts into `GuestTarget.expected`, from fresh facts, the VMID, name, `meta uid` (empty for none), `rtime`, the UUID of the Lodestore instance and the volume lines of the target (`rootfs`, `mpN`, `unusedN`, as they are).
- Right before `pct` runs, the agent reads the target's configuration and `source-ids.json` again with the discovery code and refuses with `guest_target_changed` when anything differs.
- The job lives 15 minutes from the moment it was queued; after that the server no longer hands it out (`restore_expired`).
- "Another guest" is decided by one function of the format, shared by server and agent: two guests are the same only if both have a `meta uid` and it is equal. Otherwise the confirmation must carry the pair (uid of the snapshot, uid of the target) and it must match what the agent sees. `--allow-identity-mismatch 1` is passed only when both uids exist and differ (D-18).
- A target under HA (`ct:{vmid}` in `/etc/pve/ha/resources.cfg`) is not replaced: `guest_ha_managed`. `pct restore --force` leaves the HA resource alone, and the CRM starts a container in state `started` again once the restore releases its lock, which breaks the promise that a restored container is stopped.
- `pct.fw` from the snapshot is not restored: the provider's `archive_get_firewall_config` returns `undef`. Proxmox VE writes whatever that function returns to `/etc/pve/firewall/{vmid}.fw` without checks, so a planted file would become live rules. A new container has no rules; a replaced one keeps the rules of the target. The report and the preview list `pct.fw` with the reason `firewall`. Restoring firewall rules is deferred.

### Config boundary

The configuration returned by `archive_get_guest_config` is the only boundary under `root@pam` (PVE-08). Proxmox VE calls the function twice in one restore, and, by its sources, fails ("backup changed during the restore") if the two answers differ in `meta` (an assumption, checked by the determinism test of the filter and the end-to-end restore), so the filter is a pure function of the snapshot configuration (and, for a replacement, of the target's), and the agent registers its answer once per restore. As root Proxmox VE takes `lxc.*`, `hookscript`, `features`, bind and device mount points from that answer without any permission check (from the sources); the lab restored a bind mount of a host directory from it.

The filter is an allowlist, and it parses each property string (`rootfs`, `mpN`, `netN`, `features`) by the rules of `PVE::JSONSchema::parse_property_string`:

- parts are separated by `,`, and empty parts are skipped;
- a part is `key=value`; a part without `=` is the default key, `volume`;
- a key that is repeated, or a second default part, drops the line (Proxmox VE refuses it: "duplicate key in comma-separated list property").

The type of a mount point comes from the parsed `volume`, by the rule of `classify_mountpoint`: `/dev/…` is a device, `/…` is a bind mount, and anything else must be a storage volume id `^[a-z][a-z0-9._-]*:`; everything else is dropped. Only volume mount points are kept. The output is printed from the parsed fields in the canonical order Proxmox VE prints (`volume` first, then `mp`, then the other keys in alphabetical order), never by copying the input. The volume is rewritten into the allocation form `{storage}:{size}` (the size in GiB, rounded up; a volume without `size` or larger than 64 TiB is dropped), so no line of the output refers to an existing volume; Proxmox VE does the same when `--storage` is given.

For `netN` the filter keeps an allowlist of subkeys. `bridge` must exist on the target node (`/sys/class/net/{bridge}/bridge`, present for a Linux bridge and for an applied SDN vnet); otherwise the interface is dropped. `hwaddr` and `firewall` come from Proxmox VE for a new container (a new MAC, firewall off) and from the target for a replacement. The network connections that stay are shown in the preview and the report. `features` keeps `nesting=1` only. `meta` is kept as it is. A key repeated in the main section keeps its last line, as Proxmox VE does, so the filtered configuration has the identity Proxmox VE reads from the snapshot's.

A differential test on the nested lab feeds a corpus of hostile configurations through the filter and then through `PVE::LXC::Config::parse_pct_config` and asserts that every mount point is of type `volume` and that no `lxc`, `hookscript`, `dev*` or `env` key and no feature other than `nesting` survives (25-15).

### Install and cluster

`lodestore-agent pve install` installs the two Perl files atomically on its node, then writes a marker into pmxcfs, `/etc/pve/lodestore/plugin/{node}`, with `{agent_version} {provider_protocol} {blake3 of both Perl files} {pve_manager_version}`. It waits until every node of `/etc/pve/.members` has a marker with the same plugin version and pve-manager 9.2.33 or later, and only then adds the storage:

```
pvesm add lodestore lodestore --prune-backups keep-all=1
```

and reloads `pvedaemon`, `pveproxy`, `pvestatd` and `pvescheduler` (`systemctl reload-or-restart`). The waiting is required: a node without the plugin skips the section of the shared `storage.cfg` ("skip section 'lodestore': unsupported type"), and the first write of `storage.cfg` from that node (any `pvesm add` or GUI change) rewrites the file without it, for the whole cluster. The same happens with a node that joins the cluster later. The reload is required too: the CLI tools load the plugin at start, the long-running services only after a reload.

`free_image` and `prune_backups` of the plugin refuse ("deletion is managed by the Lodestore server"), so neither the GUI nor vzdump can delete through it. A Proxmox VE cluster is one zone of trust: root on any node can change `/etc/pve` and the plugin files of every node.

The health of the plugin is checked at every discovery and before every backup: the files are in place, their hash matches the marker and the protocol is supported. Nodes of `.members` without a marker are reported in the facts. The server does not hand a container job to a node without the plugin (`guest_plugin_missing`) and opens the warning `pve_plugin_missing` for clusters that a guest policy covers. The docs say to install the plugin on a new node before `pvecm add`.

### Job model

A job is one container instance (`jobs.source_ref`). The server expands the selector of the policy (`vmids`, `pool`, `tags`) against the fresh complete list when it hands out jobs, and an agent that finds the container is not local finishes the job with `guest_not_local`. Container jobs on one node run one at a time: vzdump holds a lock per node.

### Recovery from the bucket alone

Without the server or an agent, a container is recovered with `lode` and Proxmox VE:

1. `lode restore` the snapshot into a directory of mode 0700.
2. Check and edit `config/pct.conf` against the table of keys the filter drops (docs), then copy it to `fs/etc/vzdump/pct.conf`.
3. `tar --numeric-owner --acls --xattrs -cpf /var/lib/vz/dump/vzdump-lxc-{vmid}-{date}.tar -C fs .`
4. `pct restore {vmid} /var/lib/vz/dump/vzdump-lxc-{vmid}-{date}.tar --storage {st} --unprivileged 1 --unique 1`

The tree is in the container's view, so the files keep their owners once Proxmox VE unpacks the archive in the container's namespace. The end-to-end test runs this path (25-15).

## Names

These names are public and fixed by this ADR:

| What | Name |
|------|------|
| Snapshot metadata | `ct_layout`, `ct_mode`, `ct_unprivileged`, `ct_idmap` |
| `agent.yaml`: allow replacing an existing guest | `restore.allow_guest_in_place` (boolean, default `false`) |
| Installer flag that sets it | `lodestore-agent pve install --allow-guest-in-place` |
| `agent.yaml`: staging directory of a container restore | `restore.guest_staging_dir` (default `/var/lib/lodestore/pve-staging`) |
| Alert: a protected guest that no policy selects any more | `guest_unprotected` |
| Alert: a cluster node without the plugin or with changed plugin files | `pve_plugin_missing` |

`restore.allow_guest_in_place` follows `restore.allow_in_place` and the scope `restore:guest-in-place`, and also covers the virtual machines of phase 26. The UI calls the action "Replace".

## Verified behaviour

Observed on 2026-11-26 on the nested lab (`scripts/pve/nested-pve.sh`, two nodes on a GitHub-hosted `ubuntu-latest` runner) with proxmox-ve 9.3.0, pve-manager 9.3.1, pve-container 6.1.28, libpve-storage-perl 9.1.26 (`APIVER` 17, `APIAGE` 8), libpve-guest-common-perl 6.0.15, qemu-server 9.2.24, pve-qemu-kvm 11.1.3, lxc-pve 7.0.0, pve-ha-manager 5.2.8, kernel 7.0.14-30-pve and, for the last item of the VM list, proxmox-backup-server 4.2.14. A probe provider logged every call with its arguments, process and namespaces.

### Containers

- The call order is `new` → `job_init` → `backup_init` → `backup_get_mechanism` → `backup_container_prepare` (root, in the vzdump worker) → `backup_container` (a child of the worker, uid 100000, not dumpable) → `backup_cleanup` → `backup_handle_log_file` → `job_cleanup`. `new` receives exactly one argument after the storage id, the log function.
- A running container on LVM-thin is backed up in `snapshot` mode, a running container on a directory storage falls back to `suspend` ("mode failure - some volumes do not support snapshots"), a stopped one is `stop`. In snapshot and stop mode the worker has its own mount namespace; `/mnt/vzsnap0` is empty from the initial namespace and complete through `/proc/{worker}/root`. In suspend mode there is no new namespace and `directory` is an rsync copy under `tmpdir`, with `sources` `["."]`.
- `$info` holds `directory`, `sources`, `backup-user-id`, `bandwidth-limit` (with `bwlimit`), `firewall-config`, `guest-uid`, `guest-rtime` and `cluster-id`, and no mode.
- Root in the initial namespace sees host ids (100000 + n) as owners and a version 3 capability with root id 100000; native vzdump, whose tar runs in the user namespace, stores container ids and a version 2 capability. ACL entries behave the same: `user:101000` and `group:101001` from the host, `user:1000` and `group:1001` inside the container and in the native archive, default ACLs included. A staging tree in the container's view restores to the same host view.
- Native `vzdump --dumpdir` of the same container with the same `vzdump.conf` and a walk of the provider's `directory` differ only by what vzdump drops: `lost+found` at the root of each volume, sockets, the paths matched by `exclude-path` (`/root/excluded/?*` drops the entries of the directory, not the directory) and by the standard excludes (`/var/tmp/?*` drops whole subtrees), and it adds `./etc/vzdump/pct.conf` and `./etc/vzdump/pct.fw`. On the 22 014 shared paths, owner minus 100000 and mode agree with the tar.
- The token reaches `job_init`, `backup_container_prepare` and `backup_container` from the CLI and from `pvesh`, and not from the API or the scheduler. `/proc/{pid}/environ` of the worker and of its child does not contain it at all (the process title overwrote that memory), and the child's `environ` is readable by root only.
- `--protected 0` is accepted and overrides `protected: 1` in `vzdump.conf`; without it the backup fails with "unable to set protected flag".
- A fork inside `backup_container` corrupts the result channel of vzdump ("garbage after JSON object"); the provider does not fork there.
- Restore: `archive_get_guest_config` is called twice; `--unprivileged 1 --onboot 0` override the archive; a privileged archive is refused; `--force` over a container with another uid is refused without `--allow-identity-mismatch` and keeps the target's uid with it; `--force` over a running container is refused; `--force` destroys every volume of the target, `backup=0` included; a bind mount point from the configuration is restored as root asked; the firewall configuration returned by the provider becomes `/etc/pve/firewall/{vmid}.fw`; `--storage` rewrites volume references, and a configuration that names the volumes of another container left them unchanged (same sha256). The restored files keep their owners, capabilities and content.
- A container under HA in state `started` is restored with `--force` and started by the CRM about a minute later.
- Identity: a new container has `meta: ctime=…,rtime=…,uid=…`; a container without `meta` gets a uid with its next configuration change and keeps it on the change after; an offline migration keeps `meta`. `PVE::GuestIdentity::MIN_PVE_MANAGER_VERSION` is 9.2.33.
- pmxcfs accepts new directories and files under `/etc/pve` (mode forced to 0755 and 0640, no `chmod`, no symlinks), and another node reads them at once. `/etc/pve/.vmlist` and `/etc/pve/.members` are regular files.
- A node without the plugin, also one that joined after the section was added, logs "skip section … unsupported type" and drops the section from `storage.cfg` on its first write. Long-running services see a new plugin only after `systemctl reload-or-restart`.
- The mount point parser of Proxmox VE accepts `volume=` in any position, skips empty parts, refuses a repeated key, classifies `/…` as bind and `/dev/…` as device, accepts any other volume id (also `LOCAL-lvm:…`), and prints `volume`, `mp`, then the other keys in alphabetical order. `parse_pct_config` skips a key in upper case or with spaces around it, with a warning.
- `/sys/class/net/{name}/bridge` exists for a Linux bridge and for an applied SDN vnet.

Not observed, and therefore assumptions:

- That `features: nesting=1` stays allowed for a user other than `root@pam` (from the sources of `pve-container`); the agent restores as root, so this affects only a later restore from the Proxmox VE GUI.
- The space and speed of a staging copy for a large container on a small node: on the lab an 827 MiB tree copied in 6 s and restored in 11 s. Checked by the end-to-end test with a larger container and by the space budget of the writer.

### Virtual machines (phase 26)

- A VM backup through a provider needs fleecing (`--fleecing enabled=1,storage=…`); without it vzdump refuses ("cannot setup backup access without fleecing").
- The `nbd` mechanism exposes one Unix socket per backup with one export per drive, named `drive-{device}` (`drive-tpmstate0-backup` for the TPM state), with the contexts `base:allocation` and `qemu:dirty-bitmap:snapshot-access:{storeid}`. With a reused bitmap only the dirty areas can be read; block status outside them fails with "Invalid argument".
- The dirty bitmap has a granularity of 4 MiB. It survives a live migration with local disks. It does not survive a stop and start or a crash of QEMU ("expected bitmap was missing and has been recreated"). After an online resize it is reused, and the grown area is reported clean: the provider has to compare the size with the previous backup itself.
- The TPM state is backed up in full every time (a new bitmap each run); the EFI vars disk has a bitmap.
- A bitmap of the provider and the bitmap of a PBS storage coexist and track the same writes independently; neither backup resets the other.
- A full read of a thin, almost empty 1 GiB disk through `nbdcopy` took about one second; the speed on real data was not measured.

## End-to-end tests

The nested lab is built from the Debian 13 cloud image (pinned by sha256) and the Proxmox VE apt repositories, two nodes in about six minutes on a hosted runner with `/dev/kvm`. There is no prebuilt image: after the spike the system disk of a node was 11.7 GiB (6.2 GB compressed), and installing from apt is cheaper than pulling it.

A container end-to-end run (lab, agent, backup, restore, the recovery from the bucket alone, the differential test of the configuration filter) is estimated at 15 to 20 minutes, but every run downloads about 1.5 GB from Debian and Proxmox mirrors, and one spike run failed on a reset connection to a Debian mirror. The workflow `pve-e2e.yml` therefore runs on a schedule, on pull requests with the label `pve-e2e`, and by hand, not on every pull request; its jobs stay within 30 minutes.

## Consequences

- The format gains a container layout and four metadata keys; `ct-vectors.json` is a new file and the existing vectors do not change. `format_version` stays 1.
- A container snapshot is an ordinary tree: a `lode` from before this ADR lists and restores it by id like any snapshot, with `/fs` and `/config` as its top directories and the owners in the container's view.
- [ADR 0007](/docs/adr/0007-source-model/) said that a container has no UUID of its own. From pve-manager 9.2.33 it has `meta uid`; the instance key is still the UUID the agent generates, and the uid decides when a new instance begins.
- A Proxmox VE node older than 9.2.33 is not supported for container backups; `pve install` refuses it, and so does a cluster with such a node.
- Backups to the Lodestore storage cannot be started from Proxmox VE (GUI, scheduler, API). Restore from the Proxmox VE GUI is not offered in this phase: the plugin lists no volumes.
- A restore of a large container needs free space for the staging copy on the node, in addition to the target storage.
- A replaced container keeps its firewall rules; a new container has none, whatever the snapshot holds.
- The names in "Names" change only with a new ADR.

## Alternatives considered

- **A FUSE file system for the restore directory** instead of a staging copy (D-16). It saves the space of the copy but adds a dependency and Tier 1 code that serves a whole file system to a root process; rejected for this phase.
- **A `tar` stream for restore.** `pct restore` requires a regular file and reads it twice to check it; a stream is not possible.
- **Starting jobs through `pvesh` or the API.** `pvesh` keeps the caller's environment as the CLI does, but adds nothing; a job through the API runs in `pvedaemon` and loses the token.
- **The agent creating volumes and writing the configuration itself.** Proxmox VE knows how to allocate on every storage type, shift ids in the user namespace, assign MAC addresses through SDN IPAM and keep the identity; duplicating that would be a second, untested copy.
- **Reading the tree in `backup_container`.** That process runs as the container's mapped root, without the agent's keys and rights; it is only a transport.
- **A denylist of configuration keys.** Every key Proxmox VE adds later would pass; only an allowlist fails closed.
