# Command line

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

### Restore through the server

`lode server restore <snapshot> --host <host>` asks the agent on that host to restore. The snapshot is the full id from `lode server snapshots list`.

| Option | Meaning |
|--------|---------|
| `--path PATH` | Restore only this absolute path; repeat for more. Default: everything |
| `--target DIR` | Folder on the host. Default: `/var/lodestore/restore/<date>-<first 8 of the id>` |
| `--in-place` | Restore over the original files. Needs `--yes` |
| `--conflict POLICY` | `overwrite-changed` (default), `skip-existing` or `stop-if-not-empty` |
| `--no-owners` | Do not restore file owners |

### Output

Lists print a table with the same columns and status words as the web interface. With `--json`, each row is one line of JSON in the usual `{"v":1,"type":...}` envelope; times are Unix milliseconds.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Done |
| 1 | The server or the network failed; the message is the server's |
| 2 | Wrong flags, the server address or token is not set, or `lode apply` found an invalid file |
| 4 | `lode apply --dry-run` found changes |
| 13 | The token was refused: unknown, expired or revoked, or it lacks the scope named in the message |
