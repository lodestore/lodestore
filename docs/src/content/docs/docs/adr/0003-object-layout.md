---
title: "ADR 0003: Object layout, pack format and index"
---
Status: Accepted
Date: 2026-10-13

## Context

The bucket is the source of truth. The client must write with credentials that can only create objects, never overwrite or delete, and must work against both S3 and a local folder. Object stores list slowly, may list inconsistently, and bill per request, so the layout has to avoid listing on the restore path and read as little as possible per blob.

## Decision

### Bucket layout

```
config                       repository parameters (protobuf Config), fixed name
keys/<hex>                   key slots (ADR 0002)
packs/<2 hex>/<hex>          packs
index/<hex>                  index shards
snapshots/<hex>              sealed snapshot envelopes
locks/gc-<32 hex>            GC manifest: packs marked for deletion
locks/writer-<32 hex>        marker of a running direct writer
locks/writer-<32 hex>.done   empty; the writer has finished
```

Every object except `config` and those under `locks/` is named by the lowercase hex **BLAKE3 of its exact bytes** (64 characters). A pack lives under a two-character fan-out directory taken from the first two characters of its name. Because names derive from content, a second write of the same object is a no-op, and "already exists" on a create-only write means the identical object is already there.

All writes are create-only (`put_if_absent`). The write path has no overwrite and no delete.

### Write order

An object is written only after everything it refers to exists, so a reader never meets a dangling reference, and an interrupted run leaves only unreferenced garbage:

- **init:** key slots, then the directory layout, then `config` last. A folder without `config` is not a repository.
- **backup:** packs first; an index shard only after the packs it lists; the snapshot last, after all of its shards.

### `locks/`

The objects under `locks/` coordinate garbage collection with writers. Their names are random 16-byte ids in lowercase hex, not content hashes (the same exception as `config`), and their time is the store's `LastModified`, which a writer cannot forge into the past.

- `locks/gc-<id>` is a protobuf `GcManifest` (`version`, `gc_id`, `published`, `candidate_packs` sorted) sealed with XChaCha20-Poly1305 under `k_idx`, additional data `repo_id || "gc-manifest"`. The server publishes it before it deletes anything. A writer reads every manifest when it opens the repository and ignores the records of the listed packs, so it uploads a blob again unless a second location outside the candidates holds it. A manifest that does not authenticate, has an unknown version or whose `gc_id` differs from its name stops the writer.
- `locks/writer-<id>` is a protobuf `WriterMarker` (`version`, `writer_id`, `hostname`, `started`) sealed the same way with additional data `repo_id || "writer-marker"`. A direct writer creates it before it uploads and creates the empty `locks/writer-<id>.done` when it finishes. A marker without `.done` blocks deletion; one that does not open still counts as open.

All three are create-only like every other object, and the write path never deletes them.

### Pack

```
prefix || blob frame* || header frame || trailer
```

- **Prefix, 1175 bytes:** magic `LODEPK` (6) | version (1, currently 1) | HPKE encapsulated key (1120 for X-Wing) | sealed pack key (48 = 32 key + 16 tag). The pack key is sealed to the repository public key (ADR 0002), which makes every pack self-describing for an index rebuild.
- **Blob frame:** XChaCha20-Poly1305 over `codec || payload`, as `nonce (24) || ciphertext || tag (16)`. `codec` is `0` for raw bytes and `1` for zstd. A chunk is stored raw when zstd saves less than 5%.
- **Header frame:** the same AEAD over the concatenated entries (below), under the pack key.
- **Trailer, 10 bytes:** header frame length as `u32` little-endian, then the magic `LODEPK` again. A reader fetches the last 10 bytes, learns where the header starts, and fetches it with a second ranged read.

An entry is **45 bytes**, little-endian, sorted by blob id inside a pack:

| Field | Size |
| --- | --- |
| blob id | 32 |
| blob type (`1` data, `2` tree) | 1 |
| offset of the frame in the pack, `u32` | 4 |
| frame length, `u32` | 4 |
| plaintext length, `u32` | 4 |

Rules:

- A pack holds blobs of one type. Data and tree blobs never share a pack, and a blob is never split across packs.
- Target pack size is 32 MiB; a pack is never larger than 64 MiB.
- A decompressed blob is at most 4 MiB for data and 16 MiB for tree blobs. The declared plaintext length is checked against these limits before anything is allocated, and it is the exact output capacity of decompression, so a forged frame cannot inflate beyond it.
- Chunks come from FastCDC v2020 with minimum 256 KiB, average 1 MiB, maximum 4 MiB, normalization level 1, seeded per repository (ADR 0002).

### Trees

A directory is a protobuf `Tree` of nodes sorted by name, stored as a blob of type tree. A snapshot's `root_tree` is the blob id of the root. A directory whose serialized nodes exceed about 1 MiB is cut into several tree blobs, and its node lists them in `subtrees`, in order; the reader concatenates them and checks that names stay in order across the cuts. A file node lists its data blob ids in `content`, in file order.

### Snapshot envelope

`snapshots/<hex>` is a protobuf `SnapshotEnvelope`: version, HPKE encapsulated key, sealed snapshot key, nonce, ciphertext (ADR 0002). Its name is the BLAKE3 of the envelope bytes, which is also the snapshot id.

### Index

