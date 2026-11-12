---
title: Frequently asked questions
description: Answers about restore, security, the license and daily operation of Lodestore, including what a taken-over host can do and what is not built yet.
---
## Data and restore

### What happens to my backups if the Lodestore server is lost?

Nothing happens to the data. It sits in your bucket, encrypted, and the server only manages and catalogs it. To restore you need the bucket, a key that can read it, and the repository password or the recovery key. Install the `lode` client on any Linux machine and restore from there. See [Restore without the server](/docs/recipes/restore-without-server/).

Losing the server's unlock key loses the server's access to the repositories, not the data. See [Losing the unlock key](/docs/security/threat-model/#losing-the-unlock-key).

### What if I lose the repository password?

The recovery key opens the repository the same way the password does, so keep both. If you have neither, nobody can open the data, the project included. That is a consequence of the encryption, not a gap that support can close. See [Restore without the server](/docs/recipes/restore-without-server/#what-you-need).

### What is the recovery key and where do I get it?

The recovery key is a text that starts with `lode-rk1-` and encodes the master key of the repository. The setup wizard shows it once, when you create the repository. Store it outside the server, in a password manager or a vault, and never in the same backup set as the catalog. `lode` accepts it wherever it accepts a password. See [ADR 0002](/docs/adr/0002-key-model-and-envelope/).

### Can I read my backups without Lodestore?

You need `lode`, the command line client. It is an open source static binary under Apache-2.0 that runs on any Linux machine and needs no server, no agent and no database. The repository layout is documented, so the format is not a secret kept by the server. See [Binary](/docs/install/binary/) and [ADR 0003](/docs/adr/0003-object-layout/).

## Security

### What can a taken-over host do to my backups?

The agent on a host holds a write-only S3 key. It can add data and cannot read it back or delete it, and deletion and cleanup run only on the server. Write-only does not mean no overwrite: the generated AWS policy refuses a replacement of an existing object, while MinIO and a folder repository do not, so there a stolen agent key can replace an object. Object Lock or versioning keeps the earlier version. A local folder gives the agent delete rights, so use it for trying Lodestore, not for real backups. See [Agent credentials](/docs/security/threat-model/#agent-credentials).

### How does Object Lock help?

Object Lock makes S3 refuse to delete or overwrite an object version until its retention period ends, whoever asks. With it, even a stolen server key cannot destroy backups that are still inside the period. Lodestore does the deleting only on the server and works with Object Lock. The tests cover Object Lock against MinIO; test your setup on a throwaway bucket before you rely on it. See [S3 access keys and Object Lock](/docs/recipes/s3-iam-object-lock/#turn-on-object-lock).

### How is my data encrypted?

The agent compresses and encrypts data on the host before anything is sent to storage. Data is sealed with XChaCha20-Poly1305 under a fresh key per pack, and that key is sealed to the repository public key with HPKE (X-Wing, a hybrid of X25519 and ML-KEM-768). The password opens the repository through Argon2id key slots. The bucket never holds the master key in the clear. See [ADR 0002](/docs/adr/0002-key-model-and-envelope/).

### Why does the agent run as root?

To read every file it backs up and to restore owners and permissions. A restore job writes only under the restore folders that the agent allows, and the agent checks the destination of every job itself, whoever started it. The folders and the in-place switch are settings of `agent.yaml`. See [Binary](/docs/install/binary/) and the [threat model](/docs/security/threat-model/#agent-updates).

## Project and license

### What license is Lodestore under?

Apache-2.0, for every part of the repository: the server, the agent, the `lode` client and the format library. The text is in the [LICENSE file](https://github.com/lodestore/lodestore/blob/main/LICENSE).

### How stable is the repository format in 0.x?

Format v1 is pinned by golden test vectors, and CI allows the v1 fixtures to be added to but never changed. A change that old repositories could not survive would ship as `format_version` 2 next to the old one. Lodestore is still 0.x, so read the [changelog](https://github.com/lodestore/lodestore/blob/main/CHANGELOG.md) before you update.

### How can I contribute?

Open an issue or a pull request on GitHub. Every commit is signed off under the Developer Certificate of Origin with `git commit -s`, and CI checks it. [CONTRIBUTING.md](https://github.com/lodestore/lodestore/blob/main/CONTRIBUTING.md) has the details, including the rules for changes to the repository format.

## Operations

### Which storage can I use?

S3-compatible object storage, such as AWS S3, MinIO or Ceph, or a local or network folder. Only S3-compatible storage gives the agent a separate key without delete rights and offers Object Lock, which is why a folder is meant for trying Lodestore. See [Why S3-compatible storage](/docs/quickstart/#why-s3-compatible-storage).

### Should I use SQLite or PostgreSQL for the catalog?

The server keeps its catalog in SQLite by default, on a local disk. PostgreSQL is the choice for production: set `LODESTORE_DATABASE_URL`, or `externalDatabase` in the Helm chart. The data itself is in the bucket either way. See [Docker](/docs/install/docker/) and [Helm](/docs/install/helm/).

### How many hosts can one server handle?

Lodestore is designed for fleets from a few hosts to a few hundred Linux hosts. No test measures speed, memory or behavior at that size yet, so size the server and the bucket for your own load and try it first. See [What we have not verified](/docs/security/how-we-test/#what-we-have-not-verified).

### What do I need to know about proxies and the network?

The server has two ports. Port 8080 serves the web interface and the API over plain HTTP, so put an HTTPS reverse proxy in front of it. Port 8443 carries the agents' gRPC with mutual TLS and must be forwarded as plain TCP or TLS passthrough, never through a proxy that terminates TLS. An agent behind a proxy honours `HTTPS_PROXY`. See [Networking](/docs/reference/networking/).

### What does Lodestore not do yet?

Not yet: agents for Windows and macOS, backup of Proxmox VMs and containers, a Kubernetes operator, native database backup (today, dump the database in a pre-backup hook), roles for teams (RBAC) and single sign-on, and storage such as SFTP and WebDAV. The [roadmap](/roadmap/) lists what is next and what is later.
