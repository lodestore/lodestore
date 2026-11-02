---
title: "ADR 0004: Restore data path"
---
Status: Accepted
Date: 2026-10-16

## Context

Backups go from the agent straight to the bucket, and the server only manages and catalogs. A restore has to bring data back to a chosen host, which may be the original one, another host of the fleet or a host that belongs to a different repository. The server alone holds the master secret and can read the snapshot trees. An agent holds a write-only S3 key and cannot read packs.

The data path of a restore must keep three properties: the server stays out of the bulk transfer, the agent learns as little as it can about the rest of the repository, and nothing in the path requires the agent to read the bucket with its own key.

## Decision

### The server plans, the agent restores

The server opens the snapshot, walks the selected subtrees with its own keys and gives the agent a **restore plan**. The agent fetches the encrypted frames, opens them, checks every chunk against its keyed hash and writes the files with the same restorer the command line uses.

- **The plan is a pull stream.** The agent calls `RestorePlan(job_id)` on its own channel and reads batches of items (enter a directory, an entry, its chunks, leave a directory). The stream is bounded, so the server walks no faster than the agent consumes, and one batch is at most 1 MiB. A file with a very long chunk list is continued in further items. The final batch is marked `last`; a stream that ends without it counts as broken. If the stream breaks, the agent asks for the plan again and starts over; because restoring a file is an atomic replace of identical content, a second pass is safe. The created, overwritten and skipped counters of the last pass are reported; errors and warnings add up over all passes.
- **A separate RPC, not the `Session` stream.** Plans and ciphertext are large and would delay heartbeats, cancel messages and job reports on the long-lived session. A dedicated call keeps that stream light.
- **The agent receives the plan of the selection only,** the keys of the packs the plan uses, and the `repo_id` and `k_id` of the source repository. It never receives the master secret, whole directory trees or snapshot keys.

### Pack access

- **S3 and other stores that can sign:** each batch carries, for the packs it uses, the pack key and a presigned `GET` URL valid for one hour. The agent reads frames with `Range` requests and does not go through the server. When a link is near expiry or refused, `RefreshPackAccess` returns fresh URLs for packs already issued to the job.
- **Local folder and other stores without signing:** the agent calls `ReadPackRange(job_id, pack, offset, length)` and the server relays the ciphertext in parts of at most 1 MiB. It reads the whole range before answering, so a bad range is an RPC status and not a broken stream. There is **no short circuit** for a repository on the agent's own machine: the local path uses the same RPC, so what is tested for one host is what runs for all.
- Nearby ranges of one pack are merged into one read (up to 8 MiB, gaps up to 64 KiB), which keeps the request count low on S3.
- The server records which packs it issued to each job. A refresh or relay request for any other pack, from any other agent, or for a job that is not running is refused with `PermissionDenied`.

### Compatibility

The protocol version stays as it is. The new messages and the new call are additive, and an agent states what it can do in `Hello.capabilities`. A restore job is assigned only to an agent that lists `restore_v1`; an agent without it fails the job with `restore_agent_outdated` and the job does not hang waiting for an assignment an old agent would ignore.

### Jobs on one host run in turn

A host runs one file backup or restore at a time. A restore queued behind a running backup waits in the queue and starts afterwards, and an offline target keeps the job queued until its agent connects (or the job expires). Cancel works while a restore runs: the agent stops between spans and removes its temporary files.

### The one exception: download as an archive

Downloading a selection as an archive does not use an agent. The server decrypts the data itself and streams the archive to the browser without temporary files. This is the only path on which the server reads user data in plaintext, and it is initiated by a signed-in person.

## Consequences

- Bulk data of an S3 restore flows from the bucket to the agent. The server computes plans and signatures and does not carry the bytes.
- For stores without signing, the server carries ciphertext to the agent. It is not decrypted there, but the server's network is on the path.
- The agent learns the paths and metadata of the selection, the keys of the packs it uses and the `k_id` of the source repository. The residual leak is that a pack key opens the neighbouring chunks of its pack. [The threat model](/docs/security/threat-model/) records this as an accepted risk.
- The target of a restore can be any enrolled host, including a host of another repository, because the plan carries the source repository's `repo_id` and `k_id`.
- A broken plan stream costs a restart of the pass, not an error, up to three restarts.
- A host that is offline or busy delays its restore and nothing else.

## Alternatives considered

- **Server streams the data to the agent.** Simple, and works for every store, but the server then carries every restored byte and becomes the bottleneck of a fleet restore.
- **Give the agent a read-only S3 key.** The agent could then read the whole bucket with it at any time, which cancels the write-only property that limits a compromised host.
- **Send the agent the directory trees and snapshot keys and let it plan.** Fewer server round trips, but a compromised host would learn the whole snapshot and hold keys with a long life.
- **Carry the plan inside the job assignment.** A large selection would not fit one message, and a batch of presigned URLs would outlive its use.
- **Raise the protocol version.** It would lock out agents that still run backups correctly; capabilities achieve the same without a flag day.
- **Read a local repository directly when it sits on the agent's host.** It would skip the relay in one case and leave that path untested for every other.
