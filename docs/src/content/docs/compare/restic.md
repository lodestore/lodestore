---
title: Lodestore vs restic
description: How Lodestore, a backup server with agents, compares with restic, a command-line backup program. Facts are taken from restic's own documentation. No speed figures.
---
restic is a command-line backup program. Lodestore is a server with an agent on each host. Both store encrypted, deduplicated snapshots; they differ in who runs the backups and where the control sits. The restic facts on this page come from its documentation, version 0.19.1, read on 2026-11-02. It contains no speed or resource numbers, because we have not measured them.

## Why teams pick Lodestore

- **One place for a fleet of Linux hosts.** A web interface shows which hosts backed up and which did not, schedules and retention run on the server, and alerts cover failed or missing backups, offline hosts and failed checks. Files can be searched across the snapshots of every host.
- **Hosts that cannot delete their own backups.** On S3-compatible storage the agent writes with a key that has no delete rights, and deletion and cleanup run only on the server, with S3 Object Lock and versioning supported. See the [threat model](/lodestore/security/threat-model/#agent-credentials) for what this does and does not cover.
- **Restore without the server.** The bucket and a password or recovery key are enough for `lode` to restore, so losing the server does not lose the data.
- **Checks you do not script.** Scheduled integrity checks, including a test restore of sampled files, are reported in the interface.
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/lodestore/quickstart/).

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

Lodestore starts with S3-compatible storage because only there can an agent hold a separate key without delete rights, and only there is Object Lock available.

## restic may fit when

- You back up macOS, Windows or BSD hosts, or use storage without an S3 API.
- You want a tool and not a service: no server to run, and your own scheduler or configuration management starts the backups.
- You need to copy snapshots between repositories with `restic copy`.

## Facts we checked

| Fact | Source | Version | Checked on |
|------|--------|---------|------------|
| License is BSD-2-Clause; the latest release is v0.19.1, published 2026-07-05. | [restic on GitHub](https://github.com/restic/restic) | v0.19.1 | 2026-11-02 |
| Supported repository locations: local, SFTP, REST Server, Amazon S3 and S3-compatible (Minio, Wasabi), Alibaba Cloud OSS, OpenStack Swift, Backblaze B2, Microsoft Azure Blob Storage, Google Cloud Storage, and other services via rclone. | [Preparing a new repository](https://restic.readthedocs.io/en/stable/030_preparing_a_new_repo.html) | 0.19.1 | 2026-11-02 |
| Installation instructions exist for macOS, FreeBSD, OpenBSD and Windows besides Linux, and official binaries are offered. | [Installation](https://restic.readthedocs.io/en/stable/020_installation.html) | 0.19.1 | 2026-11-02 |
| "Restic does not have a built-in way of scheduling backups, as it's a tool that runs when executed rather than a daemon." The page points to systemd and cron on Linux and BSD, Task Scheduler on Windows, and resticprofile. | [Backing up, Scheduling backups](https://restic.readthedocs.io/en/stable/040_backup.html) | 0.19.1 | 2026-11-02 |
| The threat model says restic is not designed to protect against attackers deleting files at the storage location, and describes what an attacker with append-only access (read and write allowed, delete and overwrite denied) can and cannot do. | [References, Threat Model](https://restic.readthedocs.io/en/stable/100_references.html) | 0.19.1 | 2026-11-02 |
| Append-only mode is provided by the backend: "Restic's rest-server features an append-only mode, but few other standard backends do"; for those, the page suggests rclone in between. Maintenance (`forget`, `prune`) needs full access, so the page recommends a separate, well-secured client for it. | [Removing backup snapshots, Security considerations in append-only mode](https://restic.readthedocs.io/en/stable/060_forget.html) | 0.19.1 | 2026-11-02 |
| We did not find the words "Object Lock" on the installation, repository preparation, backup, working-with-repositories, removing-snapshots, FAQ or references pages. This is a statement about those pages, not about what restic can do. | [Preparing a new repository](https://restic.readthedocs.io/en/stable/030_preparing_a_new_repo.html), [References](https://restic.readthedocs.io/en/stable/100_references.html) | 0.19.1 | 2026-11-02 |
| `restic check` verifies the structure of the repository by default, and the integrity of the stored data when flags are given; `restic copy` copies snapshots between repositories. | [Working with repositories](https://restic.readthedocs.io/en/stable/045_working_with_repos.html) | 0.19.1 | 2026-11-02 |

What Lodestore 0.1 does is listed in the [changelog](https://github.com/lodestore/lodestore/blob/main/CHANGELOG.md), and [How we test data safety](/lodestore/security/how-we-test/) says which checks exist. If a statement about restic on this page is wrong or out of date, please [open an issue](https://github.com/lodestore/lodestore/issues).
