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
