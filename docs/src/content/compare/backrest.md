---
title: Lodestore vs Backrest
description: How Lodestore, a self-hosted backup server with agents, compares with Backrest, a web interface built on top of restic. Facts are taken from Backrest's own documentation.
---
Backrest is a web-accessible backup solution built on top of restic: a web interface that wraps the restic command line. Lodestore is a self-hosted server with an agent on each host and its own repository format.

## Why teams pick Lodestore

- **Hosts that cannot delete their own backups.** On S3-compatible storage the agent writes with a key that has no delete rights, and deletion and cleanup run only on the server, with S3 Object Lock and versioning supported. In restic, the engine under Backrest, `forget` and `prune` need full access to the repository. See the [threat model](/docs/security/threat-model/#agent-credentials).
- **One server for a fleet of Linux hosts.** Schedules and retention run on the server, alerts cover failed or missing backups, offline hosts and failed checks, and you can find a file across the snapshots of every host.
- **Apache-2.0 end to end.** The server, the agent and the format are Apache-2.0; Backrest is GPL-3.0.
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/docs/quickstart/).

## At a glance

| | Backrest | Lodestore 0.2 |
|---|----------|---------------|
| What it is | A web interface on top of restic that wraps the restic command line and can run in the background | Self-hosted server with agents, Apache-2.0 |
| Scheduling | Cron-scheduled backups and maintenance such as prune, check and forget | Cron schedules in an IANA time zone, jitter and catch-up of missed runs, set on the server |
| Multiple hosts | Multihost sync: a Backrest instance acts as a server, other instances pair with it as clients using a token, send their backup history to it and can receive shared repositories | One server and an agent on each host; the web interface shows every host |
| Protection from a compromised host | Built on restic: append-only mode is provided by the storage backend, and `forget` and `prune` need full access to the repository | Agent key without delete rights on S3-compatible storage; deletion is server-only; the server checks every new backup an agent makes, so index metadata forged by a compromised host cannot delete the backups of other hosts or pass off an unrestorable backup as a success; the latest snapshot is picked from signed records of which host and job made each snapshot, not from the time a host writes into it; the agent writes restored files only into folders its own configuration allows, even when the server asks for more |
| Platforms | Linux, macOS, Windows, FreeBSD and Docker | Linux, x86_64 and arm64 |
| License | GPL-3.0 | Apache-2.0 |

### Storage

| Storage | Backrest | Lodestore 0.2 |
|---------|----------|---------------|
| AWS S3 and S3-compatible storage | Yes | Yes |
| Local folder | Yes | Yes, the agent can delete in it |
| SFTP, Azure Blob Storage, Google Cloud Storage | Yes | Not yet, see the [roadmap](/roadmap/) |
| Other services through rclone | Yes | Not in 0.2 |
