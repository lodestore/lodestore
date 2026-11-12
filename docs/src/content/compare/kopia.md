---
title: Lodestore vs Kopia
description: How Lodestore, a self-hosted backup server with agents, compares with Kopia, a snapshot backup tool with a command line and a graphical interface. Facts are taken from Kopia's own documentation.
---
Kopia is a snapshot backup tool with a command-line interface and an official graphical interface, designed to back up individual machines, with an optional server mode. Lodestore is a self-hosted server with an agent on each host, built for a fleet of Linux hosts.

## Why teams pick Lodestore

- **Built around a fleet.** Schedules and retention are set on one server for every host. A web interface shows which hosts backed up and which did not, alerts cover failed or missing backups, offline hosts and failed checks, and you can find a file across the snapshots of every host.
- **Hosts that cannot delete their own backups.** On S3-compatible storage the agent writes with a key that has no delete rights. Deletion is done by the server alone, with a two-phase cleanup that works with S3 Object Lock. See the [threat model](/docs/security/threat-model/#agent-credentials).
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/docs/quickstart/).

## At a glance

| | Kopia | Lodestore 0.2 |
|---|-------|---------------|
| What it is | Snapshot backup tool with a command-line interface and a graphical interface, designed to back up individual machines; no server is needed to run it | Self-hosted server with agents, Apache-2.0 |
| Central management | Optional server mode with an API for centrally managing backups of multiple machines; a Repository Server can proxy access to the storage | One server for all hosts: web interface, schedules and retention, alerts, search across the snapshots of every host |
| Protection from a compromised host | Object Lock on S3-API storage, enabled when the repository is created from the command line; a key without delete permission turns deletes into hidden markers; a Repository Server can give clients access that allows reading and writing but not deleting | Agent key without delete rights on S3-compatible storage; deletion is server-only; the server checks every new backup an agent makes, so index metadata forged by a compromised host cannot delete the backups of other hosts or pass off an unrestorable backup as a success; the latest snapshot is picked from signed records of which host and job made each snapshot, not from the time a host writes into it; the agent writes restored files only into folders its own configuration allows, even when the server asks for more |
| Platforms | Linux (x64, arm64, arm), macOS and Windows; FreeBSD and OpenBSD builds are marked experimental | Linux, x86_64 and arm64 |
| License | Apache-2.0 | Apache-2.0 |

### Storage

| Storage | Kopia | Lodestore 0.2 |
|---------|-------|---------------|
| Amazon S3 and S3-compatible storage | Yes | Yes |
| Local or network folder | Yes | Yes, the agent can delete in it |
| SFTP, Azure Blob Storage, Google Cloud Storage | Yes | Not yet, see the [roadmap](/roadmap/) |
| WebDAV, Google Drive, Rclone | Yes; Google Drive and Rclone are marked experimental | Not in 0.2 |
