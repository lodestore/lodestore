---
title: Lodestore vs Bareos
description: How Lodestore, a backup server built around S3-compatible object storage, compares with Bareos. Facts are taken from Bareos's own documentation.
---
Bareos is a backup system for networks of computers, a fork of Bacula 5.2 started in 2010. Lodestore is a newer system built around S3-compatible object storage.

## Why teams pick Lodestore

- **Few moving parts.** The Lodestore server is one binary with a SQLite catalog by default, or PostgreSQL for production, and the agent on each host writes straight to the bucket. In Bareos, data goes from the File Daemon through a Storage Daemon, which holds the object-store credentials.
- **Restore depends only on the bucket.** A password or recovery key and the bucket are enough for `lode` to restore, even if the server is gone.
- **Hosts that cannot delete their own backups.** On S3-compatible storage the agent writes with a key that has no delete rights. Deletion is done by the server alone, with a two-phase cleanup that works with S3 Object Lock. See the [threat model](/docs/security/threat-model/#agent-credentials).
- **Fleet view and alerts out of the box.** A web interface, schedules and retention as policies, alerts to Telegram, Slack, email and webhooks, and a fleet-wide search for a file across all hosts.
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/docs/quickstart/).

## At a glance

| | Bareos | Lodestore 0.1 |
|---|--------|---------------|
| Components | Director, a File Daemon on each client, a Storage Daemon and a PostgreSQL catalog | One server binary and an agent on each host |
| Storage | Tape, File, Fifo, Droplet (object stores, most notably S3), Dplcompat, Dedupable | S3-compatible object storage, local or network folder |
| Where object-store credentials live | In a profile on the Storage Daemon | On S3-compatible storage the agent holds a write-only key; the server holds the key that can delete |
| Protection from a compromised host | Tape and WORM media for ransomware-resilient setups | Agent key without delete rights on S3-compatible storage; deletion is server-only |
| Platforms | Linux, Windows, macOS, FreeBSD | Linux, x86_64 and arm64 |
| Sources | Files, plus File Daemon plugins for PostgreSQL, MariaDB, Percona XtraBackup, Microsoft SQL Server, Proxmox VE and VMware | Files and directories; databases through a dump in a pre-backup hook |
| Backup levels | Full, differential, incremental and Always Incremental | Deduplicated snapshots |
| License | GNU AGPLv3 | Apache-2.0 (agent, format, CLI), AGPL-3.0 (server) |
