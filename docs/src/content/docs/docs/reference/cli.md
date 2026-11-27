---
title: "Command line"
description: "The lode command line: manage a Lodestore server from a terminal or CI with an API token, and back up, restore and check repositories directly."
---
## Managing a server

`lode server` manages a Lodestore server from a terminal or from CI with an API token. It needs no repository and no repository password.

Create a token in the web interface under Settings, API tokens. The token is shown once. Give it only the scopes the commands below need.

The address and the token come from:

| What | Order of lookup |
|------|-----------------|
| Server address | `--server`, then `LODESTORE_SERVER`, then `server` of the `--profile` |
| Token | `LODESTORE_TOKEN`, then `token` of the `--profile` |

There is no flag for the token, so it never shows up in the process list. In a profile, keep the file readable by you only (`chmod 600`); `lode` prints a warning on stderr when a profile with a token sits in a file that group or others can read:

```toml
[profiles.ci]
server = "https://lodestore.example.com"
token = "lsk_..."
```

```sh
export LODESTORE_SERVER=https://lodestore.example.com
export LODESTORE_TOKEN=lsk_...
lode server hosts list
```

An `http://` address that is not on the loopback interface works, but the token then travels without TLS and `lode` prints a warning on stderr.

### Commands

| Command | Scope |
|---------|-------|
| `lode server hosts list` | `hosts:read`; the repository name and last backup columns need `snapshots:read` too and show `-` without it |
| `lode server hosts show <host>` | `hosts:read` |
| `lode server hosts label <host> KEY=VALUE...` or `--clear` (replaces all labels) | `hosts:write` |
| `lode server policies list` | `policies:read` |
| `lode server policies get <name>` (prints the YAML) | `policies:read` |
| `lode server policies delete <name> [--yes]` | `policies:write`, plus `snapshots:delete` for a policy with retention |
| `lode server policies run <name>` | `jobs:run`; see [Run limit](#run-limit) |
| `lode server policies retention <name>` | `policies:read` |
| `lode server jobs list [--state S] [--policy NAME] [--host HOST] [--kind K] [--page N]` | `jobs:read` |
| `lode server jobs show <id>` | `jobs:read` |
| `lode server jobs logs <id> [--follow]` | `jobs:read` |
| `lode server jobs run --host <host> [--policy NAME]...` | `jobs:run`; see [Run limit](#run-limit) |
| `lode server jobs cancel <id>` and `lode server jobs rerun <id>` | `jobs:run`; `rerun` is subject to the [run limit](#run-limit) |
| `lode server snapshots list --host <host> [--limit N] [--source-type TYPE] [--source KEY]` | `snapshots:read` |
| `lode server snapshots mark <id> --reason TEXT` and `clear <id> --reason TEXT` | `snapshots:delete` |
| `lode server snapshots delete <id> [--yes]` | `snapshots:delete` |
| `lode policy validate <file>...` | None; checks the files offline, see [Validate policy files](#validate-policy-files) |
| `lode apply -f <file or folder> [--dry-run] [--prune] [--adopt] [--manager NAME] [--yes]` | `policies:read` and `policies:write`, plus `snapshots:delete` when the files set or change `retention`, run a policy with retention more often, change `paths`, `exclude`, `oneFileSystem`, `excludeCaches`, `source`, `schedule.timezone` or `repository` of a policy with retention, or (with `--prune`) delete a policy with retention, and `hooks:write` when they change `hooks` or bring more hosts under a policy with hooks |
| `lode gc [--dry-run] <repo>`, `lode gc status <repo>`, `lode gc unlock <repo>` (also `lode server gc ...`) | See [Cleanup](#cleanup) |
| `lode server check <repo> [--level structure\|packs\|restore]` | See [Integrity checks](#integrity-checks) |
| `lode server find <pattern> [--host HOST]... [--label KEY=VALUE]...` | `snapshots:read`; see [Find a file](#find-a-file) |
| `lode server restore <snapshot> --host <host> [options]` | `restore:host`, or `restore:in-place` with `--in-place` or when owners are restored (see [Restore through the server](#restore-through-the-server)); and `snapshots:read` when `--target` and `--in-place` are not given |

A `<host>` is a host id or a hostname. A hostname is looked up in the host list, which needs `hosts:read`; a token without that scope must pass the id, which is in the address of the host's page in the web interface (and in the `ID` column of `lode server hosts list`, which itself needs `hosts:read`). `lode server restore` says to pass the id or use a token with `hosts:read` when the server does not know the name as a host id. When several hosts share a name, `lode` asks for the id.

Running a policy that already has a queued or running job on a host does not queue a second one. The command prints `Job <id> on <host> is already queued` for it.

Deleting a policy asks for confirmation on a terminal. Without a terminal it needs `--yes`, or it exits with code 2.

### GitOps with lode apply

`lode apply` makes the policies on the server match YAML files, so they can live in Git and be applied from CI. The server checks all files first: when any file is invalid, nothing is applied.

`-f` is a file or a folder. A folder is searched recursively for `*.yaml` and `*.yml`, in path order, and a file may hold several documents separated by `---`.

The output lists what changes and ends with the totals:

```
+ policy/etc-and-home    create
~ policy/db-nightly      update
    spec.schedule.cron          "0 2 * * *" -> "30 2 * * *"
    spec.retention.keepDaily    (unset) -> 14
- policy/old-test        delete  (managed by lode-apply)
Plan: 1 to create, 1 to update, 1 to delete, 4 unchanged.
```

The last line starts with `Applied:` when something was applied. Colors appear only on a terminal and not when `NO_COLOR` is set. With `--json` every change is one `change` line and the totals are one `summary` line.

| Option | Meaning |
|--------|---------|
| `--dry-run` | Change nothing. Exits 4 when the server differs from the files, so a CI step can detect drift |
| `--prune` | Also delete the policies of this manager that the files no longer contain. Policies of other managers and policies made in the web interface are never deleted |
| `--adopt` | Take over a policy of the same name that was made in the web interface. Without it such a name is an error |
| `--manager NAME` | The name under which the policies are tracked. Default `lode-apply` |
| `--yes` | Skip the question of `--prune` on a terminal |

On a terminal, `--prune` shows the plan first and asks `Delete N policies? [y/N]`. Without a terminal, as in CI, `--prune` itself is the consent and deletes without asking. Run `--dry-run` first when you want to see the deletions before they happen. `--prune` refuses to run, with exit code 2, when the files contain no policy at all (an empty file or one with only comments), so a template that renders to nothing cannot delete every policy of the manager.

The token needs `policies:read` and `policies:write`. When the files add or change a `retention` block, it also needs `snapshots:delete`, because retention makes the server delete snapshots; without it the server refuses with exit code 13. The same scope is needed to run a policy that has retention more often: the server compares the shortest gap between two runs of the cron schedule over the next 366 days, in the time zone of the policy, before and after the change. A schedule that runs less often, or at another time of day, needs only `policies:write`. The scope is also needed to change `paths`, `exclude`, `oneFileSystem`, `excludeCaches`, `source`, `schedule.timezone` (a missing zone is UTC) or `repository` of a policy that has retention before or after the edit, and to delete a policy with retention, which `--prune` does when the files no longer contain it; `--dry-run` shows the refusal beforehand, and without the scope the command exits with code 13. Reordering paths is not a change; reordering or repeating an exclude is, and `/etc` and `/etc/` differ.

When the files add, change or remove `hooks`, or widen a policy that has hooks (change its `repository`, or its selector other than to a shorter `hosts` list or more `matchLabels`), whichever hosts are enrolled, the token also needs `hooks:write`, because hooks run commands as root on the hosts. The server then refuses the whole run with exit code 13 and names the scope and the policies. `--dry-run` shows the same refusal before anything is applied. Deleting a policy without retention, with or without `--prune`, needs only `policies:write`. A problem in a file is printed as `file.yaml:LINE:COL: message` on stderr, with exit code 2.

A CI job that fails on drift and one that applies:

```sh
lode apply -f policies/ --prune --dry-run
lode apply -f policies/ --prune
```

### Validate policy files

`lode policy validate <file>...` reads one or more policy YAML files, each with any number of documents, and checks them offline with the rules the server applies on `lode apply`: unknown fields, the schedule, retention, limits, `spec.paths` and `spec.source` (a `source` needs `apiVersion: lodestore.io/v1alpha2`). It needs no server address and no token. A problem is printed as `file.yaml:LINE:COL: message` on stderr, and the command exits with code 3 when any file has one, 0 otherwise. With `--json` it prints one report with the problems of each file. It checks the shape of the policy, not what a server runs: a policy with a source other than `files` is valid here and is refused by this version of the server with 422.

### Run limit

A token can start the same policy on the same host at most 6 times per hour, through `lode server policies run`, `lode server jobs run` and `lode server jobs rerun`. The seventh request gets a `429` from the server with `Retry-After`; `lode` prints the server's message, which says how long to wait, and exits with code 1. A web session is not limited. Scheduled runs do not count.

### Snapshots

`lode server snapshots list` shows `Suspicious`, `Incomplete` and `Policy deleted` as words in the status column.

```sh
lode server snapshots mark <id> --reason "ransom note found"
lode server snapshots clear <id> --reason "restored and checked"
lode server snapshots delete <id>
```

A suspicious snapshot is not counted by retention and is never forgotten automatically. `clear` returns it to normal retention. `delete` removes the snapshot from the repository, from Restore and from lode. This cannot be undone, and storage space is freed later by garbage collection. On a terminal it asks first; without a terminal it needs `--yes`, or it exits with code 2. When the storage refuses the deletion because of Object Lock, the snapshot still leaves the catalog and Restore; `lode` prints a warning and exits 0.

These commands need the `snapshots:delete` scope, which is separate from reading.

### Retention preview

`lode server policies retention <name>` takes the retention rules saved in the policy and prints which snapshots they would forget now, per host. Suspicious snapshots are counted separately and are never forgotten. Nothing is deleted.

### Cleanup

Retention only forgets snapshots; cleanup frees the space they used. See [Cleanup](/docs/guides/gc/) for how it works and how to turn it on.

```sh
lode gc --dry-run main
lode gc main
lode gc status main
lode gc unlock main
```

`lode gc` and `lode server gc` are the same command. `<repo>` is a repository id, or the name of a repository that has at least one policy: a name is looked up in the policy list, which needs `policies:read`. A repository without a policy must be given by its id. A repository named `status` or `unlock` must be given by its id too.

| Command | What it does | Scope |
|---------|--------------|-------|
| `lode gc --dry-run <repo>` | Starts a dry run, waits for it and prints what a cleanup would free. Works while cleanup is off. Changes nothing in the bucket | `jobs:run` and `jobs:read` |
| `lode gc <repo>` | Starts a cleanup and waits until it has marked its packs, then prints when deletion happens. The deletion itself follows after the pause; it does not wait for it | `snapshots:delete` and `jobs:read` |
| `lode gc status <repo>` | Prints the state of the running cycle and the last cleanup and dry run | `jobs:read` |
| `lode gc unlock <repo> [--yes]` | Closes the markers of backups that never finished | `snapshots:delete` and `jobs:read` |

A dry run prints the same facts as the web interface:

```
Dry run for repository main
  Would free           12.4 GiB
  To delete            318 packs
  To repack            41 packs
  Repack transfer      8.4 GiB
  Held by Object Lock  3.2 GiB, frees after 24 Oct 2026
```

`Repack transfer` is printed only when packs are repacked, and `Held by Object Lock` only when something is held. Times are in UTC. With `--json` the command prints the full report as one line, with the lists of packs to delete and to repack (`delete`, `repack`), unreadable and unindexed objects, and locked versions.

A cleanup that cannot start prints the reason, one line for each failed safety check, and exits with code 1. `lode gc status` names the phase in words: `Checking snapshots`, `Rewriting packs`, `Marked for deletion`, `Deletion is postponed`, `Checking again before deleting` or `Deleting unused data`. `Deletion is postponed` means unfinished backups hold the deletion back; the hosts are listed under it.

`lode gc unlock` prints the hosts cleanup is waiting for and asks for confirmation on a terminal. Without a terminal it needs `--yes`, or it exits with code 2 and sends nothing. If there is nothing to unlock it says so and exits with code 0.

There is no command to abort a cycle; use **Abort cycle** in the repository card.

### Integrity checks

Two commands check that a repository is whole and restorable. [Integrity checks](/docs/guides/checks/) explains the three levels and what they find.

```sh
lode check -r s3://backups/main
lode check -r s3://backups/main --level packs --read-limit 2GiB
lode server check main --level restore
```

| Command | Where it runs | Needs |
|---------|---------------|-------|
| `lode check [--level L] [options]` | On this machine, straight against the repository. No server is involved. | The repository and its password or recovery key, like `lode ls`. |
| `lode server check <repo> [--level L]` | On the server, with the server's key. The command starts it, follows it and prints the report. | A token with `jobs:run` and `jobs:read`, and `policies:read` when `<repo>` is a name, as for `lode gc`. |

`--level` is `structure` (the default), `packs` or `restore`. The options of `lode check` that limit the work:

| Option | Level | Meaning |
|--------|-------|---------|
| `--read-limit SIZE` | `packs` | Read at most this much pack data, for example `5GiB` (the default). Packs are read in random order. |
| `--files N` | `restore` | Restore this many files, 1 to 1000. Default 20. |
| `--max-data SIZE` | `restore` | Stop when the restored files reach this size. Default `1GiB`. They are written to a temporary folder that is removed afterwards. |

`lode check` downloads what it reads to the machine it runs on, so a `packs` check of a cloud bucket costs transfer there; use `lode server check` when the server sits closer to the data. `lode server check` takes its limits from the repository's check settings and cannot be changed per run. It fails with exit code 1 and the server's message when a cleanup or another check holds the repository. If the connection to the server drops, the check continues on the server; see the Jobs page.

The output uses the words of the web interface:

```
Pack reading check for repository s3://backups/main (direct)
  Result            Passed
  Packs read        812
  Data read         4.9 GiB
  Blobs verified    190,344
  Damaged blobs     0
```

A server run prints `(server)` instead of `(direct)`, and for `packs` adds how much of the repository has been read in the last 30 days. When problems are found they follow under `Problems`, at most 20 lines and then "and N more.", and notes under `Notes`. With `--json` the command prints the report as one `check` line (`check_report` for the server run) with the complete lists.

The exit code tells a script what happened: 0 when the check passed, 3 when it found problems, 1 when it could not finish. A `packs` or `restore` check that had nothing to read, because the repository has no packs, no snapshots or no files, exits with 0 and prints `Nothing to check` instead of `Passed`; with `--json` the `result` is `nothing_to_check` (the other values are `passed`, `failed` and `stopped`) and `nothing_to_check` gives the reason.

### Find a file

`lode server find <pattern>` looks for a file in every snapshot that is still stored, on every host, including hosts that have no agent. It is the terminal form of the **Find file** tab.

```sh
lode server find nginx.conf
lode server find '*.conf' --host web-1 --label role=web
lode server find '/etc/nginx/**' --json
```

The pattern is one of three kinds, and case does not matter:

| Pattern | Matches |
|---------|---------|
| `nginx.conf` (no `/`, no glob characters) | Names that contain the text |
| `*.conf`, `nginx.?onf` | Names that match the glob |
| `/etc/nginx/**`, `*/logs/*.log` | Whole paths that match the glob; `**` crosses folders, `*` does not |

| Option | Meaning |
|--------|---------|
| `--host HOST` | Search this host, by name or id, including `agentless:<hostname>` keys; repeat for more. An unknown host exits with code 2 before anything is searched. |
| `--label KEY=VALUE` | Also search hosts with this label; repeat for more, a host must carry all of them. |

`--host` and `--label` add up: `--host web-1 --label role=web` searches web-1 and every host labelled role=web.

There is no option for a period. Use the **Find file** tab, or `since` and `until` of `POST /api/v1/find`.

The command starts the search, waits for it and prints one row per host and path:

```
HOST    PATH                    VERSIONS  LAST SEEN            SNAPSHOT
web-1   /etc/nginx/nginx.conf   3         2026-10-29 12:00:00  4f0c...
old-box /srv/nginx.conf         1         2026-10-02 03:10:00  9a1d...
```

`VERSIONS` counts the different contents the file had. `SNAPSHOT` is the full id of the snapshot the command restores: the first one the server lists for the first version, a signed one whenever the version has any, otherwise the newest. Versions that a signature covers (`verified_by` in the JSON) come before those that nothing signed, and `LAST SEEN` is when the server received the snapshot, so a snapshot with a manifest time far in the future does not become the newest version. Below the table the command prints how to restore the first row. When the host has an agent that is online, this is a ready command:

```
lode server restore <snapshot> --host web-1 --path /etc/nginx/nginx.conf
```

For a host without an agent, or with an agent that is offline, it says so and shows the same command with `--host <HOST>`: restore to another host, or download the file from the web interface. The command line has no download.

A search that hits a limit prints `Stopped at N matches.` or `Stopped after N minutes.` on stderr, prints what it found and exits with 0: the result is incomplete, not wrong. Narrow it with `--host`, `--label` or a longer pattern. With no match the command prints `No files match "<pattern>".` and exits with 0.

With `--json` the command prints one `find_session` line after the search ends. It has the fields of the REST `FindSession` (`state`, `stop_reason`, `limit`, `scanned`, `total`, `error`, and so on) and all groups in `groups`, each with its `versions` and their snapshots, signed ones first. Scripts check `state` (`done` or `incomplete`) and `stop_reason` (`limit` or `timeout`).

Exit codes: 0 when the search finished or stopped at a limit, 1 when the search failed (the message is the server's) or the network failed, 2 for an unknown host, a label without `=` or an invalid pattern, 13 when the token lacks `snapshots:read`.

### Restore through the server

`lode server restore <snapshot> --host <host>` asks the agent on that host to restore. The snapshot is the full id from `lode server snapshots list`.

The token needs `restore:host` to restore into a folder, and `restore:in-place` with `--in-place`. The two scopes are independent: `restore:in-place` does not allow a restore into a folder. This command restores files: for a snapshot of a guest or a database the server answers 422 ("This snapshot is a proxmox-vm backup and cannot be restored as files.") and `lode` exits with code 1 with that message. A snapshot of a disk image is the one exception: with no `--path`, or `--path /`, it is restored whole as one sparse file `NAME.img` in the target folder, and any other `--path` restores segments as files. `--in-place` is refused for an image. Because the default `--conflict overwrite-changed` could replace an existing `NAME.img`, such a restore needs `restore:in-place` (and a session needs its password within 15 minutes); with `--conflict skip-existing` or `stop-if-not-empty` `restore:host` is enough. An image with too many holes to describe in a plan of 4 MiB is refused with 422 and a pointer to `lode restore`. The four scopes `restore:guest`, `restore:guest-in-place`, `restore:db` and `restore:db-in-place` are reserved for those restores; no command uses them yet, and neither `restore:host` nor `restore:in-place` includes them. Downloading files or an archive in the web interface or through the API is a third scope, `restore:download`; it does not allow a restore onto a host.

By default the command also restores file owners, POSIX ACLs and other `system.*` attributes, `security.*` and `trusted.*` extended attributes, setuid/setgid bits and file capabilities. The agent keeps the setuid/setgid bits and the capabilities only in a restore in place; into a new folder it restores the owners and drops them. A token may ask for any of it only with `restore:in-place`; a token with `restore:host` alone must pass `--no-owners`, or the server refuses with 403 and names the scope. The local `lode restore`, run by whoever holds the bucket, always restores owners, ACLs and labels.

The agent decides where it writes. It accepts a restore into a folder only below one of the `allowed_restore_roots` in its `agent.yaml` (by default `/var/lib/lodestore/restore`), and a restore in place only when `allow_in_place` is set there; see [Restoring on the host](/docs/install/binary/#restoring-on-the-host). Otherwise the job fails with `restore_outside_roots` or `restore_in_place_disabled`, whether it was started by a token or in the web interface. The server knows the roots from the agent's heartbeat and refuses such a request at once with 400 and the same codes. A token cannot start a restore on an agent that is too old to report its roots (`restore_agent_outdated`); update the agent first.

| Option | Meaning |
|--------|---------|
| `--path PATH` | Restore only this absolute path; repeat for more. Default: everything |
| `--target DIR` | Folder on the host, below one of the agent's `allowed_restore_roots`. Default: `<date>-<first 8 of the id>` below the first restore folder the host reports, `/var/lib/lodestore/restore` when it reports none or the token cannot read the host list (`snapshots:read`) |
| `--in-place` | Restore over the original files. Needs `--yes` and an agent with `allow_in_place` |
| `--conflict POLICY` | `overwrite-changed` (default), `skip-existing` or `stop-if-not-empty` |
| `--no-owners` | Do not restore file owners, POSIX ACLs, `security.*` or `trusted.*` extended attributes, setuid/setgid bits or file capabilities. Without it the command asks for them, which a token may do only with `restore:in-place` |
| `--accept-unverified` | Restore a snapshot that nothing signed (see [Snapshot provenance](/docs/security/threat-model/#snapshot-provenance-and-latest)). Without it the server refuses with `restore_unverified_not_confirmed`; an unsigned snapshot is never restored in place, with or without the flag (`restore_unverified_in_place`). A signed snapshot ignores the flag |

`lode server snapshots list` shows the snapshots newest first by the time the server received them, not by the time written in the manifest.

### Output

Lists print a table with the same columns and status words as the web interface. With `--json`, each row is one line of JSON in the usual `{"v":1,"type":...}` envelope; times are Unix milliseconds.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Done |
| 1 | The server or the network failed; the message is the server's |
| 2 | Wrong flags, the server address or token is not set, `lode apply` found an invalid file, `lode gc` got an unknown repository, or `lode gc unlock` has no terminal and no `--yes` |
| 3 | `lode check` or `lode server check` found problems in the repository, or `lode policy validate` found a file with problems |
| 4 | `lode apply --dry-run` found changes |
| 13 | The token was refused: unknown, expired or revoked, or it lacks the scope named in the message |

## Backup and restore without the server

`lode backup`, `lode snapshots`, `lode ls`, `lode dump` and `lode restore` work straight against a repository, with its password or recovery key and no server. See [Restoring without the server](/docs/recipes/restore-without-server/). This section covers what changed with snapshot signatures; [Snapshot provenance and `latest`](/docs/security/threat-model/#snapshot-provenance-and-latest) explains what they protect and where they stop.

### Repository location

`-r`, `--repo` (or `LODE_REPOSITORY`, or the `repository` of a profile) names the repository:

| Form | Storage |
|------|---------|
| `/path/to/folder` | A local folder |
| `s3:https://host/bucket[/folder]` | S3 or an S3-compatible storage |
| `azblob://<container>[/<root>]` | Azure Blob Storage |
| `gcs://<bucket>[/<root>]` | Google Cloud Storage |
| `sftp://<user>@<host>[:<port>][/<path>]` | An SFTP server; the path is the repository folder on the server, an empty path is the login folder |

A location carries no secrets. `lode` refuses a URL with a query string or a fragment, or one that has `user:secret@` in front of the name, and its error does not repeat the URL. Secrets come from the environment:

| Variable | Used for | Meaning |
|----------|----------|---------|
| `LODE_AZBLOB_ACCOUNT_NAME` | `azblob://` | The storage account name. Required |
| `LODE_AZBLOB_ACCOUNT_KEY`, `LODE_AZBLOB_SAS_TOKEN` | `azblob://` | The account key or a SAS token. Exactly one is required |
| `LODE_AZBLOB_ENDPOINT` | `azblob://` | The service address. Default `https://<account>.blob.core.windows.net` |
| `LODE_GCS_CREDENTIAL_FILE` | `gcs://` | A service account key file (JSON). Without it, and without `LODE_GCS_ENDPOINT`, `lode` uses the standard Google credential chain |
| `LODE_GCS_ENDPOINT` | `gcs://` | The service address, for an emulator or a private endpoint. Without a key file it also turns off request signing and the credential chain |
| `LODE_SFTP_KEY_FILE` | `sftp://` | The OpenSSH private key of the user, without a passphrase. Required |
| `LODE_SFTP_HOST_KEY` | `sftp://` | The server's host key, one line: `ssh-ed25519 AAAA...`, or the matching line of `ssh-keyscan -p <port> <host>`. Required |
| `LODE_SFTP_PUBLISH` | `sftp://` | `hardlink` (default) or `direct`. `lode` never changes it by itself |

`lode` does not trust an SFTP host key on first use. Without `LODE_SFTP_HOST_KEY` it stops before it connects and says how to get the key. A key that differs from the pinned one stops the connection.

S3 credentials come from the standard AWS environment variables and shared config files, as before.

### Creating a repository

```sh
lode -r azblob://backups repo init --recovery-key-file ./repository.key
```

`lode repo init` first checks the password, the recovery key destination and that the repository does not exist yet; these errors and their exit codes are unchanged. It then writes and removes small probe objects to measure what the credentials it runs with can do to the backups, on every storage including a local folder. The result is printed after a successful init as `storage guarantee: <level>`, one of `create_only_enforced`, `overwrite_keeps_old_version`, `host_can_overwrite_or_delete` or `unchecked`, followed by the reasons.

If the credentials can overwrite or delete backups (`host_can_overwrite_or_delete`), or the check could not measure it (`unchecked`), `lode repo init` stops, says what a holder of these credentials can do, and creates nothing. A local folder always gives `host_can_overwrite_or_delete`, because every host that can write to it can also change it. To create the repository anyway, pass the flag:

| Option | Meaning |
|--------|---------|
| `--accept-weaker-guarantee` | Create the repository although a holder of these credentials could overwrite or delete backups, or although this could not be measured |

`lode` does not read the lifecycle rules of a bucket. On S3, Azure Blob and GCS it therefore shows a bucket that keeps earlier versions of overwritten objects as `host_can_overwrite_or_delete` with the reason that its lifecycle rules could not be read, and asks for the flag; on a bucket with a create-only key it prints the same reason as a note. The server reads the rules and applies them. `--json` output is unchanged.

### Restoring a very large directory

`lode restore` holds a directory in memory whole and stops at a directory of more than 2^23 entries or 1 GiB of tree data (the decoded length of its pieces), or at more than 2^24 entries or 2 GiB of tree data held at once across the directories it has open, each directory counted at its full length. The error names the limit: `entries in one directory`, `bytes in one directory`, `entries held at once` or `bytes held at once`. `lode backup` refuses such a tree and writes no snapshot, so a snapshot made by this version never needs more. A snapshot written before backups refused them can. The error names the limit and the flag:

| Option | Meaning |
|--------|---------|
| `--allow-large-directories` | Restore a snapshot with a directory above 2^23 entries or 1 GiB of tree data, or with more than 2^24 entries or 2 GiB of tree data held at once. Each such directory is held in memory whole. The limits of the whole walk (2^30 entries, 512 GiB of directory data) stay |

The server never lifts these limits; the flag acts only in the `lode restore` process it is given to.

### Which snapshot is `latest`

`latest` (also `latest:/path`) in `lode restore`, `lode ls` and `lode dump` is not the newest by the time in the manifest, which the host that wrote it sets. It is the newest snapshot of `files` that carries a signature, ordered by the time the storage reports for the snapshot object. Equal times are broken by the larger snapshot id. A snapshot of another source (a guest or a database) is never chosen by plain `latest`, and a WAL batch is never chosen by anything.

A snapshot of a disk image is never chosen by plain `latest`. Use `latest@image/NAME`, which needs signatures: in a repository where nothing is signed it finds no snapshot, and the snapshot is named by its id.

`latest@<source key>` (also `latest@<source key>:/path`) chooses by the same rules among the signed snapshots of one source instance, for example `lode ls latest@postgres/7123456789012345678/16384`. It is the newest of the signed snapshots of that key in the repository, as `latest` is for files (the server API chooses the latest of a source inside one host only), so a guest that moved to another node keeps its newest backup under the same key. When the signed snapshots of the key come from more than one host of the repository, for example a clone, a PostgreSQL standby or a restored copy, `lode` does not choose: it exits with code 1 and names the hosts, one per host id with the name of its newest record, the id and age of its newest signed snapshot, newest host first, up to five and then "and N more":

```
the snapshots of <key> come from more than one host: "web" ("h1") newest 3f9a12bc 1h ago, "db" ("h2") newest 7c0de4a1 2d ago; add --host with a host id to choose one
```

`--host HOST` (in `lode ls`, `lode dump` and `lode restore`, only with `latest@<source key>`) then chooses among the snapshots that host made. `HOST` is a host id carried by any signed record of the repository, or else a host name. A value in the form of a UUID is always a host id and is never looked up as a name; if no signed snapshot carries it, the command exits with code 1. A name that belongs to more than one host that has snapshots of the key exits with code 1 (`"web" names more than one host with snapshots of <key>`). A name is accepted with a warning that gives the host id, and the old name when the host was renamed; pass the id to avoid it. When a value matches a host id and is also the name of other hosts, the id wins and a warning names those hosts. When the host has no signed snapshot of the key, the error names the host and lists the hosts that have one, with the id and age of their newest snapshot. An empty `--host`, `--host` without `latest@<source key>` and `latest@` without a key are usage errors (exit code 2). A key held by one host needs no `--host`. A snapshot that `lode backup` signed with the repository password carries no host id, so its host name stands for the host: such snapshots of one key made under two host names, for example two machines that each back up a disk image named `sda`, also need `--host` with the host name. The key is the canonical key of [ADR 0007](/docs/adr/0007-source-model/); a key that is not canonical is a usage error (exit code 2), and a key with no signed snapshot exits with code 1. The character is `@` because `:` already separates the path.

- A snapshot is signed when the Lodestore server has checked it, or when `lode backup` with the repository password has checked and signed it itself. `lode snapshots` marks the rest.
- `lode` explains the choice on stderr. It warns about snapshots that claim a time in the future, about unsigned snapshots stored after the chosen one (name one by id to restore it), and about signature records that could not be opened. A signed snapshot stored more than ten minutes in the future is skipped with a warning; if every signed snapshot is, the command exits with code 1.
- If the repository holds signatures but no snapshot is signed, the command exits with code 1, like an unknown id, and asks for an id. `lode snapshots` lists them.
- In a repository that no Lodestore 0.2 server has opened, nothing is signed yet. `latest` is then the snapshot of `files` that the storage wrote last (a snapshot whose manifest claims another source is skipped, and `latest@<source key>` finds none), ignoring any stored more than ten minutes in the future, and `lode` warns that a host with writer credentials could have written it. If every snapshot is stored in the future, the command exits with code 1 and asks for an id. In this mode `latest` protects against nothing a host with writer credentials can do; name the snapshot by its id, or open the repository with a 0.2 server first.
- On a machine where `lode` has already seen signatures in a repository, it remembers that in a cache file (below). If the signatures later disappear, as when `locks/attest-*` objects are deleted from a folder repository, `latest` exits with code 1 and asks for an id instead of falling back to the rule above, and `lode backup` exits with an error and writes nothing. A machine without that file cannot know, and falls back.

```sh
lode snapshots
lode snapshots --json
```

`lode snapshots` prints a mark after the paths when the repository holds signatures: `unverified` for a snapshot nothing signed and `owner` for one signed with the repository password. A snapshot signed by the server has no mark. In a repository without signatures there are no marks. With `--json`, each snapshot has `verified_by`: `"server"`, `"owner"` or `null`, `source_type` (`files` for a snapshot nothing signed a source for) and `source_label`, a label such as "VM 101 · web" or `null`. `lode snapshots --source-type TYPE` lists the snapshots of one signed source type (`files`, `proxmox-vm`, `proxmox-ct`, `postgres`, `postgres-wal`, `mysql` or `image`; any other value is a usage error). `lode server snapshots list` takes `--source-type TYPE` and `--source KEY` (one source instance on that host, by its canonical key) and prints the same two fields.

### Disk images

`lode backup --image SOURCE` backs up a regular file or a block device, read whole, or an NBD export, read whole or by a dirty bitmap, as a snapshot of type `image`. See [Back up a disk image](/docs/recipes/disk-image-backup/) for the walk-through.

| Option | Meaning |
|--------|---------|
| `--image SOURCE` | The file or block device to read, or an NBD export `nbd+unix:///EXPORT?socket=PATH`. Any other value with `://` is refused (TCP and TLS NBD are not accepted), and so is anything else than a regular file, a block device or such an export, such as a FIFO. `PATH` must be an absolute path to the unix socket. A source that changed while it was read (a file's size or times, a device's size) fails the backup without a snapshot; a mounted or held device gets a warning. Cannot be combined with paths, `--stdin`, `--parent`, `--force-rescan`, `--exclude`, `--exclude-file`, `--one-file-system` or `--exclude-caches` |
| `--name NAME` | The name of the image, 1 to 200 bytes of UTF-8 with no `/` or control character, not `.` or `..`. Default: the file name of `SOURCE`, or the name of the NBD export (an export without a name needs `--name`). `--name` also works with `--stdin`. Percent-encoded it is at most 506 bytes (each byte outside `A-Z a-z 0-9 - . _ ~` counts as three); a longer name is refused before the source is read |
| `--bitmap BITMAP` | With an `nbd+unix://` `--image` only (code 2 otherwise): the dirty bitmap of the export that has collected the changes since the previous backup of this image, named `lode-{repo16}-{inst16}-g<N>` with `<N>` from 1. The export must also offer `lode-{repo16}-{inst16}-g<N+1>`. The name must be the one of this repository, host and image, which `lode` prints after every backup of an NBD export. Not available with `--writer-keys`, because an increment reads its parent snapshot |

The key of the image is `image/NAME` ([ADR 0007](/docs/adr/0007-source-model/)). With `--json`, `backup_summary` has an `image` object with `key`, `size` (bytes) and `read_mode` (`full` or `incremental`); without it the command prints `snapshot ID saved` and one line about the image. A wrong name, a value with `://` that is not an `nbd+unix://` export and an incompatible flag exit with code 2. In a repository with signatures the snapshot is signed with the repository password after it is checked, with the source `image/NAME`.

`lode restore SNAPSHOT --target DIR` of such a snapshot writes one sparse file `DIR/NAME.img`. `lode restore SNAPSHOT:/disks/NAME --target DIR` writes its segments (`000000`, `000001`, ...) as ordinary files directly in `DIR`, and joining them in the order of their names gives the image. `--include` cannot be used with a whole image (code 2). If the signatures cannot be read, restoring a whole snapshot by id still works: `lode` warns and restores it as the image its manifest names.

| Option | Meaning |
|--------|---------|
| `--to-device DEVICE` | Write the image to a block device instead of `--target`. Linux only. Cannot be combined with `--target`, `--include`, `--dry-run`, `--verify` or `--overwrite`. Holes are written as zeros, bytes past the end of the image are not changed, and the data is flushed at the end. Before the first write the command refuses a device that is not a block device, is smaller than the image, is mounted (or has a mounted partition), is used as swap or is held by another device |
| `--accept-unverified` | With `--to-device` only (code 2 otherwise): in a repository with signatures, write a snapshot that nothing signed, or whose signatures cannot be read |

The device is opened with `O_EXCL`. A process that has the device open without an exclusive lock, such as a virtual machine that uses it as a disk, is not seen by the check: stop it before the restore. If the write fails after it has started, the command says the device may hold a partial image. `lode restore --verify` and `lode check --level restore` verify the files of an image by slices of 4 MiB.

#### Incremental backup of an NBD export

`lode backup --image nbd+unix:///EXPORT?socket=PATH --bitmap lode-{repo16}-{inst16}-g<N>` reads the export through its own NBD client and, when everything checks out, only the ranges that the bitmap marks as dirty. The rest comes from the previous snapshot of the image on this host, and a chunk that is partly dirty is merged with the bytes of that snapshot, checked by their id. The snapshot is an ordinary snapshot of an image. The export must offer both `g<N>` and `g<N+1>`; the walk-through with the commands and the rules for creating and deleting the bitmaps is in the [recipe](/docs/recipes/disk-image-backup/#incremental-backups-from-a-dirty-bitmap), and [ADR 0007](/docs/adr/0007-source-model/) records the rule that decides between a full and an incremental read.

Without `--name` the image is named after the export, and when `lode`'s own record of the previous backup shows that another export is now behind that name, it refuses and asks for `--name`. With `--name` a changed export is a full read (`source_changed`). Only one backup of an instance runs on a machine at a time. Without a cache folder (neither `HOME` nor `XDG_CACHE_HOME` is set) every backup of an NBD export is a full read.

With `--json`, the `image` object of `backup_summary` for an NBD export has these fields besides `key`, `size` and `read_mode`:

| Field | Meaning |
|-------|---------|
| `full_reason` | After a full read, why: one of the codes below. Not present after an increment |
| `bitmap` | The name of the bitmap that was read. Not present without `--bitmap` |
| `next_bitmap` | The bitmap to pass as `--bitmap` to the next backup |
| `bitmap_to_create` | The bitmap to create before the next backup starts, which that backup exports together with `next_bitmap` |

`parent` at the top of `backup_summary` is the snapshot an increment was made against, and `null` after a full read. A backup of an image file has no `full_reason`, `bitmap`, `next_bitmap` or `bitmap_to_create`. Without `--json` the line about the image says `incremental read from ID` or `full read (REASON)`, followed by `next backup: create NAME before it starts, then pass --bitmap NAME`. A note on standard error says when `g<N+1>` was not exported before the read (the run is still incremental and the next one is a full read), and when no bitmap was given.

The codes of `full_reason`:

| Code | The read was full because |
|------|---------------------------|
| `no_bitmap` | no `--bitmap` was given |
| `bitmap_missing` | the export does not offer the bitmap |
| `no_state` | this machine has no record of the previous backup of the image |
| `state_unreadable` | that record is damaged or unreadable |
| `source_changed` | the export is not the one the previous backup read (with `--name`) |
| `no_parent` | the repository holds no snapshot of the image from this host |
| `parent_not_newest` | the newest snapshot of the image is not the one that was recorded |
| `parent_unreadable` | the previous snapshot has no binding or size, or cannot be read |
| `name_changed` | the bitmap recorded in the previous snapshot is not named for this repository, host and image |
| `gen_mismatch` | the generation of `--bitmap` is not the recorded one, or the chain of generations has forked |
| `size_changed` | the disk has another size than at the previous backup |

Exit code 2: a `--bitmap` that is not a name of this repository, host and image (the message gives the form, `...-g<N>`, with `N` from 1), `--bitmap` with a file or a block device or with `--writer-keys`, an NBD address that is not `nbd+unix:///EXPORT?socket=PATH`, an export with no name and no `--name`, and an export that changed behind a name given only by the default. An NBD peer that breaks the protocol or stops answering, and a second backup of the same instance on the machine, exit with code 1 and write no snapshot; the other exit codes of `lode` apply as usual. `lode` removes its record of the previous backup before it connects to the export and writes it back only after the snapshot is confirmed, so after a failure of the peer, the source or the repository the next backup is a full read. A refusal before that point (code 2, or the lock) leaves the record as it was.

### Backup in a repository the server has upgraded

When a 0.2 server has opened a repository, `lode backup` with a password or recovery key changes in four ways. A repository that no 0.2 server has opened behaves as before.

- It deduplicates only against index shards the server signed, and against shards this machine opened itself, not against every shard in `index/`. A shard planted by a compromised host cannot make the backup unrestorable.
- Before it exits with 0, it opens the frames its snapshot depends on and checks that the snapshot resolves, the way the server does after an agent backup. If the data does not check out, `lode backup` exits with code 1: the snapshot is left unsigned, `latest` skips it, the failing shard is remembered and the next run does not use it. A backup that could not be checked says so and also exits with 1.
- When the check passes, it signs the snapshot with the repository password (`owner` in `lode snapshots`) by adding one object `locks/attest-*`. With cleanup turned off, these objects accumulate, one per `lode backup`; see [Cleanup](/docs/guides/gc/#attestations-and-cleanup).
- An incremental backup picks its parent only among snapshots signed for this host name, ordered by the time the storage reports. `--parent latest` follows the rules of `latest` above. An explicit `--parent` is used as given, with a warning when it is not signed.

The first strict backup on a machine opens the frames of every shard its snapshot depends on, so in a large repository it reads much of the stored data and costs that much transfer. Later runs check only shards not seen before. Every open for writing, like every `latest`, also reads all `locks/attest-*` objects; with cleanup turned off their number grows with every backup, and the read grows with it.

The memory of the checks is in `$XDG_CACHE_HOME/lode/<repository id>/` (`$HOME/.cache/lode/...` when the variable is not set): `verified-shards`, `distrusted-shards` and `upgraded`, readable by you only. Deleting the folder costs a repeat of the first run and the loss of the `upgraded` memory above, and of the list of shards caught lying, which the next backup finds again. With neither `XDG_CACHE_HOME` nor `HOME` set, `lode` warns and checks the whole set on every backup.

`lode backup --writer-keys` cannot check the backup, so in such a repository it exits with an error and writes nothing. It also refuses when the key cannot read `locks/`, so it cannot tell, unless `--no-gc-check` says no server cleans up the repository, and on a machine that remembers the repository was upgraded. Back up through the agent (`lode backup --stdin` over its socket), or use the password or recovery key. In a repository no 0.2 server has opened, `--writer-keys` works as before and nothing checks the backup.

`lode backup --stdin` through the agent is signed by the server for the host of the agent; see [Agent local socket](/docs/reference/local-socket/).

## Proxmox VE node

`lodestore-agent pve install` prepares a Proxmox VE node for container backups. Run it as root on every node of the cluster, after the agent is installed there:

```sh
lodestore-agent pve install
```

It installs the Lodestore storage plugin and backup provider into `/usr/share/perl5`, reloads `pvedaemon`, `pveproxy`, `pvestatd` and `pvescheduler`, and writes the node's marker into `/etc/pve/lodestore/plugin/`. Then it waits until every node of the cluster has a marker with the same plugin and pve-manager 9.2.33 or newer, printing one line per node (`ready`, `waiting` or `outdated (...)`), and only then adds the `lodestore` storage (`pvesm add lodestore lodestore --content backup --prune-backups keep-all=1`). The storage is added once for the cluster, by whichever waiting node gets there first; running the command again changes nothing that is already in place.

| Flag | Meaning |
|------|---------|
| `--allow-guest-in-place` | Allow restoring a container over an existing one on this node: sets `restore.allow_guest_in_place: true` in this node's `agent.yaml` and restarts the agent. Other nodes keep their own setting, and the server cannot change it. Rewriting `agent.yaml` drops its comments |
| `--wait-seconds N` | How long to wait for the other nodes, default 600 |
| `--config-dir DIR` | Where `agent.yaml` is, default `/etc/lodestore` |

Every node needs pve-manager 9.2.33 or newer. Install the plugin on a new node before you join it with `pvecm add`: a node without the plugin drops the `lodestore` section of the shared `storage.cfg` the first time it writes that file. Run the command again on a node after an agent update, when the alert `pve_plugin_missing` is open for it, or when a job failed with `protocol_mismatch`.

| Code | Meaning |
|------|---------|
| 0 | The plugin is on every node and the storage exists |
| 1 | A file, a command or `storage.cfg` failed; the message says which |
| 2 | Refused: not root, not a Proxmox VE node, or pve-manager older than 9.2.33 |
| 3 | Not every node got ready within `--wait-seconds`; the message lists the nodes where the command still has to run |
