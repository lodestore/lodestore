---
title: Lodestore vs Bareos
description: How Lodestore, a backup server built around S3-compatible object storage, compares with Bareos. Facts are taken from Bareos's own documentation. No speed figures.
---
Bareos is a backup system for networks of computers, a fork of Bacula 5.2 started in 2010. Lodestore is a newer system built around S3-compatible object storage. The Bareos facts on this page come from its documentation for version 25, read on 2026-11-01. It contains no speed or resource numbers, because we have not measured them.

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
| Platforms | Linux, Windows, macOS, FreeBSD | Linux, x86_64 and arm64 |
| Sources | Files, plus File Daemon plugins for PostgreSQL, MariaDB, Percona XtraBackup, Microsoft SQL Server, Proxmox VE and VMware | Files and directories; databases through a dump in a pre-backup hook |
| Backup levels | Full, differential, incremental and Always Incremental | Deduplicated snapshots |
| License | GNU AGPLv3 | Apache-2.0 (agent, format, CLI), AGPL-3.0 (server) |

## Bareos may fit when

- You use tape, autochangers or other removable media, or want WORM media as part of your ransomware plan.
- Your fleet includes Windows, macOS or FreeBSD, or you need ready-made plugins for databases and hypervisors.
- You want a long-established product with a vendor that sells subscriptions, support and training, and you are willing to run a Director, a Storage Daemon and a PostgreSQL catalog.

## Facts we checked

| Fact | Source | Version | Checked on |
|------|--------|---------|------------|
| The latest release is 25.1.2, published 2026-10-27. | [Bareos on GitHub](https://github.com/bareos/bareos/releases) | 25.1.2 | 2026-11-01 |
| Bareos is a network client/server backup program, a fork of Bacula, "scalable from small single computer systems to systems consisting of hundreds of computers", and is free software under the GNU AGPLv3. | [What is Bareos?](https://docs.bareos.org/IntroductionAndTutorial/WhatIsBareos.html) | 25 | 2026-11-01 |
| A backup needs four things running: the Director, a File Daemon on each client, a Storage Daemon, and a PostgreSQL server for the catalog. The File Daemon sends data to the Storage Daemon. | [What is Bareos?](https://docs.bareos.org/IntroductionAndTutorial/WhatIsBareos.html) | 25 | 2026-11-01 |
| Binary policy: subscription binaries at download.bareos.com need a subscription; community binaries at download.bareos.org are public but only the latest build is kept. Both are built from the same source, and "there are no hidden nor open core components". | [What is Bareos?, Binary Release Policy](https://docs.bareos.org/IntroductionAndTutorial/WhatIsBareos.html) | 25 | 2026-11-01 |
| The package list includes File Daemon plugins for PostgreSQL, MariaDB (mariabackup), Percona XtraBackup, Microsoft SQL Server (Windows only), Proxmox VE and VMware. The Proxmox VE plugin is marked as built only for subscriptions and available only for Debian. | [What is Bareos?, Bareos Packages](https://docs.bareos.org/IntroductionAndTutorial/WhatIsBareos.html) | 25.0.0 package list | 2026-11-01 |
| Storage backends are Tape, File, Fifo, Droplet (object stores, most notably S3), Dplcompat (a Droplet replacement with a compatible storage format, using wrapper programs such as one bundled for s3cmd), GFAPI (deprecated since 25.0.0) and Dedupable. | [Storage Backends](https://docs.bareos.org/TasksAndConcepts/StorageBackends.html) | 25 | 2026-11-01 |
| The Droplet S3 backend can only be used with virtual-hosted-style buckets, path-style buckets are not supported, and it has been tested successfully with AWS S3 and CEPH Object Gateway S3. The object-store credentials are kept in a profile on the Storage Daemon. | [Storage Backends, Droplet](https://docs.bareos.org/TasksAndConcepts/StorageBackends.html) | 25 | 2026-11-01 |
| We did not find S3 Object Lock mentioned on the Storage Backends page. This is a statement about that page, not about what Bareos can do. | [Storage Backends](https://docs.bareos.org/TasksAndConcepts/StorageBackends.html) | 25 | 2026-11-01 |
| The Windows version is a native Win32 port, and the Windows component most often used is the File Daemon (client). | [The Windows Version of Bareos](https://docs.bareos.org/TasksAndConcepts/TheWindowsVersionOfBareos.html) | 25 | 2026-11-01 |
| The website says Bareos protects Linux, Windows, macOS and FreeBSD systems, supports full, differential, incremental and Always Incremental backups across disk, tape and S3-compatible object storage, and that tape and WORM media can support ransomware-resilient strategies. It offers subscriptions, support, consulting and training. | [bareos.com](https://www.bareos.com/) | site as of check | 2026-11-01 |

What Lodestore 0.1 does is listed in the [changelog](https://github.com/lodestore/lodestore/blob/main/CHANGELOG.md), and [How we test data safety](/docs/security/how-we-test/) says which checks exist. If a statement about Bareos on this page is wrong or out of date, please [open an issue](https://github.com/lodestore/lodestore/issues).
