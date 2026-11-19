---
title: "ADR 0007: Source model, instance identity and disk image layout"
description: "How a snapshot names what it backs up: a claimed and a signed source, canonical instance keys, retention and latest per instance, restore scopes by source type, and the layout of disk images."
---
Status: Accepted
Date: 2026-11-17

## Context

Until 0.2 a snapshot is a set of files of one host. Retention, `latest` and the missing-backup alert group snapshots by host and policy, which is correct only while every snapshot is the same kind of thing. Proxmox guests and databases add many independent things on one host: forty virtual machines on one node are forty histories, and keeping the newest two snapshots of the whole host (the floor of two) would let one guest's backups push out another's.

The format also has to name the source before the first guest or database backup is written. After that, the key of an instance and the layout of an image are data in users' buckets and cannot be changed under them.

This ADR records the decisions that are one-way for that reason, and marks as provisional the few that need a test bench that does not exist yet. Phase 20 ships the model, the retention, `latest`, alert and restore-scope machinery and the formats. It does not ship a backup of a guest or a database: no agent writes such a snapshot, and a job for such a source is refused before it starts.

## Decision

### Claim and signature

Two fields carry the source, and they mean different things.

- `Snapshot.source` (field 18) is the claim of the host that wrote the snapshot. It lives in the manifest, which a compromised host writes, so it decides nothing.
- `ProvenanceRecord.source` (field 6) is the source the server checked against the job and sealed with the rest of the record ([ADR 0006](/docs/adr/0006-snapshot-provenance/)). It is the only source the server, `lode` and retention believe.

For `files` neither field is written, and the bytes of manifests and records are those of 0.2. A claim with the empty key is the same as no claim. A provenance record whose field 6 is not a valid source does not open, and a snapshot named only by such a record is unverified.

The server checks the claim when it verifies a finished job, not when it binds a catalog row to the job. This differs from the refusals of the binding step: the claim lies in the manifest bytes, which only the verification reads, and both binding paths (insert and link) already write the source of the job. A claim that differs from the source of the job, an invalid claim, a `files` snapshot for a guest job and a guest claim in a `files` job all fail the verification: no signature, no verification time and no place in retention. The refusal text is constant and carries no key. The metadata of a signed record comes from the job the server holds (later, from discovery), never from the claim.

The golden vectors of the source live in a new file, `source-vectors.json`, next to the existing ones, which did not change. `lodestore-format` fuzzes the decoder (`source_decode`).

### Instance keys

An instance key is a readable canonical string. `files` is the empty key; every other key is `<type>/<segment>[/<segment>]`.

```
proxmox-vm/<cluster>/<vm uuid>
proxmox-ct/<cluster>/<container uuid>
postgres/<system_identifier>/<database oid>
postgres-wal/<system_identifier>
mysql/<server uuid>/<schema>
image/{name}
```

A segment is UTF-8, percent-encoded: the bytes `A-Z a-z 0-9 - . _ ~` stay as they are, every other byte becomes `%XX` in upper case. A decoded segment is not empty and at most 255 bytes (`MAX_SEGMENT_LEN`); the whole key is at most 512 bytes (`MAX_KEY_LEN`). Only the canonical spelling parses, the one that equals the re-encoding of its own decoding, so two spellings never name two instances and the encoding is injective. A uuid is lower case, `8-4-4-4-12`. `system_identifier` is a decimal `u64` and an oid a decimal `u32`, neither zero nor with a leading zero. The `@` and `:` characters in a key are always percent-encoded.

The type is the prefix. A reader that does not know a prefix treats the record as unreadable; it never guesses (fail closed).

Identity by type:

