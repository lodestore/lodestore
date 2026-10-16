# Threat model: secrets held by the server

This page describes what the Lodestore server stores, what protects it, and what an attacker gets in each realistic compromise. It does not cover the repository format itself; see [ADR 0002](../adr/0002-key-model-and-envelope.md) for the key hierarchy.

## What the server stores

The server keeps a catalog in SQLite or PostgreSQL. Some of its rows are secrets:

- the repository master key of every repository the server manages
- the server's own key slot password for each repository (the slot that lets the server reopen the repository without an administrator)
- the S3 credentials the server uses, including the write-only credentials it hands to agents
- the private key of the built-in certificate authority and of the gRPC server certificate

Every one of these values is wrapped with XChaCha20-Poly1305 under a 32-byte **unlock key** before it is written to the database. The associated data names the kind and id of the secret, so a wrapped value cannot be moved to another row. The wrap format carries a version number.

The unlock key comes from `LODESTORE_UNLOCK_KEY` or `LODESTORE_UNLOCK_KEY_FILE`. If neither is set, the server generates one on first start in `<data_dir>/unlock.key` (mode 0600, created exclusively), logs a warning that the key sits next to the database, and shows a banner in the UI until the key is moved. The server then unlocks the secrets by itself on every start: there is no prompt.

## What an attacker gets

| The attacker has | Master secrets of the repositories | Notes |
| --- | --- | --- |
| Only the database (a leaked backup of the catalog, a stolen SQL dump) | Not exposed | Secrets are ciphertext. Host names, schedules and snapshot metadata are visible. |
| Only the unlock key | Not exposed | The key opens nothing without the wrapped values. |
| The database and the unlock key | **Exposed, for every repository** | This combination yields the master secrets, the S3 credentials and the CA key. Auto-unlock does not protect against it. |
| The server host with a running process | **Exposed** | Unlocked secrets live in the process memory. |
| The bucket only | Not exposed | Data is encrypted under keys derived from the master key, which is never stored in the bucket in the clear. |

The auto-unlock model defends against exactly one thing: a leak of the database on its own. It is not a defence against anyone who can read both the database and the unlock key, and it should not be described or relied on as one.

## Where to keep the unlock key

