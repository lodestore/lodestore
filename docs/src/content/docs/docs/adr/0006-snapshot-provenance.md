---
title: "ADR 0006: Snapshot provenance and latest"
---
Status: Accepted
Date: 2026-11-05

## Context

A snapshot manifest is written by the host being backed up. Its `time`, `hostname` and `agent_id` are claims of that host, and a compromised host holding writer keys can plant a snapshot that names any host and carries any time, including one far in the future. If `latest` follows the manifest, that snapshot is what `lode restore latest` restores after an incident, and what "restore to the original host" offers.

ADR 0005 reserved `Attestation.sealed_provenance` for records only the holder of the master key can create. This ADR fills it. The repository format does not change.

## Decision

### What verified means

A snapshot is verified when a MAC-valid attestation holds a sealed `ProvenanceRecord` that opens under the provenance key and names the snapshot id. Attestations that fail the MAC are ignored, so a planted `locks/attest-*` object never makes a snapshot verified. A record that does not open is skipped and counted.

If several records name one snapshot they collapse to one. A server record beats the owner's; among records of one kind the earlier `verified_at` stays; on a tie the first one read stays.

### Who writes records

- The server, after it has checked a finished backup: the host comes from the agent's mTLS certificate and the job from its own catalog. A host cannot claim to be another host.
- The server, once, on the first start of 0.2 on a repository: snapshots its catalog already ties to a host and a job get a record; every other snapshot stays unverified. The server trusts the state of the repository at the moment of the upgrade.
- `lode backup` with the repository password, after it has checked its own snapshot: a record with empty `host_id` and `job_id`. An empty pair means the holder of the password signed it (`Signer::Owner`); a non-empty one means the server (`Signer::Server`).

### How `latest` is chosen

The order never comes from the manifest, except as a last resort in the legacy mode below.

- **Strict**, when at least one MAC-valid attestation exists. Only verified snapshots compete. The key is the time the store reports for the snapshot object (`LastModified`), and the record's `verified_at` when the store does not report one. Equal keys are broken by snapshot id, the larger wins, so the result does not depend on listing order. A verified snapshot whose key is more than ten minutes ahead of the clock is skipped with a warning. With no verified snapshot left the command fails and asks for an id.
- **Legacy**, when no MAC-valid attestation exists at all (a 0.1 repository the 0.2 server has not opened yet). The key is `LastModified`, or the manifest time when the store does not report it, and the command warns loudly. A snapshot whose key is more than ten minutes ahead of the clock is never chosen; if every snapshot is, the command fails and asks for an id. A caller that knows the repository was upgraded before (the local mark `lode` keeps) asks for `expect_upgraded`, and with no readable attestation it refuses instead of using the legacy order.

Attestations are read completely or not at all: when an object disappears between listing and reading (a consolidation running at the same time), the read is retried and then fails, so a partial set never switches the mode or drops records.

Both modes warn about manifests that claim a time more than ten minutes ahead of the clock. Strict also warns about unverified snapshots stored after the chosen one, and about records that did not open.

### Consolidation

Consolidating attestations carries the provenance of snapshots that still exist in `snapshots/`, one record per snapshot, and drops the rest. It cuts the records into parts so that no object exceeds the attestation size limit, and it leaves out shards the server has revoked. The listing of `snapshots/` happens after the attestations are read; a record is written only after its snapshot object, so no record of an existing snapshot is lost.

### Format

`format_version` stays 1. `ProvenanceRecord` and `sealed_provenance` were frozen with ADR 0005 and `attest-vectors.json`; no byte layout and no test vector is added or changed.

## Consequences

- The server and `lode` make the same decision, because the rule lives in `lodestore-format`.
- On a local-folder repository the agent can write `LastModified` (file mtime). Strict mode does not depend on it for who is verified, only for the order among verified snapshots: an agent cannot create a verified snapshot, but with file permissions it can reorder the verified ones by touching their files, though not into the future.
- A repository with no attestation stays in legacy mode. That mode relies on the store's clock and refuses snapshots stored in the future, but a folder repository can have an old mtime set on a planted file.
- The upgrade mark lives inside the attestations. Deleting them removes the mark from the bucket, so only a client that saw the upgrade before notices (`expect_upgraded`); `lode backup` also refuses to run on such a machine. A machine without that memory, reading a folder repository whose attestations were deleted, falls back to legacy mode. This limit is stated in the threat model.
- Unverified snapshots stay restorable by explicit id.

## Alternatives considered

- **Sign the snapshot object itself.** The writer would need a signing key, and a compromised writer would hold it.
- **Order by `verified_at` only.** It is a clock of the server, but it would hide snapshots the server has not yet checked from `latest`; the store's time orders them at no cost.
- **Treat any `locks/attest-*` object as the switch to strict mode.** A planted object would then break `latest` for everyone.