- `proxmox-vm` and `proxmox-ct` are (cluster, canonical lower-case UUID). A guest is never told apart by its VMID, so a reused VMID or CTID is a new instance. The cluster segment is the corosync cluster name, or the node name on a node outside a cluster. A node that joins a cluster later gives its guests keys with the cluster name: they become new instances, as after a migration (below). A container has no UUID of its own, so the agent of the node generates one and stores it (below); a key built from the container id alone is refused.
- `postgres` is (cluster `system_identifier`, database OID); a recreated database has a new OID. `postgres-wal` is the cluster alone.
- `mysql` is (server UUID, schema).
- `image` is the name given to `lode backup --image` with `--name`; without `--name` it is the file name of the source (`disk.raw`, `sdb`). The name is 1 to 200 bytes of UTF-8, has no `/`, no byte below 0x20 or 0x7F, and is not `.` or `..`. It is one segment of the key, so the rule also caps its percent-encoded length at 506 bytes (each byte outside `A-Z a-z 0-9 - . _ ~` counts as three), which keeps `image/{name}` within `MAX_KEY_LEN`: every name the rule accepts builds its key, and a longer one is refused before anything is read. The identity is the name and not the device, because the path of a device (`/dev/sdX`) changes between boots and between hosts, and one disk can be reached by several paths. The instance is (host, key), as for the other types, so two different disks backed up under one name on one host are one instance with one retention history and one `latest@image/{name}`. The server policy does not accept the type `image`: no agent runs such a backup, `lode` does.

Metadata (`SourceMeta`, sorted by key, a key once) is at most 32 entries, keys `[a-z0-9_]{1,64}`, values at most 256 bytes. It is for display, such as the VMID, a name or a server version; it is never part of the identity.

### An instance is (host, key)

This is a one-way decision. Retention, the floor of two, the missing-backup alert and the departure event are computed per (policy, host, key). A guest that migrates to another node by HA, or a PostgreSQL that fails over to another host, starts a new group on the new host with its own retention and alert; the old group keeps its floor of two newest good snapshots (nothing new deletes them) and raises one `source_instance_gone` event. Groups are not merged across hosts. History under live data does not change.

`latest` follows the same split. In the API the key narrows a list only inside the mandatory `host`, and the same key on another host is another instance. In `lode` the newest signed snapshot of a key is chosen across the hosts of the repository, as `latest` for `files` is in 0.2 (whichever host made it), but only while one host made them; with several hosts `lode` refuses (see "A key is not bound to a host"). `lode` writes it `latest@<key>`: a colon already separates the path in `SNAPSHOT[:PATH]`, and `@` cannot occur in a canonical key.

### Retention, latest and the missing-backup alert per instance

Retention groups snapshots by (policy, host, source key). `files` is the host with an empty key. Inside a group the pools stay: scheduled, manual, and one stdin pool per stream name. The floor of two applies to each pool of each source. The source of a group is assigned by the server from the job and the signature; the agent does not pick it. The anomaly baseline of a snapshot is the history of its own group.

An instance that stopped receiving snapshots (a guest deleted, a VMID reused, a database recreated) gets the ordinary rules of the policy, and the floor keeps its two newest good snapshots indefinitely. There is no rule that deletes it. This is how a host that left the selector already behaves.

`latest` chooses among `files` snapshots by default. A named instance is chosen with `latest@<key>` in `lode` and with the `source` parameter in the API. A WAL batch (`postgres-wal`) is never `latest`: it is not a point-in-time image. A `lode` of this version in a repository without readable signatures (the legacy mode of ADR 0006) skips snapshots whose manifest claims a source other than `files`, and answers "no verified snapshot" for a named instance. A `lode` 0.2 does not know fields 18 and 6; in a repository that holds guest or database snapshots it chooses `latest` among all snapshots.

The missing-backup alert is raised per expected instance. For `files` that is the host. For other types it is the list that discovery supplies, below. The moment an instance stopped being expected (the first alert cycle in which the policy no longer selects it) is stored in `source_instances_gone`. An hour after that moment the server raises exactly one `source_instance_gone` warning for 24 hours, whatever the time of the last success and whatever limit the policy sets. The row is removed when the instance is expected again before the warning opens, or after its window has passed; an instance that returns while the warning is open keeps the row, marked as returned, so a scan that blinks does not open a second warning. A later departure opens a new event. The alert key of a `files` pair is unchanged from 0.2.