- **Default (key file next to the database).** Protects only against a leak of the database alone. A backup of the whole data directory contains both, so it gives the attacker everything. Move the key elsewhere as soon as the server runs.
- **`LODESTORE_UNLOCK_KEY_FILE` on a separate volume or a secrets mount.** The database and the key then leave the host through different paths.
- **Helm.** The chart stores the unlock key in a Kubernetes `Secret`. Anyone who can read that `Secret` and the PVC has both. Copy the key out of the cluster (the chart's notes print the command) and keep it in a password manager or vault.
- **Never** put the key in the same backup set, repository or ticket as the database.

## Losing the unlock key

Losing the unlock key loses the server's access to the repositories, not the data. The key slots in the bucket still open with the repository password or the recovery key, and a restore needs only the bucket and one of them. To bring the server back, reconnect the existing repository in the setup wizard with the password or the recovery key.

## Agent credentials

An agent holds a writer keyset and a write-only S3 key. It can add data to the repository and it cannot read it back. The key carries no delete permission, and deletion and garbage collection run only on the server.

- **Write-only means no delete, not no overwrite.** In S3 a `PutObject` on an existing key replaces the object. The generated AWS policy therefore ends with a `Deny` on `s3:PutObject` for requests without `If-None-Match` (the `s3:if-none-match` condition key), and Lodestore itself writes every object with `If-None-Match: *`. Overwrite is refused only where the provider enforces that condition. MinIO rejects the condition key, so the policy it receives (`mc admin policy` commands in the wizard) leaves it out, and a stolen agent key can replace any pack, index or snapshot object there. The checks try this with a throwaway object: the agent key shows as **Write-only verified** only when the overwrite is refused, and as **Can overwrite** (a warning) otherwise. A compromised host can destroy data this way without ever deleting it. Enable bucket versioning or Object Lock on the bucket: with either, a replaced object keeps its earlier version and the data can be recovered.
- **A key that can delete or overwrite is accepted.** The repository wizard and **Replace agent key** show **Can delete** or **Can overwrite** as a warning and carry on; the state is stored on the repository and stays visible. Nothing blocks it, so check the badge after every key change.
- **Experimental backends give agents full credentials.** Azure Blob, GCS, SFTP and WebDAV have no separate agent key. The agent opens the store with the same options as the server, so the sealed secrets (`account_key`, `sas_token`, `credential`, `key`, `password`, `token`) are sent to every enrolled agent, and they allow deleting and overwriting. The repository is marked experimental and its agent key state normally reads **Can delete**. Use S3 with a write-only key where the central property matters.

- The S3 key is **shared by all agents of a repository**. Revoking a host cuts its gRPC stream at once and the server rejects it with the code `revoked`, but the revoked host still holds the S3 key and can keep writing to the bucket until the key is replaced. Use **Replace agent key** in the repository settings: you enter a new write-only key, the server pushes it to every connected agent, and offline agents receive it when they reconnect. The revocation dialog repeats this warning.
- Revocation is a denylist on the server, checked on the connection and on every message; there is no CRL or OCSP. The agent logs the error and exits with code 77. The systemd unit lists `RestartPreventExitStatus=77`, so the service stays `failed` and is not restarted. The agent touches no local files. A revoked `agent_id` is never accepted again; the host returns only through `install.sh --force` with a new join token.
- Agent client certificates are issued by the built-in CA, last 90 days and renew automatically. The agent generates its private key locally and it never leaves the host.

## Join tokens and the setup link

- A join token carries 256 bits of randomness. The database stores only its SHA-256. It works once and expires after one hour by default.
- The install command passes the token in the arguments of `sh`, so it is briefly visible to local users in the process list on the host being enrolled. Treat it as a short-lived secret and do not paste it into shared channels.
- The first-run setup link is printed to the server log once. Whoever reads the log before the administrator is created can create the administrator, so restrict access to the log or use the Helm `Secret` to set the administrator password up front.
- The server has no TLS on the REST and UI port. Terminate TLS on a reverse proxy, otherwise sign-in cookies and tokens cross the network in clear text. See [networking](../deploy/networking.md).

## What an agent sees during a restore

A restore job runs on the target agent, and the server decides what that agent receives. The master secret never leaves the server. The decision is recorded in [ADR 0004](../adr/0004-restore-data-path.md).

**What the agent receives for a job.**

- **A plan of the selection only.** The server opens the snapshot, walks the selected subtrees with its own keys and streams the entries it finds: names, modes, owners, times, links, extents and, for each file, the list of chunks as pack, offset and frame length. Directory trees are not sent as blobs, and snapshot keys and the master secret are not sent at all. The agent learns the paths and metadata of what it restores and nothing about the rest of the snapshot.
- **The keys of the packs the plan uses.** Each pack key opens the frames of that pack and no other pack. The set of packs issued to a job is kept by the server, and a refresh or a ciphertext read for a pack outside that set is refused. The set is dropped when the job finishes or is interrupted.
- **Presigned URLs.** On S3 the server signs a `GET` for each issued pack, valid for one hour, and the agent reads the frames it needs with `Range` requests. The agent asks for fresh URLs through `RefreshPackAccess` when a link is close to expiry or refused, and only for packs already issued to the job. A URL grants read access to that one object until it expires, so it is kept in memory, never logged and left out of error messages.
- **The identity of the source repository: `repo_id` and `k_id`.** To check a chunk against its keyed hash and to compare files with what is already on disk, the agent needs the chunk-hash key of the repository the snapshot came from. `k_id` is a derived key, not the master secret, and without access to the bucket and the index it opens nothing. It is handed over only to the agent that the administrator chose as the target of a restore, including an agent of another repository (a new machine replacing a dead one). It lives in the job assignment, is wiped from the agent's memory after the run and is never written to disk.

**What the agent still holds after a restore.**

- **Neighbouring chunks of a pack.** A pack key opens every frame in its pack, not only the chunks the plan names. An agent that keeps the key can decrypt the other chunks of those packs if it can also read the pack, which its own S3 key cannot do (below). This residue is accepted: packs are 32 MiB, the key is held in memory only for the job, and the agent has no read access to the bucket.
- **The target's own S3 key still cannot read packs.** The key an agent holds is write-only, and a restore does not change that: the test `restore_s3_agent_key_cannot_read_packs` tries a direct `GET` of pack objects with it and expects a refusal. Data reaches the agent only through presigned URLs for the packs of its job.
- **Storage without presigning.** For the local folder and the other backends that cannot sign URLs, the server reads the requested pack ranges and relays the ciphertext to the agent over gRPC, `ReadPackRange`, in parts of at most 1 MiB, for packs issued to the job only. It does not decrypt anything for the agent. The path is the same when the repository sits on the agent's own machine.

**Where the agent writes.**

- The agent runs as root and writes wherever the job says. A restore **in place** therefore replaces files anywhere on the host, and the interface asks for explicit confirmation before it sends one. There is no separate privilege model for restores. The default is a new folder, and files that exist in the target but not in the snapshot are never deleted.
- Names in the plan are validated again on the agent and every write goes through directory descriptors with `*at` calls, so a hostile plan cannot reach outside the target through `..` or a symlink.
- The target path itself is opened one component at a time. Missing folders are created `0700`. A component that a local user other than root could swap is refused: a symlink that user owns, or a folder that user owns when others can write to it or to the folder it sits in. The default location `/var/lodestore/restore` sits under root-owned folders, so a local user can neither plant nor redirect it; a custom target under a world-writable folder can at most be made to fail.

**Re-enrolment and the previous agent id.**

- A host that is enrolled again may claim its earlier `agent_id`, and the server then binds the snapshots of that id to it. This only changes which host the interface offers as the original one. The agent is write-only and cannot read snapshots, so claiming an id gives it no data. A host that took over another id could, however, be offered "restore to the original host" in the interface, a social-engineering risk. Join tokens are issued by the administrator only.

**Download as an archive is the exception.** Downloading a selection as `tar.gz`, `tar.zst` or `zip` is the one place where the server decrypts data for a person. The server streams the archive to the browser without temporary files, and no agent is involved.

## Backup hooks

A backup policy can run a script on the host before and after a backup. The server chooses which hook runs and with which arguments, and the agent decides whether it may run.

**What the boundary guarantees.**

- The agent runs only a regular file in `/etc/lodestore/hooks.d` that the administrator installed. The file must be owned by the agent's user (root in the systemd unit), must not be a symlink, and must not be writable by group or others. The directory has to meet the same rules. Anything else is refused with `hook_rejected`, and a missing script is `hook_missing`.
- A compromised server cannot create a script, because `hooks.d` is writable only by root on the host. It can only pick among the scripts that are already there.
- An inline `command` runs only when `/etc/lodestore/agent.yaml` contains `hooks: { allow_inline: true }`. The server cannot set this flag. The file counts only when it is owned by the agent's user and is not writable by group or others; otherwise the flag is off. `agent.yaml` is separate from `agent.json`, so `install.sh --force` keeps it.
- Arguments reach the script through `execve`, never through a shell. The hook gets an empty environment except `PATH`, `LODESTORE_POLICY`, `LODESTORE_JOB_ID`, `LODESTORE_HOOK_PHASE` and, for the post hook, `LODESTORE_BACKUP_STATUS`. S3 keys, writer keys and proxy settings are not passed. Standard input is `/dev/null`.
- A hook runs in its own process group. On timeout (one hour by default, set in the policy) or cancel, the group gets SIGTERM and, after 10 seconds, SIGKILL, so child processes end as well.

**What it does not guarantee.**

- A compromised server can still pass hostile arguments to a powerful installed script. Write scripts to validate `$@`, and prefer scripts that take no arguments.
- The hook runs as the agent's user, which is root. Anyone who can write `hooks.d` or `agent.yaml` already owns the host.

**Writing hooks.**

- Hooks inherit the agent's systemd sandbox: `NoNewPrivileges`, `PrivateTmp`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups`, `RestrictAddressFamilies`, `LockPersonality`, `RestrictRealtime`, `ProtectClock`, `ProtectHostname`, `ProtectKernelLogs` and `SystemCallArchitectures=native`. Namespace restrictions and a read-only file system are not applied, so mount and `unshare` based hooks still work.
- `NoNewPrivileges` makes `sudo` fail. To run a step as another user, use `runuser -u <user> --` or `setpriv`.
- `PrivateTmp` gives the agent and its hooks a private `/tmp` that is not the host's `/tmp`. A dump written there is invisible to the database process, and a policy path under `/tmp` backs up the private tree. Write dumps to `/var/lib/lodestore/staging` or `/var/backups`.
