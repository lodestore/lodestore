---
title: "ADR 0007: Source model, instance identity and disk image layout"
description: "How a snapshot names what it backs up: a claimed and a signed source, canonical instance keys, retention and latest per instance, restore scopes by source type, and the layout of disk images."
---
Status: Accepted
Date: 2026-11-17

## Context

Until 0.2 a snapshot is a set of files of one host. Retention, `latest` and the missing-backup alert group snapshots by host and policy, which is correct only while every snapshot is the same kind of thing. Proxmox guests and databases add many independent things on one host: forty virtual machines on one node are forty histories, and keeping the newest two snapshots of the whole host (the floor of two) would let one guest's backups push out another's.

The format also has to name the source before the first guest or database backup is written. After that, the key of an instance and the layout of an image are data in users' buckets and cannot be changed under them.

This ADR records the decisions that are one-way for that reason, and marks as provisional the few that need a test bench that does not exist yet. Phase 20 ships the model, the retention, `latest`, alert and restore-scope machinery and the formats. It does not ship a backup of a guest or a database: the server answers 422 to a policy whose `spec.source` it cannot run, and no agent writes such a snapshot.

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
```

A segment is UTF-8, percent-encoded: the bytes `A-Z a-z 0-9 - . _ ~` stay as they are, every other byte becomes `%XX` in upper case. A decoded segment is not empty and at most 255 bytes (`MAX_SEGMENT_LEN`); the whole key is at most 512 bytes (`MAX_KEY_LEN`). Only the canonical spelling parses, the one that equals the re-encoding of its own decoding, so two spellings never name two instances and the encoding is injective. A uuid is lower case, `8-4-4-4-12`. `system_identifier` is a decimal `u64` and an oid a decimal `u32`, neither zero nor with a leading zero. The `@` and `:` characters in a key are always percent-encoded.

The type is the prefix. A reader that does not know a prefix treats the record as unreadable; it never guesses (fail closed).

Identity by type:

- `proxmox-vm` and `proxmox-ct` are (cluster, canonical lower-case UUID). A guest is never told apart by its VMID, so a reused VMID or CTID is a new instance. The cluster segment is the corosync cluster name, or the node name on a node outside a cluster. A node that joins a cluster later gives its guests keys with the cluster name: they become new instances, as after a migration (below). A container without a UUID of its own gets one that discovery generates and stores (phase 21); a key built from the container id alone is refused.
- `postgres` is (cluster `system_identifier`, database OID); a recreated database has a new OID. `postgres-wal` is the cluster alone.
- `mysql` is (server UUID, schema).

Metadata (`SourceMeta`, sorted by key, a key once) is at most 32 entries, keys `[a-z0-9_]{1,64}`, values at most 256 bytes. It is for display, such as the VMID, a name or a server version; it is never part of the identity.

### An instance is (host, key)

This is a one-way decision. Retention, the floor of two, the missing-backup alert and the departure event are computed per (policy, host, key). A guest that migrates to another node by HA, or a PostgreSQL that fails over to another host, starts a new group on the new host with its own retention and alert; the old group keeps its floor of two newest good snapshots (nothing new deletes them) and raises one `source_instance_gone` event. Groups are not merged across hosts. History under live data does not change.

`latest` follows the same split. In the API the key narrows a list only inside the mandatory `host`, and the same key on another host is another instance. In `lode` the newest signed snapshot of a key is chosen across every host of the repository, as `latest` for `files` is in 0.2 (whichever host made it). `lode` writes it `latest@<key>`: a colon already separates the path in `SNAPSHOT[:PATH]`, and `@` cannot occur in a canonical key.

### Retention, latest and the missing-backup alert per instance

Retention groups snapshots by (policy, host, source key). `files` is the host with an empty key. Inside a group the pools stay: scheduled, manual, and one stdin pool per stream name. The floor of two applies to each pool of each source. The source of a group is assigned by the server from the job and the signature; the agent does not pick it. The anomaly baseline of a snapshot is the history of its own group.

An instance that stopped receiving snapshots (a guest deleted, a VMID reused, a database recreated) gets the ordinary rules of the policy, and the floor keeps its two newest good snapshots indefinitely. There is no rule that deletes it. This is how a host that left the selector already behaves.

`latest` chooses among `files` snapshots by default. A named instance is chosen with `latest@<key>` in `lode` and with the `source` parameter in the API. A WAL batch (`postgres-wal`) is never `latest`: it is not a point-in-time image. A `lode` of this version in a repository without readable signatures (the legacy mode of ADR 0006) skips snapshots whose manifest claims a source other than `files`, and answers "no verified snapshot" for a named instance. A `lode` 0.2 does not know fields 18 and 6; in a repository that holds guest or database snapshots it chooses `latest` among all snapshots.

The missing-backup alert is raised per expected instance. For `files` that is the host. For other types it is the list that discovery supplies (phase 21); until it does, every instance of the policy that has snapshots is expected, so that an unknown list never silences the alert. The moment an instance stopped being expected (the first alert cycle in which the policy no longer selects it) is stored in `source_instances_gone`. From that moment the server raises exactly one `source_instance_gone` warning for 24 hours, whatever the time of the last success and whatever limit the policy sets. The row is removed when the instance is expected again, and a later departure opens a new event. The alert key of a `files` pair is unchanged from 0.2.

### Restore scopes and targets

There are four new scopes, independent of each other and of the existing ones: `restore:guest`, `restore:guest-in-place`, `restore:db`, `restore:db-in-place`. `restore:host` and `restore:in-place` do not include them, and tokens issued before do not get them. An admin session gets them as it gets every scope, and the in-place variants need a step-up by the existing rule. Restoring a guest is root on the Proxmox node; the threat model describes the risk. No endpoint uses these scopes in phase 20.

`RestoreSpec` gains `oneof target { GuestTarget guest = 8; DatabaseTarget database = 9; }`. The rule for compatibility: the server sends a guest or database target with an empty `destination` and `in_place = false`, so an agent that cannot run it fails on the existing check of the destination and creates nothing. A files restore of a snapshot of another type is refused with 422 by the server.

### Disk images

A disk image is a sparse file in the ordinary tree of a snapshot. It is split into segments of 256 GiB, and its chunks are fixed-size and aligned. A chunk of zeros is a hole, recorded in `sparse_extents`. There is no new object type and no `format_version` 2, so cleanup, `lode check`, find, the disaster-recovery path of `lode` and restore read an image as they read any file. The chunk size and the shape of the NBD export are provisional (below).

### Dirty bitmap binding

A snapshot of a VM that is a base for an incremental one records in its metadata what binds a dirty bitmap to it: the bitmap name (it contains the repository id and the policy id, so a bitmap of another repository or policy is never reused), its generation, the id of the parent snapshot, the size of the disk, and the VM uuid and `vmgenid`. A mismatch of any of them means a full read, never a trusted bitmap. The key names are fixed by phase 22 together with the writer.

### WAL batches

A `postgres-wal` instance is keyed by the cluster. WAL batches have their own pool. The server reads `backup_label` of the base backup and forgets WAL before that anchor; `.history` files are never forgotten.

## Provisional and deferred

Provisional, reversible until the first image is written:

- The chunk size of 4 MiB and the shape of the NBD export are confirmed by a spike on a nested PVE 9.2 at the start of phase 22, before any image is written to the format. Phase 20 writes none.

Deferred:

- `JobSpec.source` and a `source_ref` column in `jobs_coalesce_idx`: phase 22. Until then the job columns of the source are filled only by tests.
- Series of the `lodestore_backup_*` metrics per instance with a source label, and the last snapshot of a host by source: phase 22. Today the series cover `files`, and the last snapshot of a host is the newest of any source (`shortcut`).
- "Replaced" as a state separate from "gone", the list of expected instances and the cluster segment: phase 21, from discovery.

## Consequences

- `format_version` stays 1. The golden vectors that exist did not change; `source-vectors.json` is new.
- A server of 0.2 does not read policies of `lodestore.io/v1alpha2`, the only version that has `spec.source`. A policy of `files` is exported in the old version and byte for byte as before.
- An older `lode` counts a record whose source it cannot decode as unreadable, and reads a snapshot of an unknown type only by id.
- Known limit, to be closed by the discovery binding of phase 21: the server signs the instance key that the job names, and `lode latest@<key>` spans all hosts, so once non-`files` jobs exist a compromised host could claim the key of an instance of another host. Phase 20 has no such job (the server answers 422), and the API narrows `latest` to one `host`.
- The key of an instance and the layout of an image cannot change under data already written; a change needs a new ADR.
- A guest that migrates or fails over looks like a new instance, with a new retention history, until a later phase adds a way to say "the same guest". The old group is not deleted by this.

## Alternatives considered

- **Check the claim when a row is bound to the job.** The claim is not in the catalog; only the verification reads the manifest.
- **Identity by VMID.** A VMID is reused by a different guest; keeping its history would mix two guests.
- **A new object type for images, or `format_version` 2.** Cleanup, check and restore would need new paths; a sparse file in the tree needs none.
- **Merge groups across hosts.** It would make a retention decision depend on which hosts the server knows and could delete a history under a guest that moved.