### Discovery of sources

The agent finds the sources of its host and reports them; the server stores the list as agent facts. The list is untrusted: it is clipped (1000 guests, 20 tags per guest, 16 database servers, 1000 databases per server, 64 bytes per name, 256 KiB), an item with an invalid or repeated key is dropped, and the server builds every instance key itself with the functions of `lodestore_format::source`, never taking one from the agent. A list that was cut is marked as such, and a list that is over the size limit is not stored and counts as unknown. It offers instances to the form, it narrows which jobs are handed out, and it gives the list of expected instances for the missing-backup alert. It grants no scope, changes no selector, and a signed record still takes its source from the job the server holds.

The agent scans at start and every five minutes. A scan that fails leaves the previous list in place, and a list older than 15 minutes counts as unknown, not as empty. An empty list means a successful scan that found nothing.

**Container identity.** Proxmox gives a virtual machine a UUID but a container none. The agent generates a UUID for each container, keeps it in `/var/lib/lodestore/source-ids.json` (mode 0600) and writes nothing to the Proxmox configuration. The entry is dropped when the container is gone from a scan, so a reused VMID gets a new UUID, and so does a container that changes into a VM and back. A reinstalled node, a container migrated to another node and a reused VMID are therefore new instances, the same semantics as a guest that migrates. A container recreated with the same VMID between two scans, five minutes apart at most, is not told apart; the backup of containers will check the identity again when it exists. A VM whose SMBIOS UUID is zero, invalid or shared with another VM of the node (a copy restored without a new UUID) is not reported, because its key would not be unique.

**A key is not bound to a host.** The key stays (host, key) for retention and alerts, and a clone, a PostgreSQL standby or a restored copy keeps being backed up under the key it has. `lode latest@<key>` refuses when the signed snapshots of the key come from more than one host, names the hosts with their newest snapshot and asks for `--host`; `--host` takes a host id, or a name that belongs to one host (a value in the form of a UUID is only ever a host id), and then chooses among the snapshots that host made. A snapshot signed with the repository password has no host id, so its host name stands for the host. With one host nothing changes. The API already requires `host`. The Sources tab of a host notes an instance that another host reports too.

**Expected instances.** For a policy of guests or databases the missing-backup alert expects the instances that its selector chooses from the fresh, complete list, together with every instance whose latest job failed. With no list (agent offline, scan failed, list older than 15 minutes) or with a list that was cut short, it also expects every instance that has snapshots, so that an unknown list never silences the alert. When a host has no known instance, one placeholder pair stands for the policy on that host, so a job that is refused still raises an alert. An instance that is no longer expected does not make its host healthy and does not set the last success of the fleet overview.

### Restore scopes and targets

There are four new scopes, independent of each other and of the existing ones: `restore:guest`, `restore:guest-in-place`, `restore:db`, `restore:db-in-place`. `restore:host` and `restore:in-place` do not include them, and tokens issued before do not get them. An admin session gets them as it gets every scope, and the in-place variants need a step-up by the existing rule. Restoring a guest is root on the Proxmox node; the threat model describes the risk. No endpoint uses these scopes in phase 20.

`RestoreSpec` gains `oneof target { GuestTarget guest = 8; DatabaseTarget database = 9; }`. The rule for compatibility: the server sends a guest or database target with an empty `destination` and `in_place = false`, so an agent released before targets existed fails on the existing check of the destination and creates nothing; agents from this release refuse any spec that sets `target` before they look at the destination. A files restore of a snapshot of another type is refused with 422 by the server, with one exception: a snapshot of type `image`.

