---
title: Lodestore vs Plakar
description: How Lodestore, a self-hosted backup server with agents, compares with Plakar and its commercial Control Plane. Facts are taken from Plakar's own documentation. No speed figures.
---
Plakar is an open-source backup engine with a command-line client, and a separate commercial product, Plakar Control Plane, for managing backups across an organization. Lodestore is a self-hosted server with an agent on each host. The Plakar facts on this page come from its README and documentation, community version 1.1.8, read on 2026-11-01. It contains no speed or resource numbers, because we have not measured them.

Two products carry the Plakar name, so the table below separates them where the answer differs.

## Why teams pick Lodestore

- **Central management, self-hosted, without plans or caps.** One server with a web interface, schedules and retention, integrity checks and alerts (Telegram, Slack, email, webhooks). The server is AGPL-3.0 and the agent and format are Apache-2.0. Plakar's community edition is scheduled through cron, systemd timers or the platform scheduler; its documentation puts centralized scheduling in Control Plane.
- **Hosts that cannot delete their own backups.** On S3-compatible storage the agent writes with a key that has no delete rights. Deletion is done by the server alone, with a two-phase cleanup that works with S3 Object Lock. See the [threat model](/lodestore/security/threat-model/#agent-credentials).
- **Restore without the server.** The bucket and a password or recovery key are enough for `lode` to restore, and you can restore a single file from the web interface.
- **Fleet-wide search and agent updates.** Find a file across the snapshots of every host; roll out agent updates that each agent installs only after checking a signature.
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/lodestore/quickstart/).

## At a glance

| | Plakar community | Plakar Control Plane | Lodestore 0.1 |
|---|------------------|----------------------|---------------|
| What it is | Command-line engine, ISC license | Commercial self-hosted management system on Plakar, packaged as a virtual appliance | Self-hosted server with agents, AGPL-3.0 server and Apache-2.0 agent and format |
| Scheduling | cron, systemd timer or the platform scheduler | Centralized scheduling | Server-side schedules with retention |
| Plans | None | Free (up to 500 GB of managed data), Community (for qualifying organizations) and Enterprise | None |
| Sources | Files, plus integrations such as PostgreSQL, MySQL, etcd, Kubernetes and S3-compatible object stores | More resource types, including Proxmox | Files and directories; databases through a dump in a pre-backup hook |
| Platforms | Linux, macOS, Windows, FreeBSD | Virtual appliance | Linux, x86_64 and arm64 |
| Web interface | Local, started with `plakar ui` | Full web interface | Fleet web interface served by the server |

## Plakar may fit when

- You want to back up databases, Kubernetes or object stores through integrations, or hosts running macOS, Windows or FreeBSD.
- You want to diff two snapshots, verify one without restoring it, keep a snapshot as a `.ptar` archive, or sync a store to other locations for a 3-2-1 setup.
- You want a local tool with its own web interface and no server to run, or a vendor-supported management layer with role-based access, compliance features, an audit log and legal hold, which are documented for Control Plane.

## Facts we checked

| Fact | Source | Version | Checked on |
|------|--------|---------|------------|
| Plakar is licensed ISC; the latest release is v1.1.8, published 2026-10-20. | [Plakar on GitHub](https://github.com/PlakarKorp/plakar) | v1.1.8 | 2026-11-01 |
| The README describes Plakar as an open-source backup solution built on Kloset and ptar, with browsable, independently verifiable, deduplicated and compressed snapshots, encryption of data and metadata, and a web interface started with `plakar ui`. | [Plakar README](https://github.com/PlakarKorp/plakar/blob/main/README.md) | v1.1.8 | 2026-11-01 |
| Integrations named in the README include PostgreSQL, MySQL, etcd, Kubernetes and S3-compatible object stores, installed with `plakar pkg add`. | [Plakar README](https://github.com/PlakarKorp/plakar/blob/main/README.md) | v1.1.8 | 2026-11-01 |
| "Plakar does not run as a daemon." Scheduled backups run from a systemd timer or cron, with a guide for scheduling daily backups. | [Plakar README](https://github.com/PlakarKorp/plakar/blob/main/README.md) | v1.1.8 | 2026-11-01 |
| The README links an audit of the cryptography. We did not read the audit itself. | [Plakar README](https://github.com/PlakarKorp/plakar/blob/main/README.md) | v1.1.8 | 2026-11-01 |
| Installation on macOS (Homebrew), Debian and Ubuntu, other Linux packages, a pre-built Windows binary and Go install is documented; the download page lists Linux, macOS and FreeBSD builds. | [Installation](https://www.plakar.io/docs/community/v1.1.0/quickstart/installation/), [Download](https://www.plakar.io/download/) | 1.1.x docs, v1.1.8 builds | 2026-11-01 |
| The Plakar website states that on object-lock storage backups are immutable. We did not find Object Lock settings or tests in the pages we read, and we did not test it. | [plakar.io](https://www.plakar.io/) | site as of check | 2026-11-01 |
| Control Plane is described as a self-hosted backup management system built on Plakar, packaged as a virtual appliance. On first deployment an enrollment step registers the instance with plakar.io to retrieve a license and set up billing reporting; the page says no backup data is transferred, only consumption metrics. The overview also says Control Plane adds a full web interface, centralized scheduling and inventory on top of Plakar. | [Control Plane overview](https://www.plakar.io/docs/control-plane/intro/overview/) | Control Plane docs as of check | 2026-11-01 |
| Control Plane has three plans. Free supports up to 500 GB of managed data. Community is for organizations that qualify under listed conditions (for example startups under three years old and pre-Series A). Enterprise is consumption-based with premium support; the page gives no price and directs you to sales. | [Billing & Plans](https://www.plakar.io/docs/control-plane/intro/billing/) | Control Plane docs as of check | 2026-11-01 |
| The Control Plane documentation lists scheduling (policy and user schedulers), job history, access control with many roles, compliance (SLA policies, data residency, legal hold, audit log), secret-provider integrations and, among resource types, Proxmox. | [Control Plane documentation](https://www.plakar.io/docs/control-plane/) | Control Plane docs as of check | 2026-11-01 |

What Lodestore 0.1 does is listed in the [changelog](https://github.com/lodestore/lodestore/blob/main/CHANGELOG.md), and [How we test data safety](/lodestore/security/how-we-test/) says which checks exist. If a statement about Plakar on this page is wrong or out of date, please [open an issue](https://github.com/lodestore/lodestore/issues).
