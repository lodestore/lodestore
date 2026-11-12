---
title: Roadmap
description: What Lodestore does today, what comes next and what comes later. The roadmap is public and changes together with the code.
ready:
  - Linux hosts, x86_64 and arm64
  - AWS S3, MinIO, Ceph, any S3-compatible storage, local or NFS folder
  - Server as a container, a static binary or a Helm chart
  - SQLite catalog by default, PostgreSQL for production
limits:
  - title: Proxmox VM and CT backup
    status: Next
  - title: Kubernetes operator
    status: Next
  - title: Native database backup (PostgreSQL first)
    status: Next
    note: Today, dump the database in a pre-backup hook and back up the dump.
  - title: Roles for teams (RBAC)
    status: Next
    note: Today, API tokens carry scopes that limit what each token can do.
  - title: Windows and macOS agents
    status: Later
  - title: SFTP, WebDAV and other storage
    status: Later
    note: Today, S3-compatible storage or a local or network folder.
  - title: Single sign-on
    status: Later
---
The roadmap is public and lives in the repository, so it changes together with the code. It lists directions, not dates. Missing something? [Open an issue on GitHub](https://github.com/lodestore/lodestore/issues).

## Now

The current release is about one guarantee: a host that has been taken over cannot erase or forge its backup history.

- **The server checks every new backup.** After a job finishes, the server opens the new data the snapshot depends on and verifies that the snapshot resolves.
- **Signed snapshot provenance.** Each snapshot carries a signed record of which host and job made it, and the latest snapshot is picked from those records.
- **Tight token scopes.** Restore rights are split into download, restore to a new folder and restore in place, and dangerous actions in the web interface ask for the password again.
- **A signed agent installer.** `install.sh` verifies the agent's minisign signature with the project key.
- **Limits in the format.** Every object read from the bucket is read with a bounded size, so a planted oversized object cannot exhaust a server or an agent.
- **Apache-2.0 everywhere.** The server is now licensed under Apache-2.0, like the agent, the CLI and the format.
- **Documentation and a public site.** Quickstart, guides, a threat model, a comparison with other tools and a read-only [demo](https://demo.lodestore.dev).
