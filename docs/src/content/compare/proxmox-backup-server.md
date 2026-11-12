---
title: Lodestore vs Proxmox Backup Server
description: How Lodestore, a self-hosted backup server with agents, compares with Proxmox Backup Server. Facts are taken from Proxmox's own documentation.
---
Proxmox Backup Server is a client-server backup solution specially optimized for Proxmox Virtual Environment. For virtual machines and containers on Proxmox VE it is the tool to use today. Lodestore backs up the files of Linux hosts: an agent on each host writes deduplicated, encrypted data straight to S3-compatible storage, and the server manages and catalogs.

## Why teams pick Lodestore

- **Files of Linux hosts, straight to S3.** The agent writes to S3-compatible storage itself. The server stays out of the data path, so there is no datastore to set up and no local cache to provide for the object store.
- **One server for the fleet.** Schedules and retention, integrity checks and alerts (Telegram, Slack, email, webhooks) are set in one web interface, and you can find a file across the snapshots of every host.
- **Apache-2.0 end to end.** The server, the agent and the format are Apache-2.0.
- **A short path to the first backup.** One server image and a one-line install per host; see the [Quickstart](/docs/quickstart/).

## At a glance

| | Proxmox Backup Server | Lodestore 0.2 |
|---|-----------------------|---------------|
| What it is | Client-server backup solution for virtual machines, containers and physical hosts, specially optimized for Proxmox Virtual Environment | Self-hosted server with agents, Apache-2.0 |
| Proxmox VM and CT backup | Yes, with an integrated client for QEMU and LXC in Proxmox VE | Not yet, see the [roadmap](/roadmap/) |
| Data path | The server stores the backup data and provides an API that the client uses; with an S3 backend the datastore also keeps a local persistent cache | The agent writes straight to S3-compatible storage; the server manages and catalogs |
| Backup client without delete rights | The `Datastore.Backup` privilege allows only to create, but not to delete or alter existing backups | Agent key without delete rights on S3-compatible storage; deletion is server-only |
| Restore without a running server | `proxmox-backup-debug recover` restores from intact index and chunk files, with the key file for encrypted backups | `lode` restores with the bucket and a password or recovery key |
| Client platforms | Linux, documented for amd64 environments and most modern Linux systems; a statically linked client for systems where the regular one is not available | Linux, x86_64 and arm64 |
| License | AGPL v3 | Apache-2.0 |

### Storage

| Storage | Proxmox Backup Server | Lodestore 0.2 |
|---------|-----------------------|---------------|
| Local folder | Yes, a datastore on ext4, xfs or zfs | Yes, the agent can delete in it |
| S3-compatible object storage | Yes, as a datastore backend with a local persistent cache | Yes |
