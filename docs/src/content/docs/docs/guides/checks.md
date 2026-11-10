---
title: "Integrity checks: knowing a restore will work"
---
A backup that cannot be restored is worth nothing, and the failure usually shows up on the worst day. Integrity checks read the repository the way a restore would and tell you early when something is wrong. A failed check raises the alert **Check failed**, see [Alerts and notifications](/docs/guides/alerts/).

Checks are run by the server, with the server's own storage key, and need no agent. `lode check` does the same from a terminal without any server.

Where to find them:

- Web interface: the **Integrity checks** section of the repository, under Settings, Repositories.
- Terminal: `lode check` and `lode server check`, see [Command line](/docs/reference/cli/#integrity-checks).
- API: the `Checks` group of the [API](/docs/reference/api/#integrity-checks).

## The three levels

| Level | What it does | Cost |
|-------|--------------|------|
| `structure` | Lists the bucket, reads the index and follows every snapshot and tree to its blobs. Reports snapshots that cannot be opened, whose tree has a missing pack, or that pass a restore's default limits (2^23 entries or 1 GiB of tree data in one directory, 2^24 entries or 2 GiB of tree data held at once across the directories on the way down, and the budget of the whole walk), blobs that no pack holds and packs the index names that the bucket lacks or whose size differs from the one the index records. Also counts the real size of the repository, which the overview and the metrics show as storage used. The check reads each directory once, whatever the path it is reached by (a piece of a directory is read once for every directory it belongs to), and judges each snapshot by its own path: a directory that several snapshots share is checked once, and findings in or below it are reported for every snapshot that reaches it, except as described next. The pieces of a directory over the limits of one directory are not read. When the subtree of a directory alone passes a limit of a path (entries or tree data held at once, or nesting), every snapshot that reaches it gets that limit as its finding; the check does not walk the rest of that directory, so other findings below it may not be listed for those snapshots. The members of a hardlink group are compared within a snapshot, and the snapshot in which they differ gets the finding. `lode restore --allow-large-directories` lifts the four limits of a directory and a path for a snapshot written before backups refused such trees. The count of trees checked (`Trees checked`) is the number of different trees read, so a directory shared by many snapshots counts once. | Reads metadata only. Fast, cheap. |
| `packs` | Downloads packs, decrypts every blob in them and compares its hash with its id. Also compares each pack with what the index says about it (size and the place of every blob). Finds silent damage of a stored object. | Downloads data. Limited per run, see below. |
| `restore` | Restores a random sample of files from random snapshots into a scratch folder on the server, compares each file with its recorded hash and deletes the folder. The only level that proves the whole chain, key to file, works. | Writes to the server's disk. Limited per run. |

A run ends as **passed**, **failed** (it found problems) or **stopped** (it could not finish, for example the bucket was unreachable or the server restarted). A stopped run counts as failed for alerts, because nobody learned that the repository is fine. A `packs` or `restore` check that finds nothing to read, for example in an empty repository, is not a pass: `lode check` reports **Nothing to check** and still exits with 0.

## Schedule and defaults

Every level has its own schedule. All three are on by default for every repository.

| Level | Default schedule (`cron`, UTC) | Limits |
|-------|--------------------------------|--------|
| `structure` | `0 2 * * *`, every day at 02:00 | none |
| `packs` | `0 4 * * 3`, Wednesday at 04:00 | `5GiB` read per run |
| `restore` | `0 5 2 * *`, the 2nd of the month at 05:00 | `20` files, `1GiB` written per run |

Change the cron expression, the time zone and the limits in the repository settings or with `PUT /api/v1/repositories/{id}/checks/settings`. A level can be turned off. A repository whose checks are all off shows **Off**.

Details of the schedule:

- A repository that has never had a finished structure check gets one right away, without waiting for the slot.
- After you save new settings nothing starts at once; the next run is the next slot.
- If the server was down at a slot, it starts one run when it comes back, not one per missed slot.
- A run can be started by hand at any time: from the repository in the web interface, with `lode server check`, or `POST /api/v1/repositories/{id}/checks/{level}/run`.

## Limits and rotation

Reading every pack of a large repository each week would cost a lot of traffic, so the `packs` level reads at most `Read per run` bytes (default 5 GiB) and continues the next time where the last run stopped. It picks packs in this order:

1. packs that failed the last time they were read,
2. packs that were never read, the newest first,
3. then the pack that was read longest ago.

Over several runs every pack is read. The **coverage** shows how much: the share of packs read in the last 30 days and the number of packs never read. The same number is the metric `lodestore_pack_coverage_ratio`. Raise `Read per run` or run the level more often to cover a bucket faster; the traffic is paid to your storage provider.

The `restore` level restores `Files per run` files (1 to 1000, default 20) and stops when the files together reach `Max data per run` (default 1 GiB). It picks files from random snapshots, so a month of runs tests many different snapshots. A file larger than the limit is never picked, and a file that would take the run over the limit is counted under **Skipped**. When no file fits, the run stops with "Stopped at the size limit." and counts as stopped; raise `Max data per run`.

## Disk space on the server

Only the `restore` level uses the server's disk. It writes into `check-restore/<run id>` under the data directory (`/var/lib/lodestore` in the container image) and removes the folder when the run ends, however it ends. A folder left by a crash is removed when the server starts. Keep at least `Max data per run` free on the volume of the data directory; the default needs 1 GiB.

The other levels keep only the report in the database.

## Working together with cleanup

Checks and [cleanup](/docs/guides/gc/) both read the bucket, and cleanup can delete a pack a check is about to read. They therefore take turns:

- A check does not start while a cleanup cycle is repacking or deleting. Starting one by hand answers `409` with "A cleanup cycle is running. Checks wait for it."; a scheduled check keeps its slot and tries again on the next tick.
- A cleanup cycle does not delete while a check is queued or running.
- Only one check runs in a repository at a time. A second start answers `409` with "Another check is running on this repository."

A cleanup that only waits for its pause does not block checks.

## Reading a report

The report of a finished run lists the facts of its level (snapshots checked, packs read, blobs verified, files restored, hash mismatches) and the problems it found, each with the object it concerns and what is wrong. The first 10 000 problems are kept; the total is always exact. Notes are things that are not errors: packs the index does not list yet, packs a cleanup has marked for deletion, objects in the bucket that Lodestore ignores, and locks that could not be read.

A repository in the list shows the worst result of its enabled levels: **Passed**, **Failed**, **Never run** or **Off**. `GET /api/v1/repositories/{id}/checks/runs/{run_id}/report` returns the full report.

## What to do when a check fails

- **Missing pack or blob.** The bucket lost an object, or something deleted it. Check the bucket's own logs and versioning, and restore the object from a bucket version if there is one. Snapshots that need the lost data cannot be restored completely; snapshots that do not use the object are unaffected.
- **Damaged blob or hash mismatch.** The stored bytes changed. Read the object name in the report, compare with a bucket version if you have versioning, and run the `packs` level again to see whether it repeats.
- **Stopped.** Read the message. A network or credentials problem fixes itself when the cause goes away; run the level again by hand to confirm.

The alert clears when the next finished run of the same level passes.
