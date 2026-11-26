---
title: Hetzner
description: Hetzner Object Storage with a bucket policy for a key from another project, and Hetzner Storage Box, where a host cannot be forbidden to delete. Neither is verified on the live service.
---

Hetzner offers two storages and Lodestore has a preset for each. The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Object Storage

### Guarantees

| | Key from another project with the Lodestore bucket policy |
|---|---|
| A host can delete backups | No: the policy allows get and list, and put only under `packs/`, `index/` and `snapshots/` |
| A host can overwrite backups | Yes unless Object Lock or versioning keeps the old version: the policy has no `If-None-Match` condition |
| What protects older data | Object Lock, or versioning |
| When a host is revoked | It keeps the key until you replace it and delete the old one in Hetzner |
| Lifecycle rules | The S3 reader runs; a reply it cannot read gives `lifecycle_unverified` |
| Level the check reports | What the check measures |
| Tested against | Not verified on the live service |

### In the web interface

1. **Storage.** Choose the **Hetzner Object Storage** tile.
2. **Connection.** **Region**, from which the wizard builds `https://<region>.your-objectstorage.com`, then **Bucket**, **Key prefix** and the server key: the access key ID and the secret of the bucket owner.
3. **Agent rights.** The step shows the commands and the bucket policy of the next section, with the agent as principal. Create the agent key in a second Hetzner project, apply the policy with the bucket owner's key, paste the agent's access key ID and secret, and choose **Check**.

**Skip, use the server key** lets agents use the owner's key; you accept the weaker level. **Limit agent rights** on the repository page shows the same commands and, after your password and a full check, switches the agents to a new key; delete the old one in Hetzner.

### Agent rights

`GET /api/v1/agent-policy?preset=hetzner-object-storage&bucket=<bucket>&prefix=<prefix>` returns commands and a bucket policy; the mechanism is `policy`. Create the agent key in a second Hetzner project, then apply the policy with the bucket owner's key (`aws s3api put-bucket-policy`). The policy names the agent as the principal `arn:aws:iam:::user/p<project>:<access key>` (placeholders `AGENT_PROJECT_ID` and `AGENT_ACCESS_KEY`) and allows `s3:PutObject`, `s3:GetObject`, `s3:GetObjectVersion` on the repository prefix and `s3:ListBucket` on the bucket. The endpoint template is `https://{region}.your-objectstorage.com`. This has not been checked against the real service. Turn on Object Lock too.

### Rotating agent credentials

Create a new key in the second project, update `AGENT_ACCESS_KEY` in the policy, and send the key with `PUT /api/v1/repositories/{id}/agent-key`. Lodestore stops handing out the old key but does not delete it: delete it in Hetzner. `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-key` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. A key equal to the server key is `server_key_shared` and is not rotated by `agent-key`.

## Storage Box

### Guarantees

| | Sub-account with the repository directory as its base |
|---|---|
| A host can delete backups | Yes: a Storage Box is managed SFTP and its sshd cannot be configured |
| A host can overwrite backups | Yes |
| What protects older data | Storage Box snapshots that you keep |
| When a host is revoked | It keeps the sub-account until you replace its key or delete it |
| Lifecycle rules | None |
| Level the check reports | Host can overwrite or delete, always |
| Tested against | Not verified on the live service. The SFTP client is tested against OpenSSH in a container |

### In the web interface

1. **Storage.** Choose the **Hetzner Storage Box** tile.
2. **Connection.** **Storage Box user**; the wizard builds the address `<user>.your-storagebox.de:23` from it and shows it. Enter the **Directory**, choose **Get host key**, compare the fingerprint with the one you know for your Storage Box and choose **It matches**; only then does the wizard ask for the **Private key** of the main account.
3. **Agent rights.** The step shows the instruction of the next section: a Storage Box cannot forbid deleting, so there are no commands. Create the sub-account, enter its user and private key, and choose **Check**. The level is **Host can overwrite or delete**.

**Skip, use the server key** makes the agents use the main account; only a new repository with a sub-account changes that, and the repository page offers no **Limit agent rights** for it. A repository created with a sub-account takes a new key through that panel.

### Agent rights

`GET /api/v1/agent-policy?preset=hetzner-storage-box` returns an instruction; the mechanism is `instruction`:

1. Create a sub-account whose base directory is the repository directory.
2. Give the agent only that sub-account; the server keeps the main account.
3. Keep Storage Box snapshots so that an overwritten or deleted object can be recovered.

The preset address is `<user>.your-storagebox.de:23`, with no `ssh://` in front: an SFTP address with a scheme is refused. The level stays **Host can overwrite or delete**, and so it does for any SFTP account: an SFTP user can keep writing to a file it holds open (`held_handle_rewrite`). Give the sub-account to the agents as `agent_options` (`user` and `key`); see [SFTP](/docs/storage/sftp/) for the options and the host key.

### Rotating agent credentials

Send the new sub-account key with `PUT /api/v1/repositories/{id}/agent-options`. The server checks it with a full probe and sends it to online agents at once, and to the others at their next connection. Lodestore stops handing out the old key but does not remove it: delete the old key or sub-account in the Storage Box settings. `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-options` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. A repository created on shared credentials answers `new_repository`: only a new repository with its own credentials closes the access.
