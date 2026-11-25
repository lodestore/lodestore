---
title: "Storage backends and guarantee levels"
description: "Where a Lodestore repository can live, the options of each backend, what the storage check measures, which level it reports, and which parts have been verified only on emulators."
---
A repository lives on S3 or an S3-compatible storage, Azure Blob Storage, Google Cloud Storage, an SFTP server or a local folder. The agent writes to it directly, so what a host that has been taken over can do to its own backups depends on the storage and on the credentials the host holds. Lodestore measures this when you create or connect a repository and reports it as a **guarantee level**. This page lists the backends, the levels, how the measurement works, and what has not been verified on a real service. For the threat behind it see [Agent credentials](/docs/security/threat-model/#agent-credentials).

The web wizard creates S3 and local-folder repositories. Azure Blob Storage, Google Cloud Storage and SFTP repositories are created through [the API](/docs/reference/api/#repositories-and-the-storage-guarantee) or with `lode repo init`; the agents of a server use all five backends.

## Backends

| Backend | Location | Secret options |
|---------|----------|----------------|
| S3 and compatible (AWS, MinIO, Ceph) | `bucket`, `endpoint`, `region`, `path_style`, `prefix` | the server key and the agent key |
| S3 on Backblaze B2 | the S3 options and `provider = b2` | the server key and the agent key |
| Azure Blob Storage | `container`, `root`, `endpoint`, `account_name`, `encryption_key_sha256`, `encryption_algorithm` | `sas_token`, or `account_key` (with `account_name`); `encryption_key` for a customer-provided key |
| Google Cloud Storage | `bucket`, `root`, `endpoint`, `credential_path`, `skip_signature`, `allow_anonymous`, `disable_vm_metadata`, `disable_config_load` | `credential` (the service account key as JSON), or `token` (an OAuth access token) |
| SFTP | `endpoint` (`host:port`), `user`, `host_key`, `root`, `publish` | `key` (the private key) |
| Local folder | `path` (absolute) | none |

- **Only these options** are accepted, spelled exactly as in the table. A new repository with any other option, including another spelling (`Token`) or an alias that the storage library would also read (`sas_key`, `azure_storage_account_key`), is refused with a message that names the option. A repository that is already stored opens as before.
- **Secrets** are sealed in the server database on their own and never appear in an API response. On the command line they come from `LODE_*` environment variables; see [Repository location](/docs/reference/cli/#repository-location).
- **Backblaze B2** is connected only with `provider = b2`. A location whose endpoint is `backblazeb2.com` or a subdomain of it without that option is refused, because B2 does not apply `If-None-Match` and a delete there hides the file instead of removing it. With the option Lodestore writes without `If-None-Match`, and the cleanup does not remove objects that are only hidden. A snapshot hidden by anyone but the server stops cleanup until it is brought back or removed, so a host cannot make cleanup delete its data (see [Cleanup](/docs/guides/gc/#sftp-and-backblaze-b2)).
- **Azure Blob Storage and Google Cloud Storage** take the `root` as a folder inside the container or bucket, and it is part of the address: `azblob://<container>/<root>`, `gcs://<bucket>/<root>`. An agent never reads Google credentials from its own host (the environment, well-known files or VM metadata) and never reads `credential_path` from a file; it signs with what the server sent. A Google Cloud Storage draft without `credential` (or `token`, `skip_signature` or `allow_anonymous`) is refused with `422`.
- **SFTP** needs the key of the server in `host_key`, one line of `ssh-keyscan -p <port> <host>` output. You can type it in beforehand, or let the server show it: a check of a draft without `host_key` makes the SSH handshake only, without authenticating or writing, and returns the key and its fingerprint. At first contact Lodestore only shows the key; trusting it is your decision, so compare the fingerprint with the one the server's administrator knows before you send the key back. The key is then pinned. A server that presents another key is a connection error that names both fingerprints, and raises the alert `host_key_changed` (see [Alerts and notifications](/docs/guides/alerts/)); the key is never replaced by itself. A new key is pinned only by an explicit request, which needs the password entered in the last 15 minutes and a full check with the new key, and the agents then get it from the server. An agent of an older version cannot take a new key; the answer counts such agents as `needs_update`. The user key and the host key must be ed25519 or ECDSA; RSA keys, keys with a passphrase, host certificates and passwords are not supported, and a server that has only an RSA host key is refused with a permanent error.
- **Separate agent credentials.** A repository on Azure Blob Storage, Google Cloud Storage or SFTP may have `agent_options`, the secrets that its agents use instead of the server's: `sas_token` (Azure), `credential` (Google Cloud Storage), `user` and `key` (SFTP; the options need a `key`). The Azure account key is never given to an agent, only a SAS. Details and the replacement of these credentials are in [the API reference](/docs/reference/api/#repositories-and-the-storage-guarantee).
- **Restoring** from Azure Blob Storage, Google Cloud Storage and SFTP with only the storage and the password or recovery key works as for S3; see [Restoring without the server](/docs/recipes/restore-without-server/). Through the server, an agent reads packs by signed link where the storage can issue one (S3), and otherwise the server relays the ranges.

### SFTP publication

`publish` decides how a file becomes visible under its final name.

- **`hardlink`** (the default). The file is written under a random name in `tmp/` with the mode `0444`, flushed with `fsync@openssh.com` when the server offers it, and linked to the final name with `hardlink@openssh.com`. The link fails when the name exists, so a file is never replaced, and a cut-off upload leaves a file in `tmp/`, not under the final name. The server must offer `hardlink@openssh.com`; if it does not, Lodestore stops with an error that names `publish = direct` and never switches by itself. A cleanup cycle removes files in `tmp/` that are older than the pause and than 24 hours.
- **`direct`**. The final name is created exclusively and written, for servers without a hardlink request. A cut-off upload can leave a short file, or a file of full size with a hole, under the final name. Lodestore never counts such a file as the pack it was meant to be: it compares the size and the bytes and writes the pack again under a new name, and the broken file stays in the storage, named by no index shard. An index shard or a snapshot cut off in `direct` mode stays under its final name, and the [integrity checks](/docs/guides/checks/) report it as unreadable until someone deletes it. The level of such a repository is at most **Host can overwrite or delete** (`no_atomic_publish`).

An agent on SFTP cannot be stopped from keeping a published file open and writing to it. The server cannot revoke that handle (`held_handle_rewrite`), so an SFTP repository is never reported above **Host can overwrite or delete**, whatever the account may do otherwise.

## Guarantee levels

The check writes small objects with the credentials the agents will use, tries to write the same object twice and to delete it, once in each of `packs/`, `index/` and `snapshots/`, and reads the object's earlier versions where the storage keeps them. The objects it creates have names no reader looks at, and it removes them afterwards with the credentials it has.

| Level | What a host can do | What it needs from the storage |
|-------|--------------------|--------------------------------|
| **Create-only enforced** | create objects; neither replace nor delete them | the service refuses the second write and the delete for the agent's credentials |
| **Overwrite keeps old version** | replace objects, but the storage keeps the earlier version | bucket versioning or Object Lock on S3 (or a compatible storage), no lifecycle rule that deletes versions |
| **Host can overwrite or delete** | change or delete backups | nothing: this is the level of a folder, of every SFTP repository, and of any storage where the agent's credentials can delete or overwrite without a kept version |
| **Unchecked** | not known: the check timed out, the storage was unreachable or an answer settled nothing | a repeat of the check |

The reasons behind a level are returned as codes (`guarantee_notes`); the list is in [the API reference](/docs/reference/api/#repositories-and-the-storage-guarantee).

What each level means in practice:

- **Overwrite keeps old version** does not protect against deletion by itself and does not restore anything by itself. After an overwrite the earlier bytes remain in the storage until the server, a lifecycle rule or someone with access to the bucket removes them. A host can also hide an object, for example with a delete marker. Recovery after such an attack is manual: roll the objects back to the earlier version with the tools of the storage. Lodestore does not pick an earlier version on its own.
- **A lifecycle rule** that deletes objects or their earlier versions lowers the level to **Host can overwrite or delete** (`lifecycle_deletes`). When the rules cannot be read, a level that rests on kept versions is lowered the same way (`lifecycle_unverified`); the other levels only carry the note. The server reads the lifecycle rules of S3 buckets. `lode` does not read them, so for S3, Azure Blob Storage and Google Cloud Storage it shows `lifecycle_unverified` as well. The server does not read the lifecycle rules of Azure Blob Storage and Google Cloud Storage either, so a repository there always carries `lifecycle_unverified`: **Create-only enforced** stays with the note, and a level that rests on kept versions is lowered. The reader of the rules has been tried against MinIO only; a service that does not answer the request, as the S3 API of Backblaze B2 may not, gives `lifecycle_unverified`. Only a `LifecycleConfiguration` document, or the error `NoSuchLifecycleConfiguration`, counts as an answer; any other reply, such as a bucket listing or another `404`, gives `lifecycle_unverified` too.
- **Shared credentials.** The agent of an Azure Blob Storage, Google Cloud Storage or SFTP repository that has no `agent_options` writes with the same credentials as the server, and these can delete. The level of such a repository is therefore at most **Host can overwrite or delete**, with the reason `shared_credentials`: "Hosts use the server's own credentials for this storage, so they can do anything the server can." A compromised host can destroy the backups in that storage. With `agent_options` the check measures the level with the agent's own rights, so a SAS or a service account that can only create gives **Create-only enforced**; an SFTP repository stays at **Host can overwrite or delete** because of `held_handle_rewrite`. The server does not generate restricted rights for the agent: you create the SAS, the service account or the SFTP user, and an SFTP user that cannot create directories needs the `packs`, `index` and `snapshots` folders to exist already. Give a repository its own agent credentials when you create it: a repository created on shared credentials keeps them, because every host enrolled in it already holds the server's credentials, so it moves to separate agent credentials only as a new repository. To rotate the agent credentials of a repository that has them, send `PUT /api/v1/repositories/{id}/agent-options`: the server checks them in full first, keeps the old ones when that fails, and sends the new location to online agents at once and to the others at their next connection.
- **A local folder** is always **Host can overwrite or delete** (`not_enforceable`): every host that can write to it can also change it.

### Consent

A repository whose measured level is **Host can overwrite or delete** or **Unchecked** is not created or connected until a person accepts it:

- API: `POST /api/v1/repositories/wizard` and `.../connect` answer `409` with the code `weaker_guarantee` unless the draft has `accept_weaker_guarantee: true`. Nothing is written to the storage before that.
- Command line: `lode repo init` stops and creates nothing unless `--accept-weaker-guarantee` is given.
- Web: the wizard asks for a checkbox.

The levels above them need no consent. A repository the server already knew keeps the level that was accepted for it.

### Checking again

A manual check measures the storage again and stores the measured level and its reasons. The accepted level changes by one rule: it rises when the measured level is higher, and otherwise stays. A measured level below the accepted one is shown on the repository page, with an action to accept the new level. A level of **Host can overwrite or delete** is never accepted by a check on its own, and a check that settles nothing changes neither level.

The server also checks every repository by itself once a day, with the same probe. The first pass starts ten minutes after the server starts, and a repository is due when its last recorded result or last attempt is a day old. The alert `guarantee_downgraded` ([Alerts and notifications](/docs/guides/alerts/)) is raised for one of five causes:

- the measured level is below the accepted one (`lower`);
- the accepted level is **Unchecked** and the measured one is **Host can overwrite or delete** (`weak_unaccepted`);
- the probe ran but measured nothing three times in a row (`inconclusive`);
- the check failed three times in a row before it recorded a result (`recheck_failing`);
- the daily check has not run for more than two days (`recheck_not_running`).

One failed check or one inconclusive probe changes nothing. A probe that started before the credentials, the host key or the agent options were replaced, or that finished after a newer result was stored, is discarded and does not overwrite anything. A changed SFTP host key raises its own alert, `host_key_changed`, instead of the two causes about missing measurements.

After an update, an existing repository shows **Unchecked** as its measured level until its first check. The level it had before, taken from the state of its agent key, is kept as the accepted level. The first daily check after the update can raise `guarantee_downgraded` for repositories that existed before: when the accepted level is **Unchecked** and the check measures **Host can overwrite or delete**, the cause is `weak_unaccepted`. Accept the level on the repository page if you chose it, or fix the storage. `recheck_failing` does not appear at once: it needs three failed checks in a row, as for a new repository. Agents need the new version to take a new SFTP host key and new agent credentials from the server; an older agent is counted in `needs_update` when you replace them, and keeps the old values.

**Traces of the check.** Each check writes a few small objects under `packs/probe-`, `index/probe-` and `snapshots/probe-` with random names that no reader looks at: one written with the server's credentials, one per prefix for the agent's rights where they are measured, and up to two more on SFTP. Afterwards the server removes them with its own credentials, every version of each, and waits at most 10 seconds for that. What it cannot remove stays: under Object Lock a locked version stays until its retention ends, a storage that does not let the server delete versions keeps them, and nothing is removed when the server stops during the check or the daily check is cut off after two minutes. The server logs each object it could not remove. On such storage a daily check can leave a few small objects per repository each day.

## What the check does not see

- **Where the request comes from.** The server runs the check from its own network with the credentials that hosts receive. A policy that depends on the source of a request (an IP condition, a private endpoint) can allow a host something the check did not see, or refuse it something the check allowed.
- **Rights over the bucket configuration.** The check tries to write, overwrite and delete objects. It does not test whether the agent key holds `s3:PutLifecycleConfiguration`, `s3:PutBucketVersioning`, `s3:PutBucketPolicy` or `s3:BypassGovernanceRetention`. A host with such a key can add a rule that deletes objects or their earlier versions, suspend versioning, change the bucket policy, or remove versions locked in GOVERNANCE mode, and the level stays as measured until the next check; data removed in between is gone. Give the agent key only the rights of the policy that Lodestore shows.

## Verification status

Each cell says what Lodestore does and what has been run. "Not yet" means the behavior of the real service has not been tried.

| Backend | What the service enforces | What the Lodestore client does | Level the check gives | Verified |
|---------|---------------------------|--------------------------------|-----------------------|----------|
| Local folder | nothing | creates a file by a hard link, never truncates | Host can overwrite or delete | on a real folder |
| S3, MinIO | a key without delete rights; the AWS policy denies a write without `If-None-Match`; MinIO has no such condition key | writes with `If-None-Match: *` | Create-only enforced, or Overwrite keeps old version on a versioned bucket | MinIO (pinned). The `Deny` of the AWS policy has no test, and real AWS is not yet verified |
| Backblaze B2 | bucket versioning; a key that can write can also hide an object | writes without `If-None-Match`, no delete, no multipart upload | Overwrite keeps old version at best | the client on MinIO with the B2 preset. Real Backblaze B2: not yet |
| Azure Blob Storage | a SAS with only create, read and list rights refuses overwrite and delete | `Put Blob` with `If-None-Match: *` in one request, no block upload | Create-only enforced with such a SAS as `agent_options`, with the lifecycle note; without them at most Host can overwrite or delete | Azurite. Real Azure: not yet |
| Google Cloud Storage | an IAM role without overwrite and delete rights | every upload starts with `ifGenerationMatch=0` | at most Host can overwrite or delete without `agent_options` | the client on fake-gcs-server. The refusal on the service side is **not tested**, because fake-gcs-server has no IAM. Real Google Cloud Storage: not yet |
| SFTP, `hardlink` | file permissions of the account | exclusive create in `tmp/`, then a hard link; no truncating open, remove or rename; creates a directory only when the link finds the target's directory missing; the layout, `packs/00` to `packs/ff` included, is made when the repository is created | Host can overwrite or delete (held handle) | OpenSSH in a container. Hosted SFTP and NAS: not yet |
| SFTP, `direct` | file permissions of the account | exclusive create under the final name; sizes and bytes compared | Host can overwrite or delete (no atomic publish, held handle) | OpenSSH in a container. Hosted SFTP and NAS: not yet |

As of 2026-11-20, nothing in the rows for Azure Blob Storage, Google Cloud Storage, Backblaze B2 and hosted SFTP has been run against the real service. A real service can answer differently, for instance when a retried upload whose answer was lost is repeated. [How we test data safety](/docs/security/how-we-test/#storage-emulators-and-other-backends) lists the tests and says what each proves.
