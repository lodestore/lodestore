---
title: "Cleanup: freeing storage space"
---
Retention only forgets snapshots. Their packs stay in the bucket until a cleanup cycle deletes what no remaining snapshot uses. Cleanup is run by the server, with the server's own storage key. Agents and `lode` cannot delete anything.

Cleanup is in beta and **off by default**. A dry run is always available, also while cleanup is off, and never writes to the bucket.

Where to find it:

- Web interface: the **Free up space** card of the repository.
- Terminal: `lode gc`, see [Command line](/docs/reference/cli/#cleanup).
- API: the `Cleanup` group of the [API](/docs/reference/api/).

## What a cycle does

When it first opens a repository, and in any case before the first cycle, the server signs the index shards that already exist with a key that agents do not have, and writes the signature as `locks/attest-*` objects. Only signed shards are allowed to say that they replace others. See [Index metadata from a compromised agent](/docs/security/threat-model/#index-metadata-from-a-compromised-agent).

1. **Check snapshots.** The server reads every snapshot and the whole index and works out which packs hold data no snapshot uses. A snapshot that cannot be read or that points to data which is missing is put in quarantine and stops the cycle before anything is deleted: the server cannot know what it references. A snapshot that is read in a size other than the one the listing gave is not put in quarantine: the cycle stops with a store error, as for any object that changed while it was read, and the next cycle looks at it again. See [Quarantined snapshots](#quarantined-snapshots). An index shard that is larger than a shard may be (see below) is left out and listed in the report. Packs younger than the pause are never touched.
2. **Rewrite packs.** A pack with less live data than the repack threshold (default 50%) is rewritten: its live blobs are copied into new packs and the new index shard is written before anything is deleted. At most `Max repack per cycle` (default 10 GiB) is moved per cycle; the rest waits for the next one. Before a pack is rewritten or deleted, the server opens the copy of every live blob that would remain and checks that it is that blob. A pack whose data does not open, or that cannot be read, is left alone and the next cycle looks at it again, so false index records cannot make the server delete data that a snapshot needs.
3. **Mark.** The server publishes the list of packs it plans to delete as an object `locks/gc-<id>`. From now on every backup, whether run by an agent or by `lode backup`, reads that list at the start and uploads again any blob that lives only in a marked pack.
4. **Pause.** Nothing is deleted for the length of the pause, at least 24 hours. Backups that started before the mark must finish within this time.
5. **Check again.** After the pause the server reads all snapshots again, including those created during the pause. A marked pack that a new snapshot needs is taken off the list. Deletion also waits while a restore or download of the repository is running, and while a backup that started before the mark has not finished (see [Unfinished backups](#unfinished-backups)).
6. **Delete.** The server writes a new, smaller index without the marked packs, checks that every blob of every snapshot is still found, and only then deletes the old index shards, the marked packs and last the `locks/gc-<id>` object. When it compacts the index it also merges the `locks/attest-*` objects into one, without the shards the server has revoked and without the records of snapshots that no longer exist (see [Attestations and cleanup](#attestations-and-cleanup)). Up to the first deleted object a cycle can be aborted without any loss. After that it cannot, and a cycle that stops continues where it stopped on the next run.

The first cycle after an update can have more to do. Index shards written by 0.1 have no size limit, and one above 64 MiB, up to 1 GiB, is rewritten by this cycle into shards under the limit, a step that runs even when there is nothing to delete. The cycle therefore goes through the pause in full before it removes the old shard. Readers do not load an unattested shard above 64 MiB, and an attested shard only up to 1 GiB; a shard above 1 GiB is skipped and reported, and `lode repo rebuild-index` replaces it. Every writer of this version cuts shards at 32 MiB and 1024 pack records, so later cycles have nothing to split.

The server also checks every new backup an agent makes, outside the cycle. See [Backups the server could not verify](#backups-the-server-could-not-verify).

`lode gc` stops at step 3 and prints when deletion happens. Follow the rest with `lode gc status`.

## Turning cleanup on

Open the repository, turn on **Scheduled cleanup** and save. The server refuses while one of these safety checks fails and shows what to fix:

| Check | What to do |
|-------|------------|
| Old agents | Some host runs an agent that does not know the cleanup protocol. Update it or revoke it. Revoked agents are ignored. |
| Server key | The server's storage key cannot delete objects. See [Server key](#server-key). |
| Agent key | The agent key cannot read `locks/`. See [Agent key](#agent-key). |
| Another cleanup | A `locks/gc-*` object in the bucket was not made by this server. See [Lost server database](#lost-server-database). |
| Unreadable storage | The server cannot open the repository or read its `locks/` folder; the message names the cause. A snapshot that does not open is not a failed check: the cycle puts it in quarantine, see [Quarantined snapshots](#quarantined-snapshots). A file under `snapshots/` whose name does not match its content is not a snapshot; it is ignored with a warning. |

The settings:

| Setting | Default | Meaning |
|---------|---------|---------|
| Schedule (`cron`, time zone) | `0 3 * * 0`, UTC | When a cycle starts by itself. A cycle can also be started by hand at any time. |
| Pause before deletion | `24h` | Time between mark and deletion. Not below the floor, see below. |
| Repack below | `50%` | A pack with less live data than this is rewritten. |
| Max repack per cycle | `10GiB` | Upper limit of the data moved to repack in one cycle. |

**The pause floor.** The pause must be longer than any backup can run. Every backup has a time limit, `limits.maxRuntime` of its policy (default `12h`, see [Backup policies](/docs/guides/policies/#limits)). The pause is never below the larger of 24 hours and the longest `maxRuntime` among the policies of the repository plus one hour. The interface shows the current floor. Raising `maxRuntime` in a policy raises the floor, and the pause is checked against it whenever you save the settings.

The time limit counts everything the job does, including `pre` and `post` hooks: the clock starts before the first hook runs. A long hook leaves less time for the backup itself.

## Server key

The server deletes with its own key. On AWS S3 the key needs, besides the rights it already uses to read and write the repository:

- `s3:DeleteObject`
- `s3:DeleteObjectVersion`
- `s3:ListBucketVersions`
- `s3:GetObjectVersion`

On a bucket with Object Lock add `s3:GetObjectRetention`, `s3:GetObjectLegalHold` and `s3:GetBucketObjectLockConfiguration`. Without them cleanup still works, but it cannot say when a locked version is released and reports the date as unknown.

MinIO does not tell these rights apart: a key with `s3:DeleteObject` alone can delete a version there. A setup that works on MinIO does not prove the AWS policy is complete, so grant the full list on AWS.

## Agent key

A backup has to see the `locks/gc-*` list before it reuses a pack. The agent key therefore needs read access to `locks/`: `s3:GetObject` on `locks/*` and `s3:ListBucket` for the prefixes `locks/` and `locks/*`. It still has no write or delete right on `locks/`.

Repositories connected before cleanup existed have the old agent policy without these lines. Open the repository in the web interface, take the current agent key policy from its agent key page, and replace the policy of the key in your storage with it. Until you do, the **Agent key** check keeps cleanup from turning on.

## Object Lock and versioning

On a bucket with versioning or Object Lock a plain delete only adds a delete marker; the data stays as an older version. Cleanup therefore deletes older versions itself, by version id, and never asks the storage to bypass a lock.

- A version that Object Lock still protects is **held**: it is not deleted and not counted as freed. The report shows `Held by Object Lock` with the size and the date the lock ends (`frees after 24 Oct 2026`), or says the date is unknown. The next cycle after that date frees it.
- Space therefore comes back later than the cycle: a snapshot forgotten by retention, and the packs only it used, stay in the bucket until their locks run out.
- Repacking on a locked bucket first adds new packs and leaves the old ones until their locks end. Usage can go up before it goes down; the report shows the net effect after the lock dates.
- The delete marker of an object is kept until its last real version is gone, otherwise the object would show up in listings again.
- Removing a delete marker by hand brings the object back, but the data it pointed to may have been deleted by then.

## Backups that run outside the server

`lode backup` writes into the repository directly, with no server involved, so it follows the same protocol:

- At the start it reads `locks/gc-*` and uploads again any blob that lives only in a marked pack.
- It writes a marker `locks/writer-<id>` before it reads anything, and a `locks/writer-<id>.done` object when it ends. Deletion waits for every marker that is older than the mark and has no `.done` partner.
- If the key it uses may not write under `locks/` (a write-only agent key does not), it cannot leave a marker. Such a run is limited to 23 hours and refuses to create a snapshot after that, which the pause (at least 24 hours) outlasts.
- If the key may not read `locks/`, `lode backup` stops with an error, because it could reuse a pack that is about to be deleted. Fix the key policy, or pass `--no-gc-check` when no Lodestore server cleans up this repository. With writer keys the same unreadable `locks/` also stops the check for server signatures, unless `--no-gc-check` is given.

A `lode` from before cleanup existed writes no marker and does not read `locks/`. Its backups are safe only if each of them runs for less than the pause. Update `lode` before turning cleanup on for a repository such a client writes to.

## Unfinished backups

A marker with no `.done` partner means a `lode backup` that has not ended. Deletion waits for it for as long as it takes: markers do not expire. The card shows the host and start time of each one, and `lode gc status` lists them.

If the process was killed or the machine was lost, the marker stays open forever. Once you know the backup is dead, close the marker:

```sh
lode gc unlock main
```

It shows which hosts cleanup is waiting for and asks for confirmation (`--yes` skips the question). If the backup is in fact still running, its snapshot can lose data and fail to restore, so unlock only backups you know were interrupted.

## Quarantined snapshots

A host with the writer keys can put an object under `snapshots/` that does not open, and a real snapshot can be damaged. Cleanup cannot tell what such a snapshot references, so it never deletes on a guess. A snapshot goes into quarantine when:

- the object has the right name but does not open, or
- it opens but names a tree or a data blob that is not in the repository.

A file under `snapshots/` whose name does not match its content is not a snapshot and is ignored with a warning.

What happens:

- A cycle reads every snapshot, collects all that it cannot follow, and fails before it publishes a list of packs to delete. Nothing is deleted. A dry run lists the same snapshots and does not fail.
- The alert `snapshot_quarantined` is raised, see [Alerts](/docs/guides/alerts/). The **Free up space** card lists the snapshots with the reason, and the cycle log has an event `quarantined`. The same list is in the full report (`quarantined`).
- The quarantine is checked again at every cycle. A snapshot that opens again, for example after you restored the missing object in the bucket, leaves it by itself.
- An exclusion you confirm while a cycle is reading the snapshots is kept, and is judged by the next cycle: a dry run or cleanup lifts an exclusion only when it started reading after you confirmed it. It lasts as long as the snapshot stays unreadable: a snapshot that reads again, or that is no longer in the bucket, loses its exclusion. A snapshot that is deleted while a cycle reads keeps its exclusion until the next cycle. If it breaks again later, it is back in quarantine and the cycle waits for you.

To carry on without such a snapshot, open **Free up space** and choose **Exclude from cleanup…** for it. This needs the `snapshots:delete` scope; with the API it is `POST /api/v1/repositories/{id}/gc/quarantine/{snapshot_id}/exclude`. Then run cleanup again.

Excluding is not safe for the data. An excluded snapshot no longer protects what it points to, and the next cycle may delete data that only this snapshot uses. If the snapshot is a real backup that you still need, repair it instead: restore the object or run a full check first. The snapshot object itself stays in the bucket; delete it separately if you do not need it.

A snapshot that belongs to a backup job (the catalog ties it to a host and a job, or a record signed by the server names it, see [ADR 0006](/docs/adr/0006-snapshot-provenance/)) cannot be excluded: the card says so and offers no button. Run a full check, then repair the repository or delete the snapshot. A snapshot that retention has already forgotten, but that is still in the bucket (for example under Object Lock), is not bound to a job and can be excluded.

The exclusion is kept in the server database only. After a lost database the snapshots come back into quarantine at the next cycle, and you confirm each again.

## Lost server database

A server database restored from a backup (on Docker or Helm) remembers the certificate serial of every agent as of the backup. An agent that renewed its certificate later is refused with `certificate_stale` and raises the alert `agent_certificate_rejected`, and an agent enrolled after the backup is refused as unknown. They keep retrying; enrol those hosts again with `install.sh --force` and a new join token. See [Agent credentials](/docs/security/threat-model/#agent-credentials).

The server remembers which cycle it started. If its database is lost or replaced and a cycle was marked at that time, the bucket still holds the `locks/gc-<id>` object, and the new server sees a cleanup it did not start and refuses to run (**Another cleanup**). Delete the object `locks/gc-<id>` in the bucket by hand to continue, but only after you have made sure that no other Lodestore server cleans up this bucket: that server would be using the list.

The server also keeps in the bucket the fact that it has signed the repository. A new database therefore does not sign the shards of the repository again, and does not treat a shard that appeared since then as vouched for. What the server verified is kept in the database: after a loss, the server downloads and checks once more the data that new backups depend on, which costs bandwidth, not data.

If the server says that the repository has attestations but no upgrade marker, an object in `locks/` was removed, and the server refuses to sign the repository again (the error reads "the repository has server attestations but no upgrade marker; refusing to sign its shards again"). Cleanup of that repository fails with this error before it deletes anything, and a new backup becomes failed with `verification_failed` one hour after its first failed check. To recover:

1. Find out who deleted the object from `locks/`, and what else was written to the bucket since.
2. Run `lode check --level packs` on the repository.
3. Remove the `locks/attest-*` objects of the repository in the bucket. The server then signs the current state of the index again.

A cycle that fails before it deletes anything takes its own marker down. One that stops while deleting keeps the marker, and the next run of the same server continues from it. A leftover `locks/gc-*` that this server does not know is the case described here.

## Backups the server could not verify

After a backup finishes, the server opens the new frames the snapshot depends on and checks that every tree and data blob resolves through shards it has checked. It downloads each new byte once, so plan for that egress. The agent is not kept waiting: the job can become `failed` after it finished. If the check fails, the job ends as `failed` with the error code `verification_failed`, the snapshot carries `unverified_reason` in the snapshot list of the API, and the alert for a failed backup fires when that job is still the latest finished job of its policy on that host (a job without a policy, such as a stdin backup, raises no alert yet). If the server cannot run the check, for example because the storage is unreachable, it retries for at most one hour from the first failure, and then the job becomes `failed` with `verification_failed` too.

The check counts only a snapshot that belongs to the job: a job that reports a snapshot the catalog already links to another job fails it. The hour of retries is measured on the server's clock from the first failed attempt.

Reads and cleanup do not depend on this check. When `lode restore`, or a read the server makes while browsing a snapshot, finds that the first known copy of a blob does not open, it tries the other copies listed by the index, including those of shards that a newer shard replaced, and every frame is authenticated by the blob id. A restore run by an agent and a download as an archive do not try other copies yet: they read each file's data from the first known copy only.

## Attestations and cleanup

The server signs what it checks in `locks/attest-*` objects: the index shards it vouches for, and the record of who made each snapshot. Every backup the server has verified adds exactly one such object, and so does every strict `lode backup` (a backup with the repository password in a repository the server has upgraded; see [Command line](/docs/reference/cli/#backup-in-a-repository-the-server-has-upgraded)). Only a cleanup cycle merges them. With cleanup turned off, the objects pile up by one per backup, and every open of the repository for writing and every `lode restore latest` reads all of them, so the cost grows with the number of backups. Keep cleanup turned on for a repository that receives backups.

When a cycle merges the objects it keeps one record per snapshot that still exists, drops the shards that the server has revoked, and cuts the result into parts so that no object grows past the size limit. A shard is revoked when the server's check of a backup finds that it does not hold the data it claims; the job then fails with `verification_failed`, the server remembers the shard in its database and tells every agent not to use it. A forgotten snapshot loses its record at the next merge, not at the moment it is forgotten.

## Reading a report

`lode gc --dry-run` and the **Last dry run** tab show the same facts:

| Fact | Meaning |
|------|---------|
| Would free / Freed | Bytes the cycle removes (without the held part). |
| To delete / Packs deleted | Packs that no snapshot uses. |
| To repack / Packs repacked | Packs rewritten into fewer, fuller ones. |
| Repack transfer | Data the server reads and writes for the repack. Plan the bandwidth for this. |
| Held by Object Lock | Bytes Object Lock keeps, and when they can go. Shown only when not zero. |

The full list of packs is in `lode gc --dry-run --json` and in the API (`GET /api/v1/repositories/{id}/gc/cycles/{cycle_id}/report`). The full report also has two lists that explain why a cycle was careful:

- `skipped_shards`: index shards that cleanup did not use, each with the reason. A shard is skipped when it is larger than the limit, does not open or holds a malformed record. The packs such a shard names are not deleted.
- `quarantined`: snapshots that cleanup could not follow, see [Quarantined snapshots](#quarantined-snapshots).

### A cycle that stops because of skipped shards

A skipped shard may name packs that an agent still reuses, and an agent from 0.1 reads shards without a size limit. While the repository has an enrolled agent that does not announce `shard_limit_v1`, the cycle fails when it skipped a shard and has packs to delete. The message reads that some index shards could not be read by cleanup, that agents older than 0.2 may still reuse the packs they name, names the hosts, and says that no pack was deleted. Nothing was deleted. Update or revoke the named hosts (revoked agents do not count) and run cleanup again. Without such agents, a skipped shard does not stop a cycle. Update `lode` together with the server too: a `lode` from 0.1 started with `--writer-keys` is not visible to the server.

One risk stays: an agent from 0.1 that was enrolled after a cycle (an old package, a downgrade, or a host re-enrolled after it was revoked) can deduplicate against a pack that the cycle has deleted. The server's check of its backup fails that job with `verification_failed`, and only an administrator can enroll an agent. After an update, do not enroll agents older than 0.2.
