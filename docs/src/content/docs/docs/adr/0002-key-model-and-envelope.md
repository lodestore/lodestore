---
title: "ADR 0002: Key model and encryption envelope"
description: "The Lodestore key model: a master key, key slots for passwords and recovery keys, and the encryption envelope that lets a restore work from the bucket alone."
---
Status: Accepted
Date: 2026-10-13

## Context

Requirements that shape the key model:

- Everything needed to restore lives in the bucket. With the bucket and one secret (a password or a recovery key) a restore must work on a machine that has never seen the repository, with no server.
- A host that only backs up must not be able to read what it or any other host backed up. A stolen backup host must not expose history.
- Several secrets must be able to open the same repository, and a password must be changeable without re-encrypting data.
- Deduplication needs identifiers that two writers compute identically, yet that tell an observer of the bucket nothing about the content.

## Decision

### Key hierarchy

At `repo init` the client generates a random 256-bit **master key M** and a random 16-byte **repository id** (`repo_id`). Everything else is derived from M with `blake3::derive_key`; the context strings are fixed forever:

| Derived key | `derive_key` context | Purpose |
| --- | --- | --- |
| chunk id key `k_id` | `lodestore v1 chunk id key` | keyed BLAKE3 over plaintext gives blob ids |
| index key `k_idx` | `lodestore v1 index key` | encrypts index shards |
| server MAC key `k_srv` | `lodestore v1 server mac key` | authenticates server attestations; held by readers only, never by writer keys ([ADR 0005](/docs/adr/0005-server-attestations/)) |
| HPKE seed | `lodestore v1 hpke ikm` | input to the X-Wing key pair derivation |
| chunker seed | `lodestore v1 cdc seed` (applied to `k_id`) | first 8 bytes, forced odd, seed the FastCDC gear table |
| recovery KEK | `lodestore v1 recovery kek` (applied to the recovery key) | wraps M in a recovery slot |
| recovery checksum | `lodestore v1 recovery checksum` (applied to the recovery key) | first 3 bytes are the typo check |

Two more context strings are used as HPKE `info` prefixes, not for `derive_key`: `lodestore v1 pack key` and `lodestore v1 snapshot key`.

The X-Wing key pair is derived deterministically from M, so M alone is enough to reopen the repository, and the public key stored in `config` can be checked against what a slot unlocks.

### Envelope: who can read what

- **Blobs (file chunks and tree blobs)** are compressed with zstd, then sealed with **XChaCha20-Poly1305** (192-bit random nonce, 128-bit tag) under a **random per-pack key**. The per-pack key is generated fresh for every pack, so nonce count per key stays small and the 192-bit nonce removes the birthday limit of 96-bit nonces.
- **The pack key is sealed to the repository public key with HPKE** and stored in the first 1175 bytes of the pack itself (see ADR 0003). Only a holder of the private key, which is derived from M, can open it.
- **Snapshots** use the same construction: a random snapshot key, sealed with HPKE to the repository public key, stored in the envelope next to the snapshot body.
- **Index shards** are sealed under `k_idx`.

The HPKE suite is fixed by `config` and written as IANA code points:

- KEM `kem_id = 0x647a`: X-Wing (X25519 + ML-KEM-768), a hybrid KEM, so a future quantum attacker who records a bucket today still has to break X25519 as well, and a break of the young ML-KEM does not expose the data on its own.
- KDF `kdf_id = 0x0001`: HKDF-SHA256.
- AEAD `aead_id = 0x0003`: ChaCha20-Poly1305.

HPKE is used in single-shot mode, once per pack and once per snapshot: HPKE `info` is `context || repo_id`, with the contexts above.

The `hpke` crate is pinned **exactly** (`=0.14.1`). It is pre-1.0, X-Wing is a draft that has changed, and the deterministic key derivation from the seed is part of the on-disk contract: a version that derived a different key pair from the same seed would make every existing repository unreadable. Upgrading the pin requires a test that opens a repository created by the previous version.