For an image, a selection of `/` restores the whole image as one sparse file `{name}.img`; any other selection restores the segments as ordinary files, as for a snapshot of files. `in_place` is refused with 422 for an image. A restore of the whole image with `overwrite_changed` needs `restore:in-place`, and a session needs the step-up, as any restore in place does; `skip_existing` and `stop_if_not_empty` need `restore:host` as before. This is defence in depth, not a boundary: `restore:host` with `overwrite_changed` can still replace a `NAME.img` in a restore folder by restoring a files snapshot that holds one, and the segments of `/disks/NAME` by restoring them again. There is no scope of its own: the restore uses the same planner, writes an ordinary sparse file and goes through the same scopes, so the risk is that of a file restore. The server does not write to block devices; that is `lode restore --to-device` only.

The server hands the agent the plan of the image as one entry, and the head of that plan is bounded: the entry of an image with many holes lists its extents, and the server accepts at most 4 MiB of it (about 350 thousand extents). A fragmented image above that is refused before the job with 422 ("too many holes") and a pointer to `lode restore`, which has no such limit. This is a deliberate limit of the restore through the server.

### Disk images

A disk image is a sparse file in the ordinary tree of a snapshot. It is split into segments of 256 GiB, and its chunks are fixed-size and aligned. A chunk of zeros is a hole, recorded in `sparse_extents`. There is no new object type and no `format_version` 2, so cleanup, `lode check`, find, the disaster-recovery path of `lode` and restore read an image as they read any file.

The layout is final from phase 22:

- The segments are the files `/disks/{name}/000000`, `/disks/{name}/000001` and so on (six decimal digits, consecutive from `000000`) in the ordinary tree, and `Snapshot.paths` is `["/disks/{name}"]`.
- A chunk is 4 MiB, which is also the blob limit of [ADR 0003](/docs/adr/0003-object-layout/), fixed and aligned to the start of the image. A segment holds at most 65 536 chunks (256 GiB). Every segment but the last is a whole number of chunks. The largest image is 64 segments, 16 TiB.
- The canonical node of a segment lists the ids of the non-zero chunks only, in order, and its extents are always non-empty: `[{0, 0}]` for a segment that is all holes. A reader accepts only this form, so one image has one encoding.
- The snapshot metadata says how to read it: `image_layout` (`1`), `chunk_size` and `read_mode` (`full`). Each disk has `disk_{h}_size`, the size of the image in bytes.
- `image-vectors.json` freezes the layout; it is a new file, and `lodestore-format` fuzzes the decoder (`image_layout`).

A reader that does not know the layout or the chunk size refuses the snapshot; it does not guess. It also refuses an image whose segments add up to another size than `disk_{h}_size`.

A backup reads the source once, without a snapshot of its own, so an image is crash-consistent at best. After the last read `lode` checks the source again and fails without a snapshot when a regular file changed its size, modification time or change time, or a block device changed its size; it warns when the device is mounted or held by another device.

#### Restoring to a block device

`lode restore ID --to-device DEVICE` is the only way to write an image to a device. It works on Linux only, opens the device with `O_EXCL`, and refuses before the first write a device that is not a block device, is smaller than the image, is mounted (or has a mounted partition), is used as swap or is held by another device. Holes are written as zeros; bytes past the end of the image are left alone, and the data is flushed at the end. In a repository with signatures it refuses a snapshot that nothing signed, and one whose signatures cannot be read, unless `--accept-unverified` is given. A repository without a single signature has nothing to compare with, so it writes.

The checks have one limit, accepted: the kernel does not tell a device that a process (for example a virtual machine) holds open without an exclusive lock from a free one. Stop such processes before the restore.

### Dirty bitmap binding

