---
title: How we test data safety
description: Which automated checks protect your backups, when they run, and what they do not cover.
---
A backup tool is trusted for one thing: the data comes back. This page lists the checks that exist in the repository and in CI today, what each one proves, and where the gaps are. Every file named here is in the repository; if a check is not listed, assume it does not exist. The section [What we have not verified](#what-we-have-not-verified) is part of the page, not a footnote.

Lodestore is version 0.1. The checks below are the ones built and kept green so far, not an external audit. No external audit of the format or the cryptography has been done.

## What counts as losing data

A test is written to catch one of these:

- A snapshot in the bucket cannot be restored, or restores different bytes, with only the bucket and the repository password or recovery key.
- An object a snapshot needs is deleted by anything other than the server's cleanup, or overwritten by Lodestore's own writer. A stolen agent key is a different case: see [`s3_write_only.rs`](#what-the-bucket-looks-like-from-outside) below.
- Cleanup deletes something that a snapshot, finished or still being written, needs.
- The bucket holds readable file names or contents.
- A change to the format silently breaks old repositories or deduplication.

## Frozen format fixtures

The repository format is frozen by committed data, so a dependency bump or a refactor cannot change it unnoticed.

- [`crates/lodestore-format/tests/golden.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/golden.rs) reads a repository written by an earlier build (`tests/fixtures/v1`): it lists the snapshots, restores the contents, and re-encodes them to the same bytes. Any change to the encoding, the chunker, the ciphers or the compression that old repositories would not survive makes it fail.
- [`crates/lodestore-format/tests/vectors.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/vectors.rs) holds frozen outputs of the primitives for fixed inputs: the chunk boundaries, key derivation, key slots, the AEAD, the key exchange, packs, index shards and the cleanup records. zstd output is left out on purpose because it is not guaranteed between versions; decompressing the committed frames is covered by the golden test.
- The CI job `fixtures-append-only` in [`ci.yml`](https://github.com/lodestore/lodestore/blob/main/.github/workflows/ci.yml) runs [`scripts/check-fixtures-append-only.sh`](https://github.com/lodestore/lodestore/blob/main/scripts/check-fixtures-append-only.sh). It fails when a change modifies, deletes or renames a file under `fixtures/v1`; new files are allowed. A format change has to ship as `format_version` 2 with its own fixtures next to the old ones.

Limit: the fixtures prove that old data stays readable. They do not prove that the format is secure; see the cryptography notes in the [threat model](/lodestore/security/threat-model/) and the [ADRs](/lodestore/adr/0002-key-model-and-envelope/).

## What the bucket looks like from outside

- [`no_plaintext.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/no_plaintext.rs) backs up files whose names and contents are recognizable and then searches every object, names included, for them. Nothing may appear.
- [`s3_write_only.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/s3_write_only.rs) uses an S3 user with a write-only policy from the test kit, close to what an agent key gets. It checks that this user can back up and deduplicate, and cannot delete or read packs. Lodestore's own writer refuses to replace an object (`If-None-Match: *`), but that is the client behaving, not a limit on the key: with the policy generated for MinIO an agent key can overwrite, and `wizard_s3_write_only_key_can_overwrite_on_minio` in [`repos_s3.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/repos_s3.rs) records it. Versioning or Object Lock is what protects the data there. On MinIO the key holder sends a plain `PutObject`, and nothing in the policy refuses it.
- [`restore_hostile.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/restore_hostile.rs) feeds the restore snapshots that someone holding the writer keys could forge (names such as `..`, duplicate or unsorted entries, size lies, decompression bombs) and a target with planted symlinks. The restore must refuse or stay inside the target.

## Object storage and Object Lock

S3 behavior is tested against a real S3 server, not a mock. [`crates/lodestore-testkit/src/minio.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-testkit/src/minio.rs) starts a pinned MinIO release (`RELEASE.2025-09-07T16-13-09Z`) and its `mc` client, downloaded from their GitHub releases and checked against a sha256 recorded in the file. A test keeps `scripts/pins.env` and the MinIO image of the demo stand (`deploy/demo/minio/Dockerfile`) equal to those values.

- [`s3.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/s3.rs): backup and restore round trip, presigned reads, create-only writes. Its slower tests go through a fault-injecting proxy ([`fault_proxy.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-testkit/src/fault_proxy.rs)): a throttled upload is retried, and an upload whose answer was lost is counted as success. One test shows that deleting a version of an object under COMPLIANCE retention is refused.
- [`gc_purge_s3.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/gc_purge_s3.rs): removal of object versions on versioned, COMPLIANCE-locked and GOVERNANCE-locked buckets, including a delete marker kept while the version under it is locked, and an agent key being denied.
- [`gc_s3.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/gc_s3.rs): whole cleanup cycles on versioned, COMPLIANCE and GOVERNANCE buckets. There is no whole cycle on a plain bucket: `gc_purge_s3_plain_bucket` in `gc_purge_s3.rs` covers the removal step only, and `gc_chaos.rs` below runs on a local folder. Locked versions must survive, the report must say how many bytes are still locked and until when, and a server clock ahead of the bucket's must make cleanup wait. The test where a lock runs out clears the retention on the version with `mc` as the storage administrator; it does not wait for a real day to pass.
- [`retention_s3.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/retention_s3.rs) and [`repos_s3.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/repos_s3.rs): what retention does when the server key may not delete and when Object Lock keeps a version, and whether the repository wizard detects Object Lock.

## Cleanup (garbage collection)

Cleanup is the only code that deletes data, so it gets the most scrutiny. Its design is described in [Cleanup](/lodestore/guides/gc/).

- [`gc_interleave.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/gc_interleave.rs) runs backups and the cleanup steps in every order the protocol allows, with one or two backups, and after each order restores every snapshot in the bucket. The model assumes the rule the design relies on: a backup that opened before the cleanup manifest was published finishes before the verification step, which the time limit of an agent job guarantees. A backup that goes past that limit is outside what this test shows.
- [`gc_chaos.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/gc_chaos.rs) runs two real agents against a real server. Backups run while a cleanup cycle marks, verifies and sweeps, and in one variant the server restarts in the middle of the cycle. Afterwards every kept snapshot must restore and no backup job may have failed.
- [`gc_mark.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/gc_mark.rs) shows that the survey step only reads and stops on anything it cannot follow. [`gc_repack.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/gc_repack.rs) runs repack, verify, compact and sweep on a real repository in a folder. [`gc_locks.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/gc_locks.rs) shows that a writer does not trust candidate packs that cleanup has published for deduplication, and stops on a manifest it cannot read.

## Restoring without the server

- [`disaster.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/disaster.rs): a repository is created, its catalog database is deleted, and the data is restored from the bucket with the recovery key alone.
- [`rebuild.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/rebuild.rs): the server rebuilds its catalog from the bucket into an empty database, keeps history it already had, skips unreadable snapshots with a reason, and never deletes rows.
- [`recovery.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lode-cli/tests/recovery.rs), [`rebuild_index.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lode-cli/tests/rebuild_index.rs) and [`e2e_chain.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lode-cli/tests/e2e_chain.rs): the `lode` command line restores with a recovery key, rebuilds a damaged index, and runs the whole chain (init, backup, list, restore) against a local folder and against MinIO.
- [`list_independence.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/list_independence.rs): a restore does not depend on listing the bucket.
- The quickstart check, described below, stops the server and restores with the `lode` binary from the bucket and the password.

## Integrity checks

[Integrity checks](/lodestore/guides/checks/) have three levels (`structure`, `packs`, `restore`), and each has tests that damage a repository and expect the check to notice.

- [`check.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/check.rs): a clean repository passes; a missing or truncated pack, an index that disagrees with a pack, a corrupted pack and a damaged file in a sampled restore are reported; byte and size limits are respected.
- [`checks_api.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/checks_api.rs) and [`checks_gate.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/checks_gate.rs): the same through the server, including the rotation of the `packs` level over several runs, one run at a time per repository, and a panicking or hanging check ending its run instead of blocking the others.

## Metadata, installation and updates on a real Linux host

Some behavior needs root, extended attributes or systemd. These tests are `#[ignore]`d in a normal run and are started by [`scripts/test-linux.sh --root`](https://github.com/lodestore/lodestore/blob/main/scripts/test-linux.sh) in the `linux-root` workflow:

- `fidelity_root_` in [`fidelity.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/fidelity.rs): owners, setuid bits, capabilities, POSIX ACLs, device nodes, `trusted.` attributes survive a restore.
- `backup_root_` in [`backup_behavior.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-format/tests/backup_behavior.rs): backups that stay on one file system.
- `e2e_install_` in [`e2e_install.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/e2e_install.rs): the install script run on a systemd host enrolls an agent, writes the unit and the files with the expected modes, and the agent comes online. A second test installs with `--trust-key` and checks that the key lands in the trusted keys folder.
- `update_systemd_` in [`update_systemd.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-agent/tests/update_systemd.rs): a failed agent update returns the previous binary through a systemd timer.

## The quickstart check

[`scripts/quickstart-ci.sh`](https://github.com/lodestore/lodestore/blob/main/scripts/quickstart-ci.sh) takes the marked command blocks from the [Quickstart](/lodestore/quickstart/) page: starting the server, the setup link, the agent install and the restore without the server. It starts MinIO and the server image, goes through the setup wizard and the host enrollment with the install script, makes a backup, and restores it through the API. It then compares the restored tree with the original using `diff -r` and a list of sha256 sums. Finally it stops the server container and restores again with only `lode`, the bucket and the password, and compares again. The whole run has a budget of 600 seconds, which is the "first backup in ten minutes" promise.

Limit: the script drives the API where the page describes clicks in the interface (wizard, adding a host, restore). It proves the mechanics, not the screens. There is no browser test. The policy of the first backup is set in the script and does not come from the page. The check after publishing adds `--from-github` to the install command, so it installs the agent from GitHub Releases and does not test the download from your server. The local folder section of the page is not run.

## When each check runs

| Workflow | File | Runs | What it contains |
|----------|------|------|------------------|
| `ci` | [`ci.yml`](https://github.com/lodestore/lodestore/blob/main/.github/workflows/ci.yml) | every pull request, every push to `main` | format, clippy, no `openssl-sys` or `ring` in the dependency tree, the workspace tests with SQLite (including the MinIO-backed ones), the web tests, license checks, `fixtures-append-only`, the self-tests of the release scripts (job `release-scripts`: version check and signing) |
| `linux-root` | [`linux-root.yml`](https://github.com/lodestore/lodestore/blob/main/.github/workflows/linux-root.yml) | pull requests to `main`, weekly, before a release | the root tests above and the slow `s3_` tests (fault proxy, Object Lock version delete) |
| `quickstart` | [`quickstart.yml`](https://github.com/lodestore/lodestore/blob/main/.github/workflows/quickstart.yml) | pull requests to `main`, pushes to `ci/**`, before and after a release | the quickstart check above |
| `build` | [`build.yml`](https://github.com/lodestore/lodestore/blob/main/.github/workflows/build.yml) | weekly, by hand, before a release | static musl builds, the image smoke test, a Helm install into a kind cluster, the server test suite against PostgreSQL 16, security advisories |
| `release` | [`release.yml`](https://github.com/lodestore/lodestore/blob/main/.github/workflows/release.yml) | on a `v*` tag | runs `ci`, `build` and `linux-root`, signs, builds the images, smoke-tests both architectures, runs the quickstart on the candidate image, then publishes; after publishing it runs the quickstart again against the published image and opens an issue if that fails |

Two consequences. The PostgreSQL run is not part of every pull request; it runs weekly and before a release. And the post-publish quickstart cannot stop a release that is already public, it can only report.

The nextest `ci` profile in [`.config/nextest.toml`](https://github.com/lodestore/lodestore/blob/main/.config/nextest.toml) retries one test, `reenroll_keeps_agent_id_and_rebinds_snapshots`, up to three times. That is a known flaky test and it is not about stored data, and it is the only retry in that profile. A web test sets its own retry in its options, `{ retry: n }`, so search `web/src` for those.

## What we have not verified

- **Object Lock on real AWS S3.** All Object Lock behavior is checked against MinIO. MinIO and AWS can differ in details such as how retention is reported, how long a lock lasts and how delete markers behave. Until a run against AWS exists, treat Object Lock support on AWS as expected, not demonstrated.
- **The refusal of overwrites in the AWS policy.** No test covers the `Deny` on `s3:if-none-match` that the generated AWS policy ends with. The tests that touch overwriting use the MinIO policy, which leaves it out.
- **`#[ignore]`d tests outside the filters.** The `linux-root` workflow starts only the filters in [`linux-root.yml`](https://github.com/lodestore/lodestore/blob/main/.github/workflows/linux-root.yml). An ignored test that matches none of them does not run in CI. Check that file for `hooks_rejects_foreign_owner`, which guards the [hook ownership rule](/lodestore/security/threat-model/#backup-hooks), `special_files_warn_devices` and `download_archive_zip64_large`.
- **Alert delivery to real services.** Alerts to Telegram, Slack and SMTP are tested against local stand-ins that check the request and the error handling ([`notify.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/tests/notify.rs), and for mail delivery [`notify/email.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/src/notify/email.rs)). No test sends a message through the real Telegram, Slack or a real mail server.
- **No fuzzing.** There are no fuzz targets for the format parsers and no property-based tests. The hostile-restore tests above are hand-written cases, so they find only what someone thought of.
- **No browser end-to-end test.** The web interface has component tests; nothing drives a real browser through a backup and restore. The quickstart check uses the API for those steps.
- **Test storage rests on an archived project.** The upstream MinIO repository is archived, and the tests use one pinned release. If that download disappears or stops working on a new platform, the S3 tests need another S3 server. The choice of a replacement is open.
- **Platforms.** CI runs on Linux x86_64 only, plus an arm64 image smoke test at release. The agent supports Linux on x86_64 and arm64. The tests are not run on macOS or Windows in CI.
- **Scale.** Nothing here measures speed, memory or behavior with hundreds of hosts or billions of objects.
- **Operations.** Restoring from a real disaster in your environment, including your IAM policies and network, is something only you can test. Run a [`restore` check](/lodestore/guides/checks/) and the quickstart's restore without the server on your own bucket before you depend on it.
