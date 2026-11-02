---
title: Lodestore vs Plakar
description: How Lodestore, a self-hosted backup server with agents, compares with Plakar and its commercial Control Plane. Facts are taken from Plakar's own documentation.
---
Plakar is an open-source backup engine with a command-line client, and a separate commercial product, Plakar Control Plane, for managing backups across an organization. Lodestore is a self-hosted server with an agent on each host.

Two products carry the Plakar name, so the table below separates them where the answer differs.

## Why teams pick Lodestore

- **Central management, self-hosted, without plans or caps.** One server with a web interface, schedules and retention, integrity checks and alerts (Telegram, Slack, email, webhooks). The server is AGPL-3.0 and the agent and format are Apache-2.0. Plakar's community edition is scheduled through cron, systemd timers or the platform scheduler; its documentation puts centralized scheduling in Control Plane.
- **Hosts that cannot delete their own backups.** On S3-compatible storage the agent writes with a key that has no delete rights. Deletion is done by the server alone, with a two-phase cleanup that works with S3 Object Lock. See the [threat model](/docs/security/threat-model/#agent-credentials).
- **Restore without the server.** The bucket and a password or recovery key are enough for `lode` to restore, and you can restore a single file from the web interface.
- **Fleet-wide search and agent updates.** Find a file across the snapshots of every host; roll out agent updates that each agent installs only after checking a signature.
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/docs/quickstart/).

## At a glance

| | Plakar community | Plakar Control Plane | Lodestore 0.1 |
|---|------------------|----------------------|---------------|
| What it is | Command-line engine, ISC license | Commercial self-hosted management system on Plakar, packaged as a virtual appliance | Self-hosted server with agents, AGPL-3.0 server and Apache-2.0 agent and format |
| Protection from a compromised host | Immutable backups on object-lock storage, per the Plakar website | Immutable backups on object-lock storage, per the Plakar website | Agent key without delete rights on S3-compatible storage; deletion is server-only |
| Scheduling | cron, systemd timer or the platform scheduler | Centralized scheduling | Server-side schedules with retention |
| Plans | None | Free (up to 500 GB of managed data), Community (for qualifying organizations) and Enterprise | None |
| Sources | Files, plus integrations such as PostgreSQL, MySQL, etcd, Kubernetes and S3-compatible object stores | More resource types, including Proxmox | Files and directories; databases through a dump in a pre-backup hook |
| Platforms | Linux, macOS, Windows, FreeBSD | Virtual appliance | Linux, x86_64 and arm64 |
| Web interface | Local, started with `plakar ui` | Full web interface | Fleet web interface served by the server |