### AAD binds ciphertext to its place

Every AEAD and HPKE operation authenticates `repo_id` followed by a type label, so an object cannot be moved to another repository or another role:

| Object | Key | AAD |
| --- | --- | --- |
| key slot (wrapped M) | KEK | `repo_id \|\| "key-slot"` |
| pack key | HPKE | `repo_id \|\| "pack-key"` |
| blob frame | pack key | `repo_id \|\| blob_type \|\| blob_id` |
| pack header | pack key | `repo_id \|\| "pack-header"` |
| index shard | `k_idx` | `repo_id \|\| "index"` |
| snapshot key | HPKE | `repo_id \|\| "snapshot-key"` |
| snapshot body | snapshot key | `repo_id \|\| "snapshot"` |

A blob frame's AAD contains the blob id, which is the keyed hash of the plaintext. After opening a frame the reader recomputes the keyed hash of the plaintext and compares it with the id, so a frame cannot be swapped for another blob.

### Key slots

`keys/<BLAKE3 of the slot bytes in hex>` holds a protobuf `KeySlot`: version, kind, KDF parameters, nonce, and M wrapped with XChaCha20-Poly1305 under a KEK. Slots are independent, so there can be many passwords and many recovery keys, and removing one never touches data.

- **Password slot:** KEK from **Argon2id** (version 0x13), default `m = 65536 KiB, t = 3, p = 4`, 16-byte random salt. The parameters are stored in the slot, so defaults can be raised later and old slots keep working. A reader refuses parameters above `m = 1 GiB (1048576 KiB), t = 16, p = 16`, so a hostile slot cannot make it allocate without bound. Those limits apply to one slot. A single opening also has a total budget: the sum of `m × t` over every slot it tries may not exceed `16777216` (`1 << 24`), the work of one slot with the largest allowed parameters, so a bucket full of slots that are each within the limits cannot keep a reader busy for an unbounded time. Only the slots the opening actually tries count. Opening with the password tries Argon2id password slots; slots of another kind, with another KDF, or with parameters above the limits above are not tried and cost nothing, and opening with a recovery key starts no Argon2 at all. When the budget would be exceeded by the next slot, the opening stops with an error that says the secret is wrong or the slot it opens was not tried, and gives the number of slots left untried (`lode` exits with code 12 as for a wrong password). An opening also reads at most 1024 key slots, in the order of the listing, and keeps none in memory; when no slot opens and the listing held more, the same error counts the slots past the first 1024 as untried and names both limits. Slots are tried cheapest first (memory times passes), so an honest repository with a few default slots stays far below the budget (a default slot costs `196608`), and an object under `keys/` that is misnamed or does not decode is skipped rather than failing the open. A reader downloads no key slot larger than 64 KiB and no `config` larger than 1 MiB; a larger slot is skipped and a larger `config` is refused.
- **Recovery slot:** the recovery key is 32 random bytes with full entropy, so no password KDF is needed. The KEK is `derive_key("lodestore v1 recovery kek", key)`.

The **recovery key text** is `lode-rk1-` followed by 14 groups of 4 base32 characters (RFC 4648 alphabet, no padding): 32 key bytes plus a 3-byte checksum, 35 bytes in all. Parsing is case-insensitive and ignores dashes and whitespace; the checksum is verified before the key is used. A secret that starts with `lode-rk1-` is treated as a recovery key by the CLI.

`config` is written last at init. Until it exists there is no repository, so an interrupted init leaves nothing that opens. If two inits race, whichever `config` lands first (a create-only write) wins; the loser's slots stay behind but cannot open the winner's repository, because their AAD names the loser's `repo_id`.

### Writer keys

A host that only backs up gets a **writer keys file**: the line `LODE-WRITER-KEYS v1`, then the base64 of a protobuf `WriterKeys` message (`repo_id`, `config_hash`, `k_id`, `k_idx`, and the HPKE suite with the public key). The file is created with mode 0600. It contains the BLAKE3 of the `config` it was exported for, and the client refuses to write if `config` has changed since.

