---
title: SFTP
description: The chrooted OpenSSH user with a deny list for agents on SFTP, why the level stays Host can overwrite or delete, and what has been tested.
---

The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/). The options, the host key and the publication modes are described there too.

## Guarantees

| | Chrooted user with a deny list |
|---|---|
| A host can delete backups | No through SFTP requests: `remove`, `rmdir`, `rename`, `posix-rename`, `setstat`, `fsetstat`, `lsetstat`, `symlink`, `copy-data` and `mkdir` are denied and new files are read-only (`-u 0222`) |
| A host can overwrite backups | Yes, through a file it keeps open: the account cannot be stopped from writing to a published file whose handle it holds |
| What protects older data | File permissions and, for the held handle, nothing on the SFTP side |
| When a host is revoked | It keeps the key until you replace it and remove the old one from `authorized_keys` |
| Lifecycle rules | None |
| Level the check reports | **Host can overwrite or delete**, always (`held_handle_rewrite`) |
| Tested against | OpenSSH in a container. Hosted SFTP and NAS: not yet |

## Agent rights

`GET /api/v1/agent-policy?preset=sftp&prefix=<absolute path>&user=<name>` returns shell commands and an `sshd_config` block; the mechanism is `commands`. The commands assume that the repository lives in `<root>/data` and that the agent sees it as `/data` inside its chroot:

- a group and a user without a shell, and the server's user added to the group;
- `<root>` owned by root, and `<root>/data` owned by the server's user, group `lode`, mode `2775` (`SERVER_USER` is a placeholder);
- the agent's public key in `authorized_keys`;
- a `Match User` block with `ChrootDirectory <root>`, `ForceCommand internal-sftp -P <deny list> -u 0222`, and no forwarding.

Create the directories `packs`, `index` and `snapshots` beforehand: the agent cannot create directories. The preset is marked verified: the commands are run against OpenSSH in a container.

## Limits

- The level never rises above **Host can overwrite or delete**, whatever the account may do otherwise. A deny list does not stop a client that holds a file open from writing to it.
- The key of the agent and the host key must be ed25519 or ECDSA.
- Hosted SFTP and NAS have not been tested.

## Rotating agent credentials

The agents' key is `agent_options.key` (with `user`). Send a new one with `PUT /api/v1/repositories/{id}/agent-options`; the server checks it in full and sends it to the agents. Lodestore stops handing out the old key but does not remove it: remove the old key from `authorized_keys`. `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-options` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. A repository on shared credentials answers `new_repository`: its hosts hold the server's key, and only a new repository with its own credentials closes the access.