An index shard is an AEAD-sealed (key `k_idx`) protobuf `IndexShard`: a list of pack records, each with the pack id, the HPKE encapsulated key and sealed pack key copied from the pack prefix, the pack size, and the pack's entries (the same 45-byte records, concatenated). The in-memory index maps blob id to (pack, offset, frame length, plaintext length, type). The first record seen for a blob id wins; duplicates across shards are harmless.

`IndexShard.supersedes` lists the ids of shards that the new shard ranks below every shard not listed anywhere. It is a preference, never a reason to lose a location: a reader uses a superseded shard's record of a blob only while no current shard places that blob, whatever order the shards are read in. Any writer key can seal a shard, so a shard that supersedes everything hides nothing. A collector compacts the index in this order: it writes the new shards with an empty `supersedes`, reads the index back without the shards it is replacing and checks that every live blob is still placed in a listed pack, then writes one shard without records whose `supersedes` names the replaced shards. Only then does it delete the replaced shards, the ones it chose itself, and after them the packs. A compaction cut short leaves duplicate records and nothing missing. The golden vectors in `tests/fixtures/v1/index-vectors.json` fix this resolution.

A reader checks, before trusting a record, that the entry lies after the prefix and inside the pack size and that the declared plaintext length is within the limit for its type. Plaintext is then verified against its keyed hash after decryption, so a wrong index can cause an error but never wrong data.

### `index_refs` and independence from LIST

A snapshot carries `index_refs`: the ids of the index shards that cover every blob it uses. Restore loads those shards by their exact keys and never lists. Listing `index/` is only a fallback, used when a snapshot's shards are missing, and a writer's discovery step. A shard that LIST fails to return costs a writer some deduplication and costs a reader nothing.

### Rebuilding the index

The index is a cache; the packs are the truth. `lode repo rebuild-index` lists `packs/`, skips packs that some shard already mentions and packs a running cleanup has marked for deletion in `locks/gc-*`, and for each remaining pack fetches the first 1175 bytes, the last 10 bytes and the header frame with three ranged reads. It opens the pack key with the repository private key, decrypts the header, checks every entry against the pack's bounds, and writes new shards with at most 1024 pack records each. A pack that cannot be parsed is skipped with a warning and the command exits with code 1; the others are still indexed. A second run finds nothing to do and writes no shard.

The rebuild does not hash a pack to compare it with its name: that needs a full download and belongs to integrity verification. A tampered pack is still caught when a blob is read, by authentication and by the keyed-hash check.

### Limits and checks of the tree decoder

A snapshot may come from a writer that holds the writer keys and is not trusted, so a reader checks every tree before it touches the restore target:

- A node name is 1 to 1024 bytes, is not `.` or `..`, and contains neither `/` nor NUL. 1024 is above the `NAME_MAX` of common Linux filesystems, so snapshots from other systems stay readable.
- Names inside a directory are strictly ascending, also across the pieces of a split directory.
- Directories nest at most 4096 levels deep. The walk uses an explicit stack, so depth costs memory, not call stack.
- A node carries only what its type allows: content ids, sparse extents and a hardlink group only on files, subtrees only on directories, a target on every symlink. Every content and subtree id is 32 bytes.
- The chunks of a file add up to its size (or, for a sparse file, to the sum of its extents).
- The members of one hardlink group agree in size and content ids. A writer that sees a file change between the reads of two of its names stores the later one as a separate file and reports a warning.

A file node lists at most (16 MiB - 2 MiB) / 34 data blob ids, about 430 thousand chunks or 430 GB at the average chunk size, so that its tree blob stays within the 16 MiB limit. A larger file is skipped with the `TOO_LARGE` issue. Lifting the ceiling needs an indirection for the content list and so a higher `min_reader_version`.

### Frozen fixtures

`crates/lodestore-format/tests/fixtures/v1` holds a repository written by this format (two snapshots, password and recovery key slots, a writer-keys file) and the frozen outputs of its primitives (`vectors.json`). Tests read, restore and re-encode them on every run, so a change that old repositories would not survive fails the build. The directory is append-only: CI rejects a change that modifies, deletes or renames a file in it. A change to the format needs `format_version` 2 and a new `fixtures/v2` next to this one.

## Consequences

- Restore costs one ranged read for a pack prefix per pack touched, one for a frame per blob, and no LIST in the normal case.
- Packs of 32 MiB keep the object count low for S3 while keeping a ranged read per blob cheap.
- The index can be lost or thrown away without losing data; the price is reading three ranges per pack.
- Offsets and lengths are `u32`, which is why a pack is capped at 64 MiB, far below 4 GiB.
- Because tree and data blobs live in different packs, listing the history of a repository touches few, small tree packs.
- Garbage (packs or shards from an interrupted backup) accumulates until a server-side collection removes it; the client never deletes.

## Alternatives considered

- **One object per chunk.** Simple, but a 100 GB backup becomes 100 thousand objects with per-request cost and latency on S3.
- **Packs encrypted under a key derived from the master key.** Every writer would need the master key; the sealed pack key in the prefix avoids that and keeps packs self-describing.
- **Index as a single mutable file.** Needs overwrite, which the write path must not have.
- **Relying on LIST to find the index.** Eventually consistent listings make restore flaky; `index_refs` removes the dependency.
