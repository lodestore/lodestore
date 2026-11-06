---
title: "Command line"
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
| `lode server policies delete <name> [--yes]` | `policies:write` |
| `lode server policies run <name>` | `jobs:run` |
| `lode server policies retention <name>` | `policies:read` |
| `lode server jobs list [--state S] [--policy NAME] [--host HOST] [--kind K] [--page N]` | `jobs:read` |
| `lode server jobs show <id>` | `jobs:read` |
| `lode server jobs logs <id> [--follow]` | `jobs:read` |
| `lode server jobs run --host <host> [--policy NAME]...` | `jobs:run` |
| `lode server jobs cancel <id>` and `lode server jobs rerun <id>` | `jobs:run` |
| `lode server snapshots list --host <host> [--limit N]` | `snapshots:read` |
| `lode server snapshots mark <id> --reason TEXT` and `clear <id> --reason TEXT` | `snapshots:delete` |
| `lode server snapshots delete <id> [--yes]` | `snapshots:delete` |
| `lode apply -f <file or folder> [--dry-run] [--prune] [--adopt] [--manager NAME] [--yes]` | `policies:read` and `policies:write`, plus `snapshots:delete` when the files set or change `retention` |
| `lode gc [--dry-run] <repo>`, `lode gc status <repo>`, `lode gc unlock <repo>` (also `lode server gc ...`) | See [Cleanup](#cleanup) |
| `lode server check <repo> [--level structure\|packs\|restore]` | See [Integrity checks](#integrity-checks) |
| `lode server find <pattern> [--host HOST]... [--label KEY=VALUE]...` | `snapshots:read`; see [Find a file](#find-a-file) |
| `lode server restore <snapshot> --host <host> [options]` | `restore`, and `snapshots:read` when `--target` and `--in-place` are not given |

A `<host>` is a host id or a hostname. A hostname is looked up in the host list, which needs `hosts:read`; a token without that scope must pass the id, which `lode server hosts list` shows in the `ID` column. When several hosts share a name, `lode` asks for the id.

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

The token needs `policies:read` and `policies:write`. When the files add or change a `retention` block, it also needs `snapshots:delete`, because retention makes the server delete snapshots; without it the server refuses with exit code 13. A problem in a file is printed as `file.yaml:LINE:COL: message` on stderr, with exit code 2.

A CI job that fails on drift and one that applies:

```sh
lode apply -f policies/ --prune --dry-run
lode apply -f policies/ --prune
```

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

| Option | Meaning |
|--------|---------|
| `--path PATH` | Restore only this absolute path; repeat for more. Default: everything |
| `--target DIR` | Folder on the host. Default: `/var/lodestore/restore/<date>-<first 8 of the id>` |
| `--in-place` | Restore over the original files. Needs `--yes` |
| `--conflict POLICY` | `overwrite-changed` (default), `skip-existing` or `stop-if-not-empty` |
| `--no-owners` | Do not restore file owners, setuid/setgid bits or file capabilities |
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
| 3 | `lode check` or `lode server check` found problems in the repository |
| 4 | `lode apply --dry-run` found changes |
| 13 | The token was refused: unknown, expired or revoked, or it lacks the scope named in the message |

## Backup and restore without the server

`lode backup`, `lode snapshots`, `lode ls`, `lode dump` and `lode restore` work straight against a repository, with its password or recovery key and no server. See [Restoring without the server](/docs/recipes/restore-without-server/). This section covers what changed with snapshot signatures; [Snapshot provenance and `latest`](/docs/security/threat-model/#snapshot-provenance-and-latest) explains what they protect and where they stop.

### Which snapshot is `latest`

`latest` (also `latest:/path`) in `lode restore`, `lode ls` and `lode dump` is not the newest by the time in the manifest, which the host that wrote it sets. It is the newest snapshot that carries a signature, ordered by the time the storage reports for the snapshot object. Equal times are broken by the larger snapshot id.

- A snapshot is signed when the Lodestore server has checked it, or when `lode backup` with the repository password has checked and signed it itself. `lode snapshots` marks the rest.
- `lode` explains the choice on stderr. It warns about snapshots that claim a time in the future, about unsigned snapshots stored after the chosen one (name one by id to restore it), and about signature records that could not be opened. A signed snapshot stored more than ten minutes in the future is skipped with a warning; if every signed snapshot is, the command exits with code 1.
- If the repository holds signatures but no snapshot is signed, the command exits with code 1, like an unknown id, and asks for an id. `lode snapshots` lists them.
- In a repository that no Lodestore 0.2 server has opened, nothing is signed yet. `latest` is then the snapshot the storage wrote last, ignoring any stored more than ten minutes in the future, and `lode` warns that a host with writer credentials could have written it. If every snapshot is stored in the future, the command exits with code 1 and asks for an id. In this mode `latest` protects against nothing a host with writer credentials can do; name the snapshot by its id, or open the repository with a 0.2 server first.
- On a machine where `lode` has already seen signatures in a repository, it remembers that in a cache file (below). If the signatures later disappear, as when `locks/attest-*` objects are deleted from a folder repository, `latest` exits with code 1 and asks for an id instead of falling back to the rule above, and `lode backup` exits with an error and writes nothing. A machine without that file cannot know, and falls back.

```sh
lode snapshots
lode snapshots --json
```

`lode snapshots` prints a mark after the paths when the repository holds signatures: `unverified` for a snapshot nothing signed and `owner` for one signed with the repository password. A snapshot signed by the server has no mark. In a repository without signatures there are no marks. With `--json`, each snapshot has `verified_by`: `"server"`, `"owner"` or `null`.

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
