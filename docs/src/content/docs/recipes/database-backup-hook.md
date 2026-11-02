---
title: Back up PostgreSQL with a hook
description: Dump a PostgreSQL database before each backup with a pre-backup hook, and back up the dump file.
---

A database file copied while the database runs is not a reliable backup. The safe way is to ask the database for a consistent dump, write it to disk and back up that file. A backup policy can do this with a hook: a script on the host that runs before the backup.

This recipe dumps one PostgreSQL database with `pg_dump` and backs up the dump.

## How hooks work

- A hook is a script in `/etc/lodestore/hooks.d` on the backed-up host. The policy only names it. A policy cannot create or change a script, so a policy alone cannot run new code on the host.
- The agent runs the script only if the file and the directory belong to root, are not writable by group or others, and the file is a regular, executable file and not a symlink. Otherwise the job fails with `hook_rejected`; a script that is not there fails with `hook_missing`.
- The hook runs as root with an almost empty environment: `PATH`, `LODESTORE_POLICY`, `LODESTORE_JOB_ID`, `LODESTORE_HOOK_PHASE` (`pre` or `post`) and, in a post hook, `LODESTORE_BACKUP_STATUS` (`success`, `warnings`, `failed` or `cancelled`). Keys, tokens and proxy settings are not passed. Standard input is `/dev/null`.
- A hook that takes longer than its `timeout` (one hour by default, at most `24h`) is stopped together with its child processes.

## Keep the credentials out of the policy

The policy is stored on the server and shown in the web interface and in `lode apply` output. It must not hold a password. The script below needs none: it runs `pg_dump` as the `postgres` operating system user, which PostgreSQL accepts over its local socket (peer authentication). If your database needs a password, put it in a file that only root can read, such as `/root/.pgpass`, and read it from there. The agent does not pass its own environment to the hook, so a variable set elsewhere is not visible to the script.

The argument that the policy passes is the database name. The script checks it, because the server chooses the arguments, not you.

## The script

Save it as `/etc/lodestore/hooks.d/pg-dump.sh`:

```sh
#!/bin/sh
set -eu

db=${1:-}
case $db in
  '' | *[!A-Za-z0-9_]*) echo "pg-dump.sh: bad database name" >&2; exit 2 ;;
esac

dir=/var/backups/postgres
umask 077
install -d -m 0700 "$dir"
tmp=$(mktemp "$dir/.$db.XXXXXX")
trap 'rm -f "$tmp"' EXIT

runuser -u postgres -- pg_dump --format=custom "$db" >"$tmp"
mv "$tmp" "$dir/$db.dump"
```

The dump goes to a temporary file and replaces `app.dump` only when `pg_dump` succeeded. A failed dump therefore never leaves a half-written file that the next backup would store as good. Do not write the dump under `/tmp`: the agent has a private `/tmp` that the database cannot see, and a policy path under `/tmp` would back up that private folder. `/var/backups` is a safe place.

Install it so that the agent accepts it:

```sh
sudo install -d -o root -g root -m 0755 /etc/lodestore/hooks.d
sudo install -o root -g root -m 0755 pg-dump.sh /etc/lodestore/hooks.d/pg-dump.sh
```

Run it once by hand as root, `sudo /etc/lodestore/hooks.d/pg-dump.sh app`, before you rely on it.

## The policy

`hooks.before` lists scripts that run in order before the backup. `args` is passed to the script as arguments. No shell is involved.

```yaml
apiVersion: lodestore.io/v1alpha1
kind: BackupPolicy
metadata:
  name: postgres-nightly
spec:
  repository: main
  selector:
    matchLabels:
      role: db
  paths:
  - /var/backups/postgres
  hooks:
    before:
    - script: pg-dump.sh
      args: [app]
      timeout: 30m
  schedule:
    cron: 30 2 * * *
  retention:
    keepDaily: 7
    keepWeekly: 4
```

Create it in the web interface, or put it in git and use [`lode apply`](/lodestore/recipes/gitops-helm/). The schedule and retention fields are described in [Backup policies](/lodestore/guides/policies/).

## When a hook fails

- A failing **before** hook (a non-zero exit, a timeout, a rejected or missing script) stops the job. No backup is made, the job ends as failed, and the log shows the hook output (the last 64 KiB). That is what you want here: when the dump failed, backing up last night's dump as if it were new would be worse than a visible failure. The alert **No recent backup** fires when this goes on for longer than the policy allows; see [Alerts](/lodestore/guides/alerts/).
- A failing **after** hook does not undo the backup. The job ends with warnings instead of succeeded.
- The time limit of the policy (`limits.maxRuntime`) counts the hooks too.

To remove the dump after the backup, add a script to `hooks.after`. It receives `LODESTORE_BACKUP_STATUS`, so it can keep the file when the backup failed.

Hooks run commands directly. An inline `command:` in a hook is refused unless the host allows it in `/etc/lodestore/agent.yaml` with `hooks: { allow_inline: true }`. Prefer a script.
