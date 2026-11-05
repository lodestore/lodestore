---
title: "ADR 0005: Server attestations"
---
Status: Accepted
Date: 2026-11-04

## Context

Every writer holds `k_idx` (ADR 0002), so any holder of writer keys can seal an index shard that claims any blob and supersedes any other shard (ADR 0003). The agent is the least trusted party in the system: it runs as root on a host that can be compromised, and it has no way to tell a legitimate shard from a planted one. The server, which holds the master key, is the only party that can be given a root of trust the agent does not have.

The format needs an object that only the server can create, that survives in the bucket without the server, and that older readers do not trip over. The 0.1 readers open only `locks/gc-*` and `locks/writer-*` and skip other names under `locks/`.

## Decision

### The server MAC key

`k_srv = derive_key("lodestore v1 server mac key", M)`, where M is the master key. It exists only in `ReadKeys`. `WriteKeys`, the writer-keys file and everything the agent receives carry neither `k_srv` nor anything it can be derived from: the agent holds `k_id` and `k_idx`, and derivation runs from M, so a writer-keys holder cannot compute it.

### The attestation object

An attestation is a create-only object named `locks/attest-<32 hex of a random attest_id>`. It is written with the same create-only write as every other object and is compatible with Object Lock. Its bytes are

```
0x01 || MAC (32 bytes) || protobuf Attestation
```

The MAC is keyed BLAKE3 under `k_srv`. Its input, in order, is the domain `lodestore v1 attestation`, the `repo_id`, the frame byte, the body length as a little-endian `u64`, and the body. A reader compares the MAC in constant time and rejects an unknown frame byte. The MAC binds the repository, so an object copied from another repository does not verify. The `attest_id` inside the body is under the MAC and must equal the name of the object, so an object cannot be renamed.

`Attestation` carries:

- `attest_id`: 16 random bytes, the name.
- `created`: when the server wrote it.
- `shard_ids`: 32-byte ids of index shards, in the clear and sorted. An agent can read them without any key and uses them only as a filter for deduplication. Nothing that decides a server check, a collection or a read may rely on them without verifying the MAC.
- `markers`: repository-level facts, in the clear.
  - `1`: the repository upgrade is complete. It sits on its own last upgrade object, with an empty `shard_ids`, and every part of a consolidation carries it too.
  - `2`: a part of an upgrade. It names the shards that existed when the server first opened the repository. The server finishes an interrupted upgrade by running it again. A repository that has MAC-valid attestations of other kinds but no attestation with marker `1` shows that the completion object was removed; the server then refuses to sign the repository again.
- `sealed_provenance`: repeated AEAD blobs under `derive_key("lodestore v1 attestation seal key", k_srv)`, additional data `repo_id || "lodestore v1 provenance"`. The field carries the provenance records of snapshots (`ProvenanceRecord`: snapshot id, host id, host name, job id, verification time), filled as described in [ADR 0006](0006-snapshot-provenance.md) without a format change. `seal_provenance` and `open_provenance` froze the schema. A holder of writer keys cannot read the host or job.

### What an attestation means

The server trusts the records of an attested shard when it applies `supersedes` and when it collects garbage. An attestation does not mean the server opened the frames of that shard's packs. A check of a finished backup relies only on shards whose frames the server opened itself; the server keeps that list, and it is not a reading of the attestation objects. After such a check the server attests the host's shards it opened, except a shard that lists `supersedes`: opened frames do not vouch for that list.

`format_version` stays 1. A new object kind under `locks/` does not change how any existing object is read, and readers that do not know the prefix skip it.

### Why `locks/`

The agent's bucket policy already lets it read under `locks/` (it reads the cleanup manifests before a backup) and does not let it write there, so an attestation is readable for the dedup filter and not forgeable by IAM. The 0.1 code opens only `locks/gc-` and `locks/writer-`, so it ignores the new names.

### What each side checks

The server and `lode`, holding the master key, read every `locks/attest-*` object, drop those whose MAC does not verify (they are reported and ignored, because a planted object must not stop the server; a cleanup cycle then deletes them), and use the rest. The agent cannot verify a MAC. For the agent an attestation is a hint for deduplication; the boundary of trust is the server-side check and the collector, described in the threat model.

On a local-folder repository the agent can write into `locks/`. The MAC stops it from forging an attestation, but it does not stop deletion of attestations or re-planting of old ones. The consequences, and the server's refusal to sign again a repository that has attestations without marker `1`, are described in the threat model.

### Frozen fixture

`crates/lodestore-format/tests/fixtures/v1/attest-vectors.json` fixes `k_srv`, the provenance sealing subkey, a MAC over a fixed body, a full attestation frame and sealed provenance bytes for fixed inputs. It is a new file next to the existing vectors, which do not change.

## Consequences

- Only a holder of the master key writes and verifies attestations; a writer-keys holder can add objects under `locks/attest-` but they never verify.
- The format of the attestation is part of the repository format from this version on; the context strings and the frame layout are fixed forever.
- A repository that is never upgraded has no attestations, and the 0.1 behaviour is unchanged.
- The cost is one small object per upgrade or consolidation step, listed with the other lock objects.

## Alternatives considered

- **An asymmetric signature.** It would let an agent verify, but the verifier would then hold a long-lived public key that cannot be rotated without rewriting every attestation, and an agent that can verify still cannot make a decision the server must make anyway.
- **A new top-level prefix such as `attest/`.** It needs a change of the agent's bucket policy and a new rule for every store; `locks/` already has the access split that is needed.
- **Mutating `config` or an index shard.** It needs overwrite, which the write path does not have.
- **Hiding `shard_ids` as well.** The agent's dedup filter needs them without a key, and hiding them would add nothing to the server's check.
