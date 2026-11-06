---
title: "Threat model: secrets held by the server"
---
This page describes what the Lodestore server stores, what protects it, and what an attacker gets in each realistic compromise. It does not cover the repository format itself; see [ADR 0002](/docs/adr/0002-key-model-and-envelope/) for the key hierarchy.

## What the server stores

The server keeps a catalog in SQLite or PostgreSQL. Some of its rows are secrets:

- the repository master key of every repository the server manages
- the server's own key slot password for each repository (the slot that lets the server reopen the repository without an administrator)
- the S3 credentials the server uses, including the write-only credentials it hands to agents
- the private key of the built-in certificate authority and of the gRPC server certificate

Every one of these values is wrapped with XChaCha20-Poly1305 under a 32-byte **unlock key** before it is written to the database. The associated data names the kind and id of the secret, so a wrapped value cannot be moved to another row. The wrap format carries a version number.

The server also keeps a search cache in `<data_dir>/find-cache/`: the list of file paths of every snapshot that a fleet-wide find has walked. Each frame of a cache file is compressed and sealed with the same AEAD under the unlock key, with the snapshot id, the frame position and the end of the file bound in as associated data, so the file holds no path in the clear and a reordered or truncated file is rejected. A file is deleted together with its snapshot and whenever it fails to open (a different unlock key included); the server rebuilds it from the bucket on the next search. The cache is only an accelerator: restoring needs nothing from it.

The unlock key comes from `LODESTORE_UNLOCK_KEY` or `LODESTORE_UNLOCK_KEY_FILE`. If neither is set, the server generates one on first start in `<data_dir>/unlock.key` (mode 0600, created exclusively), logs a warning that the key sits next to the database, and shows a banner in the UI until the key is moved. The server then unlocks the secrets by itself on every start: there is no prompt.

## What an attacker gets

