---
title: Lodestore vs restic
description: How Lodestore, a backup server with agents, compares with restic, a command-line backup program. Facts are taken from restic's own documentation.
---
restic is a command-line backup program. Lodestore is a server with an agent on each host. Both store encrypted, deduplicated snapshots; they differ in who runs the backups and where the control sits.

## Why teams pick Lodestore

- **One place for a fleet of Linux hosts.** A web interface shows which hosts backed up and which did not, schedules and retention run on the server, and alerts cover failed or missing backups, offline hosts and failed checks. Files can be searched across the snapshots of every host.
- **Hosts that cannot delete their own backups.** On S3-compatible storage the agent writes with a key that has no delete rights, and deletion and cleanup run only on the server, with S3 Object Lock and versioning supported. See the [threat model](/docs/security/threat-model/#agent-credentials) for what this does and does not cover.
- **Restore without the server.** The bucket and a password or recovery key are enough for `lode` to restore, so losing the server does not lose the data.
- **Checks you do not script.** Scheduled integrity checks, including a test restore of sampled files, are reported in the interface.
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/docs/quickstart/).

## At a glance

| | restic | Lodestore 0.1 |
|---|--------|---------------|
| Shape | A command-line program that runs when you start it | A server and an agent on each host |
| Scheduling | Not built in; cron, systemd timers or Task Scheduler start it | Cron schedules in an IANA time zone, jitter and catch-up of missed runs, set on the server |
| Protection from a compromised host | Append-only mode is provided by the backend, for example rest-server; `forget` and `prune` need full access | Agent key without delete rights on S3-compatible storage; deletion is server-only |
| Integrity checks | `restic check` verifies the repository structure, and stored data when flags are given | Three scheduled levels: structure, packs and a test restore |
| Platforms | Linux, macOS, FreeBSD, OpenBSD, Windows | Linux, x86_64 and arm64 |
| License | BSD-2-Clause | Apache-2.0 (agent, format, CLI), AGPL-3.0 (server) |

### Storage

| Storage | restic | Lodestore 0.1 |
|---------|--------|---------------|
| AWS S3 and S3-compatible storage | Yes | Yes |
| Local or network folder | Yes | Yes, the agent can delete in it |
| SFTP, Azure Blob Storage, Google Cloud Storage | Yes | Planned |
| REST Server, OpenStack Swift, Backblaze B2, Alibaba Cloud OSS, other services through rclone | Yes | Not in 0.1 |