### Threat model for writer keys

A holder of writer keys **can**:

- write new packs, index shards and snapshots (it has the public key, `k_idx` and `k_id`);
- deduplicate against the existing data, by reading the index (it has `k_idx`), which reveals blob ids, pack ids, offsets and sizes;
- **confirm a file**: with `k_id` it can compute the blob id of any content it already has and check whether the repository contains it. A writer that holds a file can learn that the same file is backed up.

It **cannot**:

- read blob contents, file names, directory structure, snapshots or their metadata: the pack keys and snapshot keys are sealed to a public key whose private half is derived from M, and M is not in the file;
- delete or overwrite anything through Lodestore, which only ever uses create-only writes and has no delete on the write path. Deletion is a separate trait that the backup code is never given.

### Residual risks

- **The chunker seed is 64 bits.** It hides chunk boundaries from someone who only reads the bucket, but 64 bits is not a long-term secret, and a writer-keys holder knows it anyway. The seed is a fingerprinting defence, not a confidentiality guarantee for file contents.
- **Overwrite by raw credentials.** A write-only S3 user could in principle overwrite an object in a way that bypasses Lodestore, if the bucket allows overwrites. Authentication catches tampering on read, but availability is lost. The mitigation is bucket-level: versioning and Object Lock, which later work configures and tests.
- **A forged index shard.** Shards are authenticated with `k_idx`, which every writer holds, and a writer does not check that the packs a shard names exist, so a compromised writer can publish a shard that claims blobs it never uploaded or declares that it supersedes real shards. Lodestore counts `supersedes` only from shards that the server has signed with a key writers do not have, opens the surviving copy of a blob before cleanup removes anything, and checks every new backup an agent makes on the server, which turns a snapshot that cannot be restored into a failed job. A client that can list `packs/` (opened with a password or recovery key) also ignores records whose pack is missing or has a different size. The mechanism is in [ADR 0005](/docs/adr/0005-server-attestations/), and what it covers and what it does not is in the [threat model](/docs/security/threat-model/#index-metadata-from-a-compromised-agent).
- **Restoring a malicious snapshot as root.** A compromised writer can add a snapshot that contains setuid files, device nodes or odd ownership. Restoring it as root recreates them. Restore to a scratch location, or as an unprivileged user, when the writer is not trusted.
- **`LODE_PASSWORD`** is visible to every process that can read the environment of the process that holds it. A password file or a password command avoids that.
- A password is only as strong as its Argon2id parameters; the recovery key does not depend on them.

## Consequences

- Restore needs only the bucket and one of: a password, a recovery key. M is recovered from a slot, and the rest follows.
- Rotating a password adds a slot and removes the old one; data is untouched.
- Rotating M itself would require re-sealing every pack key and snapshot key (not blob contents); the `key_epoch` field in `config` and snapshots is reserved for it.
- Per-pack random keys plus HPKE cost one KEM operation per pack and per snapshot on write, and one per pack on read (cached per pack within a run).

## Alternatives considered

- **AES-256-GCM with random 96-bit nonces under one long-lived key.** Birthday-bound collision risk at repository scale. Kept out; XChaCha20-Poly1305 has no such limit and is fast without AES hardware.
- **Classical X25519-only HPKE.** Simpler and widely implemented, but exposes the long-lived backup data to harvest-now-decrypt-later attacks. The hybrid KEM costs a larger encapsulation (1120 bytes per pack) that is negligible next to a 32 MiB pack.
- **A symmetric key shared with every backup host (restic style).** Any host could read all history. Rejected: it defeats the writer-keys model.
- **Age or PGP for envelopes.** Extra dependency and format for what HPKE does in a few calls, with less control over AAD binding.
- **Deriving per-pack keys from M instead of sealing random ones.** Would force every writer to hold M, which is exactly what writer keys must avoid.