| The attacker has | Master secrets of the repositories | Notes |
| --- | --- | --- |
| Only the database (a leaked backup of the catalog, a stolen SQL dump) | Not exposed | Secrets are ciphertext. Host names, schedules and snapshot metadata are visible. A copy of `data_dir` does not reveal file names either as long as the unlock key is not in it (see the auto-generated `unlock.key` above): the search cache is sealed under that key. |
| Only the unlock key | Not exposed | The key opens nothing without the wrapped values. |
| The database and the unlock key | **Exposed, for every repository** | This combination yields the master secrets, the S3 credentials and the CA key. Auto-unlock does not protect against it. |
| The server host with a running process | **Exposed** | Unlocked secrets live in the process memory. Signed updates keep this attacker from installing a binary through the update path, but a restore job can still write files anywhere on an enrolled host as root, so this is root on those hosts too (see [Agent updates](#agent-updates)). |
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
- **A local folder gives the agent delete rights.** There is no separate key: the agent writes with file permissions and can delete and overwrite. It is for trying Lodestore, not for real backups.
- **Only S3-compatible storage has these properties.** A separate agent key without delete rights and Object Lock exist only there, which is why 0.1 offers S3-compatible storage and a local folder and nothing else. The agent and `lode` of 0.1 are built without Azure Blob, GCS, SFTP and WebDAV. Those are on the v0.2 roadmap, and they would hand agents full credentials unless that changes first.

- The S3 key is **shared by all agents of a repository**. Revoking a host cuts its gRPC stream at once and the server rejects it with the code `revoked`, but the revoked host still holds the S3 key and can keep writing to the bucket until the key is replaced. Use **Replace agent key** in the repository settings: you enter a new write-only key, the server pushes it to every connected agent, and offline agents receive it when they reconnect. The revocation dialog repeats this warning.
- Revocation is a denylist on the server, checked on the connection and on every message; there is no CRL or OCSP. The agent logs the error and exits with code 77. The systemd unit lists `RestartPreventExitStatus=77`, so the service stays `failed` and is not restarted. The agent touches no local files. A revoked `agent_id` is never accepted again; the host returns only through `install.sh --force` with a new join token.
- Agent client certificates are issued by the built-in CA, last 90 days and renew automatically. The agent generates its private key locally and it never leaves the host.

## Agent updates

An agent update replaces the binary of a process that runs as root on the host. The server cannot use this path to hand an agent code, because the agent decides what it runs.

**What the agent trusts.**

- The public key of the project, built into the agent binary, and the `*.pub` files in `/etc/lodestore/trusted-keys` on the host. Nothing else counts as a signer, and the server has no message that adds, replaces or removes a key.
- A key file counts only when it passes the same ownership test as a hook: the file and the directory belong to the agent's user (root in the systemd unit), are not writable by group or others, and the file is not a symlink and is at most 4 KiB. `install.sh --trust-key` installs a key that way, as `root:root` with mode 0644 in a 0755 directory.
- The signature covers the binary and a trusted comment, `lodestore-agent <version> <arch>`, under a global signature. The agent checks the SHA-256 the server announced, the signature, that the signed version is the requested one, that the architecture is its own, that the version is newer than the running one, and that the new binary prints its version when started with `version`. It refuses anything else and reports the reason.

**What the server can and cannot do.**

- It can hold updates back by never starting them, or roll out a newer version that a trusted key signed. A rollout of an older signed binary is refused by the agent, so a downgrade is not possible.
- Through an update it cannot give an agent a key to trust, a binary for another architecture, a binary signed by an unknown key, or a binary of its own making.
- Only a token or session with the scope `agents:update` starts, pauses, resumes or cancels a rollout. `hosts:write` is not enough.

**A server that is taken over.**

The signature checks stop a compromised server from installing a binary through the update path, and that is the limit of what they do. A server that is taken over holds the repository keys and can start a restore job on any enrolled host. The job runs as root and writes wherever it is told, so the attacker can put files of their choosing anywhere on the host, including the agent binary, `/etc/lodestore/trusted-keys`, `/etc/lodestore/hooks.d` and `agent.yaml`. Treat a server compromise as root on every enrolled host.

**The installer.**

`install.sh` checks the binary only against a SHA-256 served by the same server, so the check shows the download is intact, not who built it. On a server you do not trust, verify the binary with `minisign -V` first.

**Rollback guard.**

The old agent keeps its binary as `lodestore-agent.prev`, writes a record of the update, and arms a transient systemd timer for 120 seconds before the process restarts. If the new version does not connect and report itself in that time, the timer runs the previous binary's `update-rollback`, which puts it back. A new binary that crashes or never connects is therefore rolled back by the timer. The report of the new version and the timer exclude each other: whichever takes the record of the update first decides, so an update confirmed to the server stays in place. If the previous binary has disappeared there is nothing to put back, and the host reports that instead of a rollback.

Running `install.sh` by hand ends the update: it stops the timer and removes the record of the update and `lodestore-agent.prev`. An agent that starts with a version other than the target of the update drops the record as well, so the guard does not replace a binary installed by hand.

**Residual risk.**

If the host reboots inside the two-minute window, the transient timer is lost. The agent that starts afterwards puts the guard back for the rest of the window, or rolls back at once when the window has already run out. The only gap is a reboot in the window combined with a new binary that dies before its start-up code (`on_start`) has run: then nothing re-arms the guard and nothing rolls back. The server still notices. It shows the update as failed once its deadline passes (`rolled_back`, the alert `agent_update_failed`), or, if the report of the switch never reached it, the host appears offline (`agent_offline`). Run `install.sh` on the host to repair it.

## Join tokens and the setup link

- A join token carries 256 bits of randomness. The database stores only its SHA-256. It works once and expires after one hour by default.
- The install command passes the token in the arguments of `sh`, so it is briefly visible to local users in the process list on the host being enrolled. Treat it as a short-lived secret and do not paste it into shared channels.
- The first-run setup link is printed to the server log once. Whoever reads the log before the administrator is created can create the administrator, so restrict access to the log or use the Helm `Secret` to set the administrator password up front.
- The server has no TLS on the REST and UI port. Terminate TLS on a reverse proxy, otherwise sign-in cookies and tokens cross the network in clear text. See [networking](/docs/reference/networking/).

## What an agent sees during a restore

A restore job runs on the target agent, and the server decides what that agent receives. The master secret never leaves the server. The decision is recorded in [ADR 0004](/docs/adr/0004-restore-data-path/).

**What the agent receives for a job.**

- **A plan of the selection only.** The server opens the snapshot, walks the selected subtrees with its own keys and streams the entries it finds: names, modes, owners, times, links, extents and, for each file, the list of chunks as pack, offset and frame length. Directory trees are not sent as blobs, and snapshot keys and the master secret are not sent at all. The agent learns the paths and metadata of what it restores and nothing about the rest of the snapshot.
- **The keys of the packs the plan uses.** Each pack key opens the frames of that pack and no other pack. The set of packs issued to a job is kept by the server, and a refresh or a ciphertext read for a pack outside that set is refused. The set is dropped when the job finishes or is interrupted.
- **Presigned URLs.** On S3 the server signs a `GET` for each issued pack, valid for one hour, and the agent reads the frames it needs with `Range` requests. The agent asks for fresh URLs through `RefreshPackAccess` when a link is close to expiry or refused, and only for packs already issued to the job. A URL grants read access to that one object until it expires, so it is kept in memory, never logged and left out of error messages.
- **The identity of the source repository: `repo_id` and `k_id`.** To check a chunk against its keyed hash and to compare files with what is already on disk, the agent needs the chunk-hash key of the repository the snapshot came from. `k_id` is a derived key, not the master secret, and without access to the bucket and the index it opens nothing. It is handed over only to the agent that the administrator chose as the target of a restore, including an agent of another repository (a new machine replacing a dead one). It lives in the job assignment, is wiped from the agent's memory after the run and is never written to disk.

**What the agent still holds after a restore.**

- **Neighbouring chunks of a pack.** A pack key opens every frame in its pack, not only the chunks the plan names. An agent that keeps the key can decrypt the other chunks of those packs if it can also read the pack, which its own S3 key cannot do (below). This residue is accepted: packs are 32 MiB, the key is held in memory only for the job, and the agent has no read access to the bucket.
- **The target's own S3 key still cannot read packs.** The key an agent holds is write-only, and a restore does not change that: the test `restore_s3_agent_key_cannot_read_packs` tries a direct `GET` of pack objects with it and expects a refusal. Data reaches the agent only through presigned URLs for the packs of its job.
- **Storage without presigning.** The local folder cannot sign URLs, so the server reads the requested pack ranges and relays the ciphertext to the agent over gRPC, `ReadPackRange`, in parts of at most 1 MiB, for packs issued to the job only. It does not decrypt anything for the agent. The path is the same when the repository sits on the agent's own machine.

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
- A policy cannot create a script: it only names one, and `hooks.d` is writable only by root on the host. Through a policy a compromised server can only pick among the scripts that are already there. A restore job is a different path: it runs as root and can write into `hooks.d`, so this boundary does not hold against a server that is taken over (see [Agent updates](#agent-updates)).
- An inline `command` runs only when `/etc/lodestore/agent.yaml` contains `hooks: { allow_inline: true }`. A policy cannot set this flag, though a restore job can write the file, as it can `hooks.d`. The file counts only when it is owned by the agent's user and is not writable by group or others; otherwise the flag is off. `agent.yaml` is separate from `agent.json`, so `install.sh --force` keeps it.
- Arguments reach the script through `execve`, never through a shell. The hook gets an empty environment except `PATH`, `LODESTORE_POLICY`, `LODESTORE_JOB_ID`, `LODESTORE_HOOK_PHASE` and, for the post hook, `LODESTORE_BACKUP_STATUS`. S3 keys, writer keys and proxy settings are not passed. Standard input is `/dev/null`.
- A hook runs in its own process group. On timeout (one hour by default, set in the policy) or cancel, the group gets SIGTERM and, after 10 seconds, SIGKILL, so child processes end as well.

**What it does not guarantee.**

- A compromised server can still pass hostile arguments to a powerful installed script. Write scripts to validate `$@`, and prefer scripts that take no arguments.
- The hook runs as the agent's user, which is root. Anyone who can write `hooks.d` or `agent.yaml` already owns the host.

**Writing hooks.**

- Hooks inherit the agent's systemd sandbox: `NoNewPrivileges`, `PrivateTmp`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups`, `RestrictAddressFamilies`, `LockPersonality`, `RestrictRealtime`, `ProtectClock`, `ProtectHostname`, `ProtectKernelLogs` and `SystemCallArchitectures=native`. Namespace restrictions and a read-only file system are not applied, so mount and `unshare` based hooks still work.
- `NoNewPrivileges` makes `sudo` fail. To run a step as another user, use `runuser -u <user> --` or `setpriv`.
- `PrivateTmp` gives the agent and its hooks a private `/tmp` that is not the host's `/tmp`. A dump written there is invisible to the database process, and a policy path under `/tmp` backs up the private tree. Write dumps to `/var/lib/lodestore/staging` or `/var/backups`.

## API tokens

An API token lets a script or CI job reach the REST API without a browser session. It is the boundary between the CI environment and the server, and the token is the only credential that crosses it.

- **Scopes.** A token carries a list of scopes chosen by the administrator. Each token-enabled route names one scope through the `Scoped<S>` extractor and rejects a token without it with `403`. A route that does not use the extractor accepts sessions only, so a new route is closed to tokens until someone opts it in. A test lists the session-only routes (token management, join tokens, repositories, catalog, server settings, the administrator account) and checks that a token holding every scope still gets `401` there.
- **Storage.** Only the SHA-256 hash and the first characters of the token are stored. The secret is shown once, so the database and its backups do not hold a usable token.
- **Revocation.** Every request looks the token up in the database, so a revoked or expired token stops working at once. There is no cache to wait out.
- **No CSRF with Bearer.** The browser does not attach an `Authorization` header by itself, so a Bearer request cannot be forged from another site. Session requests keep their CSRF check. A request with a Bearer header is never mixed with the cookie: the token alone decides.
- **Audit.** A restore or download started by a token records `token:<name>` as who started it.
- **Plain HTTP.** A token sent over HTTP without TLS can be read on the network. Serving the API over TLS, or keeping CI and server on a trusted network, is the operator's job. `lode` warns when it sends a token to a non-loopback `http://` address.
- **What a stolen token gives.** Everything its scopes allow and nothing else: a token with `restore` can read and download backed-up data, so give such a token to few jobs and use short expiry. A token cannot create other tokens, change the password or touch repositories.

## Retention and forgetting snapshots

After every backup that ends as succeeded or with warnings, the server applies the retention rules of the policy to one pair: the policy and the host. It deletes the `snapshots/<id>` object with its own credentials and removes the catalog row. Packs are never touched here; unreferenced data is the business of garbage collection.

**What the boundary guarantees.**

- Only the server forgets. The agent key is write-only and cannot delete, so a compromised agent cannot forget anything, its own snapshots included.
- A pass looks only at snapshots that carry the tag of its policy, sit in the repository of the policy and were bound to their host by the server: the host is recorded when the agent reports a finished job on its own authenticated session. A snapshot that the catalog sync merely found in the bucket gets its host from fields the agent wrote into the manifest (`agent_id`, `hostname`), so it is kept in the catalog but never counts toward any rule or the floor and is never forgotten by retention. Snapshots of other policies, manual `lode` snapshots, stdin snapshots without `--policy` and snapshots of a deleted policy are never counted and never forgotten.
- The object key comes from `snapshot_object_key` for a 64-character hex id that is already in the catalog. No path from a user or an agent is used.
- A compromised agent can flood its own pair with fresh snapshots to push good ones out of the retention window. The damage is limited to that pair and host, because only snapshots linked to a finished job of that host count, so a compromised agent cannot plant manifests that claim another host and displace its history. A snapshot marked suspicious (by the anomaly detector or by an administrator) counts toward no rule and is never forgotten, and the two newest good complete snapshots of a pair are always kept, even with `keepLast: 1`.
- Receive time, not the time written by the agent, orders the snapshots, so an agent cannot move a snapshot in or out of a window with a forged clock. If the receive time of any snapshot in a pair is not yet confirmed by the storage (a catalog rebuilt from a database that predates the receive-time column, or a backend that reports no modification time), the pass is postponed and a warning is written to the job log, because buckets computed from estimated times could collapse and drop history.
- Only snapshots the server has signed count (see [Snapshot provenance and `latest`](#snapshot-provenance-and-latest)). A snapshot that nothing signed takes no slot in any rule, does not push a signed one out of a window and is never forgotten by retention; an administrator can still delete it. A host that plants a hundred snapshots with the writer keys therefore cannot displace its real history. Its real backups are another matter: they are signed, and a compromised host can still make enough of them to push its own older history out of the window.
- Incomplete snapshots take no slot in any rule, and no rule forgets them; they are neither counted nor deleted by retention.
- Through an API token, creating a policy with `spec.retention` or changing it requires the `snapshots:delete` scope in addition to `policies:write`, since a tighter rule makes the next backup delete older snapshots. Sessions are not affected.

**When the storage refuses the delete.**

- With Object Lock or versioning, a plain delete succeeds by adding a delete marker; the earlier version stays in the bucket. The catalog and `lode` no longer see the snapshot, and no tombstone is needed.
- If the storage answers "access denied" (a server key without `s3:DeleteObject`, a lock rule that forbids it), the server writes a tombstone, removes the row from the catalog and adds a warning, `Could not delete snapshot <id>: <reason>`, to the log of the backup job. The job stays succeeded. The tombstone stops later catalog syncs from bringing the row back while the object is still in the bucket; it is dropped when the object disappears. A catalog sync retries the delete at most once a day for a tombstoned object that is still listed, and drops the tombstone when the delete goes through.
- A temporary backend error leaves the row in place and the next pass retries.

**What the markers do not protect against.**

- Suspicious marks and the anomaly details live in the database only. If the database is lost and the catalog is rebuilt from the bucket, the marks are gone; the rebuilt snapshots are not bound to a job, so retention neither counts nor forgets them, and old history simply stops being rotated. The mark "signed" comes back from the bucket during the rebuild, but the host and the job of a snapshot do not.
- A forgotten snapshot cannot be brought back without bucket versioning. The interface shows a preview of what a rule change would forget and asks for confirmation before it saves a tighter rule, to guard against an accidental change.

## Garbage collection and storage keys

Forgetting a snapshot only removes its manifest. Space comes back when the server runs garbage collection, which deletes packs and index shards that no remaining snapshot references. Deletion is a server-only act.

**What an agent key can do around `locks/`.**

- Read: the agent key may list and read `locks/`. Garbage collection publishes the candidate packs of a cycle there, and a running backup checks them before it reuses an existing pack.
- Not write: the agent key has no `PutObject` on `locks/`, so a compromised host cannot plant a candidate list or block a collection cycle with markers that never close.
- Not delete: no `Delete*` action is granted on any prefix. On a bucket with versioning or Object Lock this covers deleting a version by its version id as well.
- `locks/attest-*`: the objects that carry the server's signature over index shards, and the signed record of who made each snapshot, are written only by the server (and, for provenance, by `lode backup` with the repository password). A compromised host can neither write one nor make one verify. See [Index metadata from a compromised agent](#index-metadata-from-a-compromised-agent) and [Snapshot provenance and `latest`](#snapshot-provenance-and-latest).

**What the server key needs.** The server deletes with its own key, never the agent key, and never asks the storage to bypass a lock. On AWS S3 versioned deletion takes `s3:DeleteObject`, `s3:DeleteObjectVersion`, `s3:ListBucketVersions` and `s3:GetObjectVersion` in addition to the rights it already uses. MinIO does not tell these apart: a key with `s3:DeleteObject` alone removed a version in testing, so a MinIO setup that works does not prove the AWS policy is complete.

**Locked versions.** When the storage refuses to delete a version because Object Lock still protects it (MinIO answers 400 `InvalidRequest`, AWS answers 403 `AccessDenied`), the version is reported as retained, not as a failure. Its delete marker is kept until the last real version is gone, otherwise the object would reappear in listings. A later cycle frees the version once the lock has run out.

## Index metadata from a compromised agent

An agent holds the key that seals index shards ([ADR 0002](/docs/adr/0002-key-model-and-envelope/)). Whoever controls an agent can therefore write a shard that lists any blob at any place and declares that it supersedes any other shard, and can put a pack of their own choosing in the bucket. This section says what Lodestore does about that. The signature objects are described in [ADR 0005](/docs/adr/0005-server-attestations/).

**What the server signs.** The server writes create-only objects `locks/attest-*` that list the index shards it vouches for, with a MAC under a key derived from the repository master key. Agents do not have that key. They can read the objects and cannot forge or alter one. The server and `lode` hold the master key, verify every object and ignore those that fail. The server logs each object that fails with its key, and the next cleanup cycle reports it as a warning and deletes it (a storage that keeps it under Object Lock leaves it ignored).

**What this prevents.**

- **A planted shard does not steer cleanup.** `supersedes` counts only from attested shards, in garbage collection and in restore. The writer of a backup, whether an agent or `lode backup`, does not apply `supersedes` at all: it cannot verify the signature. The check of a new backup never signs a host's shard that lists `supersedes`, even when its frames open: the frames say nothing about whether that list is honest.
- **Cleanup opens the surviving copy.** Before it deletes or replaces a pack that holds a live blob, the server reads the frame at the place the index names as the surviving copy and opens it: the authentication tag and the keyed blob id must match. If the frame does not open, the pack stays. A pack that cannot be read, or whose live blobs do not all open, is skipped by the rewrite step and left for a later cycle, and the cycle records a warning that names it. This check does not depend on trusting any shard, so it also covers shards signed at the upgrade.
- **A malformed shard is left out.** A shard that does not open, or holds a record outside its pack or with an unknown blob type, is skipped and reported by cleanup, by the deduplication of a backup and by the server's check of a new backup. It does not stop any of them, even when the upgrade signed it.
- **Reads fall back to other copies.** If the first known location of a blob does not open, runs past the end of its pack or is in a pack of a version the reader does not know, `lode restore` and the reads the server makes itself (browsing, tree reads) try the others, including those listed only by a shard that a newer shard supersedes. Every frame is authenticated by its blob id, so a false copy is rejected and never returned. A false record that claims the wrong size for a chunk does not fail the file either: when the sizes in the index do not add up, the restore reads the chunks and counts what they hold. A restore run by an agent and a download as an archive do not fall back yet: they read file data from the first known location only, so a false first copy makes that file fail there while `lode restore` still recovers it.
- **A backup that cannot be restored does not end as a success.** After every backup the server opens every new frame the snapshot depends on and checks that all its trees and data blobs resolve through shards whose frames it has opened itself. This costs egress: the server downloads every new byte once. If the check fails, the job is set to `failed` with the error code `verification_failed`, the snapshot is marked with `unverified_reason` in the snapshot list of the API, and the usual alert for a failed backup fires when that job is still the latest finished job of its policy on that host; a job without a policy, such as a stdin backup, raises no alert yet. A job can therefore turn from succeeded to failed after the agent reported it finished. If the server cannot run the check (the storage is unreachable, or the server refuses to sign the repository, see below), it retries for at most one hour from the first failed attempt, measured on the server's clock; after that the job also becomes `failed` with `verification_failed`. A job that reports a snapshot the catalog already links to another job fails the check, and the other job's snapshot is left as it was. This check covers backups made by agents. `lode backup` run directly against a repository that a 0.2 server has upgraded checks itself: it deduplicates only against signed shards (and shards this machine opened itself), opens the frames its snapshot depends on the way the server does before it exits with 0, and signs the snapshot with the repository password. The first run on a machine reads the frames of every shard the snapshot depends on; later runs keep what they checked in `$XDG_CACHE_HOME/lode/` (see [the command line](/docs/reference/cli/#backup-and-restore-without-the-server)). `lode backup --writer-keys` cannot check anything and refuses to run in such a repository. In a repository that no 0.2 server has opened, `lode backup` is not checked at all: it deduplicates against every shard in `index/`, so a planted shard can make such a backup unrestorable without an error. Back up through an agent, or open the repository with a 0.2 server first, when the bucket is shared with hosts you do not fully trust.
- **The agent's deduplication filter is not a boundary.** Once the server has upgraded a repository, an agent deduplicates only against its own shards and the attested ones. The agent cannot verify a signature, so this narrows what a planted shard can influence and nothing more. The boundary is the server's check above: a shard that passes the filter but lacks the data it claims produces a failed job, not a silent gap.

**What the upgrade trusts.** When the server first opens a repository (it creates one, you connect an existing one, or a repository written by 0.1 meets a newer server) it signs the shards that are in `index/` at that moment. The signature means "present at the upgrade", not "contents verified". The server relies on such a shard only for `supersedes` and for cleanup. The check of a new backup does not rely on it: the first time a backup refers to the shard, the server opens its frames, and it remembers the result. A shard planted before the upgrade is therefore signed too, and a backup that depends on it becomes a failed job. The upgrade also signs who made the snapshots the catalog already ties to a host and a job; see [Snapshot provenance and `latest`](#snapshot-provenance-and-latest). A forgery made before the upgrade is found across the whole repository by `lode check --level packs`; run it once after upgrading.

**Where the state lives, and a folder repository.** The list of shards whose frames the server opened is in the server database. The fact that a repository was upgraded is in the bucket, as a marker in `locks/attest-*`, so a new database does not sign the shards again. On S3 the agent key is read-only on `locks/`, which rules out everything below. On a local or network folder the agent has file permissions and can write and delete in `locks/`:

- It cannot forge a MAC. An object that does not verify is ignored, logged by the server and deleted by the next cleanup cycle.
- A planted or deleted list of shards affects only the agents' deduplication filter. Agents cannot check the MAC, so a planted list steers the deduplication of every host on the repository until the next cleanup cycle deletes it. The outcome is failed jobs on those hosts, not a successful backup that cannot be restored.
- Deleting attestations withdraws the trust in `supersedes` (the index still holds the other copies, and `lode restore` falls back to them) and in deduplication against verified data. It does not turn the backup check off, because the list of verified shards is in the database.
- Deleting attestations also deletes the signed records of who made each snapshot. The server keeps its own marks in the database and is not affected; `lode` on a machine that has not seen the signatures falls back to the legacy choice of `latest`. See [Snapshot provenance and `latest`](#snapshot-provenance-and-latest).
- If the marker is deleted while other attestations remain and the database is lost as well, the server refuses to sign the repository again and says so. Cleanup of that repository then fails with that error before it deletes anything, and a new backup becomes a failed job with `verification_failed` one hour after its first failed check.
- If an agent deletes the marker together with every attestation and the database is lost as well, nothing in the bucket shows it. This is an accepted residual risk. The next upgrade signs the current index, including a shard planted in the meantime. Such a shard still cannot cause a deletion, because cleanup opens the surviving copy, and a backup that depends on it becomes a failed job with `verification_failed`: the upgrade signature does not replace opening the frames.
- Do not use a folder repository for hosts you do not trust.

**What stays open.**

- When `index/` is lost, agents deduplicate only against their own shards and the attested ones, so they upload again data that the server has not yet verified. `rebuild-index` accepts every shard of a repository that has not been upgraded, and only attested shards after the upgrade. This costs bandwidth and space, not data.
- Who made a snapshot, and which snapshot is the latest, are covered separately; see [Snapshot provenance and `latest`](#snapshot-provenance-and-latest) for what that covers and where it stops.

## Snapshot provenance and `latest`

A snapshot manifest is written by the host being backed up. Its `time`, `hostname` and `agent_id` are claims of that host. Whoever holds the writer keys can plant a snapshot that names another host and carries any time, even one far in the future. If `latest` followed the manifest, that snapshot is what `lode restore latest` and "restore to the original host" would offer after an incident. This section says how Lodestore avoids that and where the protection stops. The design is in [ADR 0006](/docs/adr/0006-snapshot-provenance/).

**Who signs a snapshot.** A snapshot is signed when a record in a `locks/attest-*` object names it. The record is sealed with a key derived from the repository master key, so an agent cannot write or alter one, and on S3 its key cannot write `locks/` at all.

- **The server** writes the record after it has checked a finished backup (see above). The host in the record comes from the agent's client certificate and the job from the server's own catalog, never from the manifest. A snapshot whose manifest does not name the host that reported it fails the check. A `lode backup --stdin` stream that goes through the agent is reported on the agent's own session, so it is signed for the host of that certificate, and host A cannot pass a snapshot off as host B's.
- **`lode backup` with the repository password or recovery key**, in a repository a 0.2 server has upgraded, signs its own snapshot after it has checked it. The record has no host and no job, which means "signed by the holder of the password": `lode snapshots` marks it `owner` and the web interface shows "Signed with password". The holder of the password can write anything to the bucket anyway; the signature only lets such a snapshot take part in `latest`.
- **At the upgrade**, the first time a 0.2 server opens a repository, it signs the snapshots that its catalog already ties to a host and a job, that is, those a host reported as finished on its authenticated session, including those whose job record the server has since pruned. A job still on record must have ended as succeeded or with warnings. A repository created by a 0.2 server has nothing to sign this way. Every other snapshot in the bucket stays unsigned. The server trusts the state of the repository at that moment: a snapshot that a compromised host reported as its own before the upgrade is signed as that host's.

**What the server does with signed and unsigned snapshots.**

- The lists of a host, the timeline, the host picker, the restore page and the fleet-wide find order snapshots by the time the server received them, never by the manifest; the find lists signed versions of a file first. The time of the last backup (the fleet overview, the "no backup" alert, metrics) and the baseline of the anomaly detector use signed snapshots only, so a planted snapshot cannot hide an overdue backup. The host picker and the restore page open the newest signed snapshot.
- Retention counts only signed snapshots and never forgets an unsigned one (see [Retention and forgetting snapshots](#retention-and-forgetting-snapshots)).
- The interface marks an unsigned snapshot "Unverified". A restore of it needs a confirmation, and the server refuses to restore it in place (`restore_unverified_in_place`) or without the confirmation (`restore_unverified_not_confirmed`). An unsigned snapshot stays restorable to another folder and by its id.

**How `lode` chooses `latest`.** `lode restore`, `lode ls` and `lode dump` use the same rule as the server, because it lives in the format library.

- **Strict**, when the repository holds at least one attestation that passes the MAC. Only signed snapshots compete, ordered by the time the storage reports for the snapshot object (`LastModified`), not by the manifest. A signed snapshot stored more than ten minutes ahead of the clock is skipped with a warning. If none is left, the command fails and asks for an id. If the attestations change while `lode` reads them, as during a cleanup cycle, it reads them again and fails rather than decide on a partial set. It warns about unsigned snapshots stored after the chosen one and about manifests that claim a time in the future.
- **Legacy**, when the repository holds no valid attestation. This is a 0.1 repository that no 0.2 server has opened, and nothing there can be signed yet. `latest` is the snapshot the storage wrote last, and the command warns that it could have been written by a host with writer credentials. A snapshot stored more than ten minutes ahead of the clock is never chosen; if every snapshot is, the command fails and asks for an id. **In legacy mode the protection does not exist**: a compromised host that wrote last wins. Open the repository with a 0.2 server before you need an emergency restore, or name the snapshot by its id.

**Folder repositories.** On a local or network folder, `LastModified` is the file's modification time, which whoever can write the file can set. In strict mode the modification time does not decide who is signed: an agent cannot create a signed snapshot. A writer with file permissions can still change the order of `lode`'s choice among signed snapshots by touching their files; it cannot add a snapshot to the set. The server orders by its own clock and is not affected. The modification time is the only evidence in legacy mode, and in this case:

- `lode` keeps a mark per repository on the machine where it has seen signatures (`$XDG_CACHE_HOME/lode/<repository id>/upgraded`). With the mark, if `latest` finds no readable attestation, it refuses and asks for an id instead of falling back to legacy mode, so deleting `locks/attest-*` from a folder repository does not silently change the answer. `lode backup` on that machine refuses to run in that state as well.
- A machine without the mark, such as a new one used for an emergency restore, cannot tell a repository whose attestations were deleted from a 0.1 repository. It falls back to legacy mode, and the holder of the writer keys, who on a folder has file permissions, can delete the attestations and give a planted snapshot an old modification time so that it is the newest one that is not "in the future". This is not prevented. On S3 the agent key cannot delete or write in `locks/`, so the signatures survive. Do not use a folder repository for hosts you do not trust.

**A lying shard costs one backup.** A host can plant an index shard, or a hint in a forged attestation list, that names blobs it did not upload, hoping a later backup deduplicates against it. When the server's check of a new backup finds shards that do not hold what they claim, it records every one of them in its database as distrusted, up to 64 per backup, and the job fails once. The cap keeps one hostile snapshot from flooding the list; a hint naming more lying shards costs one more backup per 64. The server sends the list to agents with every job and on every connection, and an agent never uses a listed shard for deduplication, so the next backup of that host does not rely on it. Agents receive the newest 65 536 revocations; cleanup reads all of them. A shard the server catches lying again moves back to the front of that list, so a host that floods the list with junk shards to push an old revocation out costs its victim one more failed backup per 64 evicted shards that backup relies on. The next cleanup cycle leaves the shard out of the consolidated attestation, and compaction never signs its records again under a new shard. The revocation lives in the server database, not in the bucket, because the repository format does not change. A lost database loses the list, and the next failing check records it again. `lode backup` keeps its own list of shards it has caught lying, and of those it has opened, in its cache folder.

**After the database is lost.** The rebuild of the catalog restores from the bucket which snapshots are signed and by whom (server or password), and the time of the signature. A server signature is restored only on a snapshot whose rebuilt host is the host the record names; any other snapshot stays unsigned. It does not restore which host and which job made a snapshot, because only the database held that.

**What this does not cover.**

- Provenance says who produced a snapshot, not that its contents are honest. A compromised host can still make real backups that are signed as its own, with any contents, and make enough of them to push its own older history out of a retention window. Limiting what a host may write is planned. `lode restore latest` chooses across all hosts of the repository; to restore one host, find its snapshot with `lode snapshots` and name it by id.
- Legacy mode, a folder repository and a machine without the mark, as described above.
- A `lode backup --writer-keys` backup in a repository that no 0.2 server has upgraded is not checked by anything. In an upgraded repository the command refuses to run, because writer keys cannot check the backup.
- A server that is taken over signs what it likes. The signature is the server's, so it proves what the server saw, not that the host was honest.

## Demo mode

`demo_mode` turns a server into a public, read-only demonstration. It is meant for a throwaway instance filled with synthetic data and never for real backups: it hands every anonymous visitor an administrator session, and that session can read every snapshot and download it, decrypted by the server. The server logs a warning on every start with the flag on. Do not put it in a Helm values file or a quickstart.

**Sign-in.** `POST /api/v1/auth/demo` signs a visitor in as the administrator without a password. The session lasts one hour and is not extended. The endpoint exists only with the flag on and answers 404 otherwise. Sign-ins are rate limited per client address, so behind a reverse proxy `trusted_proxies` must be set, or every visitor shares one bucket.

**What is open.** A default-deny allowlist of (method, route template) pairs, `ALLOWED` in [`demo.rs`](https://github.com/lodestore/lodestore/blob/main/crates/lodestore-server/src/demo.rs), which is the full list:

- Reading hosts, snapshots, entries, jobs, alerts, policies, repositories and their reports, agent updates, the agent policy and the notification thresholds.
- Reading the list of API tokens and the administrator account. A visitor sees token names and the administrator's name and address, never a token secret. This is why the stand's account must be synthetic.
- The search, `POST /api/v1/find` and `DELETE /api/v1/find/{id}`, and creating and fetching an archive download.
- `POST /api/v1/snapshots/{id}/summary`, which counts what a selection holds.
- `POST /api/v1/auth/logout`, which ends the visitor's own session.
- The two policy checks that the policy editor needs, `POST /api/v1/policies/validate` and `POST /api/v1/policies/{name}/retention-preview`. Both only read the catalog and answer with a result, and a test compares the database before and after.

Saving, running or deleting a policy stays closed. Any other route under `/api/v1` is refused with 403 `demo_mode`, and the web interface turns that answer into one notice, "Not available in the demo." A path that matches no route gets 404. The allowlist is checked by a test against the OpenAPI document, so a new route is closed until someone lists it on purpose. The API reference, `/api/docs` and `/api/v1/openapi.json`, sits outside the allowlist and is served to any session, so a demo visitor can read it.

**What is closed, and why.**

- Every write not listed above: tokens, settings, policies and their hooks (a hook runs a command on a host), restore to a host, agent registration, agent updates, garbage collection and checks.
- Requests that carry an address from the client, such as testing a repository or a notification channel. They would turn the server into a way to reach other services on its network.
- Bearer tokens. A request with `Authorization: Bearer` is refused before the route is looked at. Other schemes are not treated specially here: they carry no credential the server accepts, so the session and the allowlist still decide.
- HEAD, OPTIONS and any method not in the list.
- The notification channel page. The list of channels answers with an empty list in the demo, so webhook addresses, chat ids and SMTP hosts never leave the server. The server test walks the schema of every allowed response for secret-looking field names and seeds secrets to check that no allowed read returns them.
- `/metrics` is not mounted.
- Archive downloads are limited to two at a time, and each stops after two minutes. A slow download would otherwise hold a thread and keep garbage collection waiting.

**What stays open.** `/install.sh` and `/install/agent/{name}` serve the public installer and the agent binaries, and the archive download serves whatever the demo repository holds. All three are fine only because the data is synthetic.

**Switching the mode.** Sessions of one mode must not outlive the other. When the server starts with a different `demo_mode` than the one stored, it deletes every session before it listens. Fill the demo with data and maintain it only with the flag off, and close the reverse proxy for that time.

**What the server does not provide.** Isolation of the host. A flaw in the allowlist or in the server would put the process under the control of a stranger, so the deployment has to keep the demo away from everything else: rootless containers or user-namespace remapping, no host directories and no container socket mounted, a separate network without a route to other services, CPU, memory and disk limits, a read-only root filesystem, `cap_drop: ALL`, `no-new-privileges`, and only the HTTP port published, behind a reverse proxy with its own rate limit. The gRPC and object storage ports stay unpublished, and the demo uses secrets of its own.
