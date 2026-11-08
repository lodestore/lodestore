---
title: Lodestore vs Plakar
description: How Lodestore, a self-hosted backup server with agents, compares with Plakar and its commercial Control Plane. Facts are taken from Plakar's own documentation.
---
Plakar is an open-source backup engine with a command-line client, and a separate commercial product, Plakar Control Plane, for managing backups across an organization. Lodestore is a self-hosted server with an agent on each host.

Two products carry the Plakar name, so the table below separates them where the answer differs.

## Why teams pick Lodestore

- **Central management, self-hosted, without plans or caps.** One server with a web interface, schedules and retention, integrity checks and alerts (Telegram, Slack, email, webhooks). The server, the agent and the format are Apache-2.0.
- **Hosts that cannot delete their own backups.** On S3-compatible storage the agent writes with a key that has no delete rights. Deletion is done by the server alone, with a two-phase cleanup that works with S3 Object Lock. See the [threat model](/docs/security/threat-model/#agent-credentials).
- **Restore without the server.** The bucket and a password or recovery key are enough for `lode` to restore, and you can restore a single file from the web interface.
- **Fleet-wide search and agent updates.** Find a file across the snapshots of every host; roll out agent updates that each agent installs only after checking a signature.
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/docs/quickstart/).

## At a glance

| | Plakar community | Plakar Control Plane | Lodestore 0.2 |
|---|------------------|----------------------|---------------|
| What it is | Command-line engine, ISC license | Commercial self-hosted management system on Plakar, packaged as a virtual appliance, with a Terraform provider, an Ansible collection, a Kubernetes operator and an HTTP API described in OpenAPI | Self-hosted server with agents, Apache-2.0 |
| Protection from a compromised host | Immutable backups on object-lock storage, per the Plakar website | Immutable backups on object-lock storage, per the Plakar website | Agent key without delete rights on S3-compatible storage; deletion is server-only; the server checks every new backup an agent makes, so index metadata forged by a compromised host cannot delete the backups of other hosts or pass off an unrestorable backup as a success; the latest snapshot is picked from signed records of which host and job made each snapshot, not from the time a host writes into it; the agent writes restored files only into folders its own configuration allows, even when the server asks for more |
| Plans | None | Free (up to 500 GB of managed data), Community (for qualifying organizations) and Enterprise | None |
| Sources | Files, plus integrations such as PostgreSQL, MySQL, MongoDB, Microsoft SQL Server, etcd, Kubernetes, Proxmox, VMware and S3-compatible object stores | More resource types, including Proxmox | Files and directories; databases through a dump in a pre-backup hook |
| Storage | Local folders, S3-compatible object storage and SFTP, and more through integrations | Object storage resources: S3-compatible storage, Azure Blob Storage and Google Cloud Storage | S3-compatible object storage, or a local or network folder such as NFS |
| Platforms | Linux, macOS, Windows, FreeBSD | Virtual appliance | Linux, x86_64 and arm64 |
| Web interface | Local, started with `plakar ui` | Full web interface | Fleet web interface served by the server |
