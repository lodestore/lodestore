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

## In the web interface

1. **Storage.** Choose the **SFTP (OpenSSH)** tile.
2. **Connection.** **Address** as `host` or `host:port`, with no `ssh://` or `sftp://` (an address with a scheme is refused), and **Directory**, an absolute path on the server. Choose **Get host key**. The server makes the SSH handshake and shows the key type and the fingerprint, and nothing else is sent. Compare the fingerprint with `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the SFTP server and choose **It matches**; **It does not match** stops you. Only then does the wizard ask for the server's **User** and **Private key**, so the private key is sent after you have confirmed. A change of the address or the directory drops the confirmed key.
3. **Agent rights.** The step shows the commands and the `sshd_config` block of this page, with the agent's **User** (default `lodestore-agent`). Run them, paste the agent's private key, and choose **Check**. The check creates the folders of the repository with the server's key, so the directory may be empty. The level is **Host can overwrite or delete**.

**Skip, use the server key** makes the agents log in as the server's user. You accept the level, and only a new repository with its own user and key changes that: the repository page offers no **Limit agent rights** for it. A repository created with its own agent key takes a new key through that panel, after your password and a full check. **Check host key again** on the repository page shows the key the server presents now; if it differs from the pinned one, the dialog shows both fingerprints.

## Agent rights

`GET /api/v1/agent-policy?preset=sftp&prefix=<absolute path>&user=<name>` returns shell commands and an `sshd_config` block; the mechanism is `commands`. The commands assume that the repository lives in `<root>/data` and that the agent sees it as `/data` inside its chroot:

- a group and a user without a shell, and the server's user added to the group;
- `<root>` owned by root, and `<root>/data` owned by the server's user, group `lode`, mode `2775` (`SERVER_USER` is a placeholder);
- the agent's public key in `authorized_keys`;
- a `Match User` block with `ChrootDirectory <root>`, `ForceCommand internal-sftp -P <deny list> -u 0222`, and no forwarding.

Do not create the repository's directories yourself: the storage check creates `packs`, `index`, `snapshots` and `tmp` with the server's key, creating the repository adds the rest of the layout, and the agent never creates a directory. The preset is marked verified: the commands are run against OpenSSH in a container.

## Limits

- The level never rises above **Host can overwrite or delete**, whatever the account may do otherwise. A deny list does not stop a client that holds a file open from writing to it.
- The key of the agent and the host key must be ed25519 or ECDSA.
- Hosted SFTP and NAS have not been tested.

## Rotating agent credentials

The agents' key is `agent_options.key` (with `user`). Send a new one with `PUT /api/v1/repositories/{id}/agent-options`; the server checks it in full and sends it to the agents. Lodestore stops handing out the old key but does not remove it: remove the old key from `authorized_keys`. `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-options` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. A repository on shared credentials answers `new_repository`: its hosts hold the server's key, and only a new repository with its own credentials closes the access.