A snapshot of a VM that is a base for an incremental one records in its metadata what binds a dirty bitmap to it: the bitmap name (it contains the repository id and the policy id, so a bitmap of another repository or policy is never reused), its generation, the id of the parent snapshot, the size of the disk, and the VM uuid and `vmgenid`. A mismatch of any of them means a full read, never a trusted bitmap. The key names are fixed by phase 22 together with the writer. The metadata of a disk is keyed `disk_{h}_...`, where `{h}` is the first 16 lower-case hex characters of the BLAKE3 hash (no key) of the bytes of the disk name; the name itself, up to 200 bytes, would not fit a metadata key (`[a-z0-9_]{1,64}`).

Phase 22 writes `image_layout`, `chunk_size`, `read_mode` (`full`) and `disk_{h}_size`. Reserved for the phase of the incremental read (it writes them; phase 22 does not): `disk_{h}_bitmap_name`, `disk_{h}_bitmap_gen`, `disk_{h}_full_reason`, `vm_uuid` and `vmgenid` (phase 26), and the form of the bitmap name, `lode-{repo16}-{inst16}-g{N}`. The codes of `full_reason` are `no_bitmap`, `bitmap_missing`, `no_state`, `state_unreadable`, `no_parent`, `parent_not_newest`, `parent_unreadable`, `name_changed`, `gen_mismatch`, `size_changed` and `source_changed`.

The rule: the decision between a full and an incremental read is made before the first read; the state of the bitmap is spent before the read and written only after the snapshot is confirmed. Nothing in this ADR or in the documentation says that the incremental read exists: it does not yet.

### WAL batches

A `postgres-wal` instance is keyed by the cluster. WAL batches have their own pool. The server reads `backup_label` of the base backup and forgets WAL before that anchor; `.history` files are never forgotten.

## Provisional and deferred

The chunk size of 4 MiB is no longer provisional: it is final with phase 22, the first phase that writes an image. The shape of the NBD export of PVE 9.2 (`nbdinfo --map`, `query-block`, speed) is a spike of phase 25; the format does not depend on its result.

Deferred:

- A `source_ref` column in `jobs_coalesce_idx`: phase 25, where the server first hands out jobs that are not `files`. Until then the job columns of the source are filled only by tests. `JobSpec.source` already carries the selector of the policy, so the agent can check it against its own scan.
- Series of the `lodestore_backup_*` metrics per instance with a source label, and the last snapshot of a host by source: phase 25. Today the series cover `files`, and the last snapshot of a host is the newest of any source (`shortcut`).
- "Replaced" as a state separate from "gone": a later phase.

## Consequences

- `format_version` stays 1. The golden vectors that exist did not change; `source-vectors.json` is new.
- A server of 0.2 does not read policies of `lodestore.io/v1alpha2`, the only version that has `spec.source`. A policy of `files` is exported in the old version and byte for byte as before.
- An older `lode` counts a record whose source it cannot decode as unreadable, and reads a snapshot of an unknown type only by id. This holds for a snapshot of an image and for `lode` 0.2 and the builds of phases 20 and 21: such a `lode` reads the snapshot only by its id, restores the segments as files, and joining them in the order of their names with `cat` gives the image. The server policy does not accept the type `image`.
- `image-vectors.json` is new; the other vectors did not change.
- The key of an instance is not bound to the host that may claim it (see "A key is not bound to a host"). A compromised host that reports the key of an instance of another host can end up with signed snapshots under that key; `lode latest@<key>` then refuses and names both hosts, and does not choose the newer snapshot.
- The key of an instance and the layout of an image cannot change under data already written; a change needs a new ADR.
- A guest that migrates or fails over looks like a new instance, with a new retention history, until a later phase adds a way to say "the same guest". The old group is not deleted by this.

## Alternatives considered

- **Check the claim when a row is bound to the job.** The claim is not in the catalog; only the verification reads the manifest.
- **Identity by VMID.** A VMID is reused by a different guest; keeping its history would mix two guests.
- **A new object type for images, or `format_version` 2.** Cleanup, check and restore would need new paths; a sparse file in the tree needs none.
- **Merge groups across hosts.** It would make a retention decision depend on which hosts the server knows and could delete a history under a guest that moved.
