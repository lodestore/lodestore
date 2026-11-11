---
title: "ADR 0001: Serialization of repository objects"
description: "Why Lodestore repository objects use Protocol Buffers, so a third-party restorer in another language can read a repository long after this implementation."
---
Status: Accepted
Date: 2026-10-13

## Context

A Lodestore repository is a set of immutable objects in a bucket or a folder. Everything that describes a repository (`config`, key slots, index shards, snapshots, trees) must be:

- readable by a third-party restorer written in another language, because the data has to outlive this implementation;
- extensible without breaking existing repositories, because a repository written today is read by builds that do not exist yet;
- hashable: object names are the BLAKE3 of the exact bytes written, so what is written must be what is hashed.

## Decision

**Protobuf, proto3, for every structured object.** The schema is `crates/lodestore-format/proto/lodestore/format/v1/format.proto`. Code is generated at build time with `prost` 0.14.4 and compiled with `protox` 0.10.0, a pure-Rust protobuf compiler, so no `protoc` is needed on any build machine.

Rules the schema follows, stated at the top of the `.proto` file:

- Field numbers are never reused or renumbered. A retired number is `reserved`.
- No `map` fields and no floating-point types. Time is `(seconds: int64, nanos: uint32)`; sizes are unsigned integers.
- Paths, file names, link targets, xattr names and values, user and group names, and host names are `bytes`, never `string`. Unix names are byte strings and are not always UTF-8. Free-form text produced by Lodestore itself (tags, messages, program version) is `string`.
- Repeated fields are written in an order chosen by the writer and fixed by the rules of the object: tree nodes sorted by name, index records sorted by blob id.
- **The hash is taken over the exact bytes written.** An object name is `BLAKE3(bytes)`. A reader hashes the bytes it fetched and never re-encodes a decoded message to verify it. Encoding therefore does not need to be canonical across implementations; it only needs to be stable for the bytes already stored.

**Fixed binary framing where protobuf does not fit.** Packs and index records are not protobuf. A pack is `prefix || blob frame* || header frame || trailer`, and the pack header and the `entries` field of an index record are arrays of fixed 45-byte records (see ADR 0003). Fixed widths allow ranged reads of a pack's prefix, trailer and header without parsing anything, and binary search over an index later without a format change.

**Evolution.**

- Adding an optional field is compatible. proto3 decoders skip unknown fields, so older readers keep working. Older writers drop nothing because they never rewrite an existing object.
- Changing the meaning of an existing field, or anything else an old build would misread, is not compatible and must be announced in `config`:
  - raise `min_reader_version`: builds that do not support that version refuse to open the repository at all;
  - raise `min_writer_version`: old builds may read but refuse to write;
  - add a string to `incompat_features`: builds that do not know the feature refuse to read or write. The feature changes what an object means, so a build that read the repository anyway would misread it.
  A build checks these on open, before it touches any other object.
- `prost` is pinned to the minor version in `Cargo.toml`, and a golden test that re-encodes fixed messages and compares the bytes must guard against an encoder change before the format is declared stable. Existing objects are never rewritten, so a change of encoder affects only new objects, but a silent difference in field order would still make two builds produce different names for the same logical object and weaken deduplication of tree blobs.

## Consequences

- A restorer in any language needs a protobuf runtime, the `.proto` file, XChaCha20-Poly1305, HPKE with X-Wing, BLAKE3 and zstd, plus the fixed layouts in ADR 0003.
- Byte-exact hashing keeps verification independent of the implementation: nothing needs to agree on a canonical form.
- Names that are `bytes` cost a conversion at the edges (the CLI prints non-UTF-8 paths as base64 in JSON), but restore reproduces them exactly.
- Two builds may encode the same message to different bytes only if their encoders differ in field order or varint handling. That is a deduplication loss, never a correctness problem.

## Alternatives considered

- **Canonical CBOR (RFC 8949 deterministic encoding).** Gives canonical bytes by definition, but needs a schema layer on top, has no first-class unknown-field tolerance for evolving structs, and the Rust and other-language tooling for schemas and codegen is weaker. The deterministic form is only valuable if something re-encodes before hashing, which this design avoids by hashing the written bytes.
- **JSON.** Not binary-safe for names, large, and floating-point and ordering questions leak into the format.
- **A hand-written binary format for everything.** Compact, but every extension is a breaking change and a third-party restorer has to reimplement every parser by hand.
- **`serde` with bincode or postcard.** Tied to Rust types and to field order, with no cross-language story.
