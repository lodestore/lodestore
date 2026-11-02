---
title: Lodestore
description: Centralized backup for Linux hosts. Data goes straight to your storage, encrypted, and restores with only the repository and a password.
---

Lodestore is a centralized backup system for Linux hosts. A lightweight agent on each host writes deduplicated, encrypted data straight to S3-compatible object storage (AWS S3, MinIO, Ceph and others) or to a local or network folder such as NFS. The server only manages and catalogs, so a restore works with nothing but the bucket and a password or recovery key, even if the server is gone.

Version 0.1 backs up files and directories. Databases are covered by [dumping them in a pre-backup hook](/docs/recipes/database-backup-hook/). Proxmox and Kubernetes backups are planned.

- [Quickstart](/docs/quickstart/): install the server, connect a host and make a first backup in about ten minutes.
- [Architecture decisions](/docs/adr/0001-serialization/): the records behind the repository format, key model and object layout.
