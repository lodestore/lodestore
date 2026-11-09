---
title: Restore without the server
description: Restore files from a Lodestore repository with only the bucket and the repository password or recovery key, when the server is gone.
---

This is the promise Lodestore is built around. The server only manages and catalogs. Your data is in the bucket, encrypted, and the `lode` command line client opens it with nothing else: no server, no agent, no database.

A continuous integration job runs the same restore from the last step of the [Quickstart](/docs/quickstart/#restore-without-the-server) on a clean machine for every pull request. Try it once on your own repository before you need it.

## What you need

- The `lode` binary. See [Binary](/docs/install/binary/) for the download and how to check it. It runs on any Linux machine; it does not have to be one of your backed-up hosts.
- The bucket, with its address and an access key that can **read** it. The key of the agent cannot read data; use the server's key, or any other key with read access to the bucket.
- The repository password, or a recovery key. The recovery key is the text that the setup wizard showed you once and that starts with `lode-rk1-`.

If you have none of the keys, the data cannot be opened by anyone, including the project. That is the point of the encryption.

## Restore

Put the password or the recovery key in a file that only you can read. A recovery key goes in the same place as a password:

```sh
umask 077
printf '%s' 'lode-rk1-...' > ./repository-key
```

Tell `lode` where the repository is and how to reach the storage:

```sh
export AWS_ACCESS_KEY_ID=your-read-access-key
export AWS_SECRET_ACCESS_KEY=your-read-secret-key
export AWS_REGION=eu-central-1
export LODE_REPOSITORY="s3:https://s3.eu-central-1.amazonaws.com/my-bucket"
export LODE_PASSWORD_FILE=./repository-key
```

The repository location is `s3:` followed by the storage address and the bucket, and a folder inside the bucket if the repository does not start at its root: `s3:https://host/bucket/folder`. For MinIO or another S3-compatible storage use its address, for example `s3:http://192.0.2.20:9000/my-bucket`. `lode` reads the keys and the region from the standard AWS environment variables or the AWS shared config files. Without `AWS_REGION` it uses `us-east-1`, which is right for most S3-compatible storage but not for AWS.

List the snapshots, then restore:

```sh
lode snapshots
lode restore latest --target ./restored
```

`lode snapshots` prints the oldest first. Each line is a short id, the time in UTC, the host name and the backed-up paths. `latest` is the newest signed snapshot of the whole repository, whichever host made it, judged by the time the storage wrote it, not by the time the snapshot claims. A snapshot is signed when the server checked it or when `lode backup` with the repository password did. `lode` warns when it skips a snapshot that nothing signed or that claims a time in the future. In a repository that no Lodestore 0.2 server has opened there are no signatures yet; `latest` then warns that it is the snapshot the storage wrote last, and you can name a snapshot by its id instead. If no snapshot is signed, `lode` asks for an id. The marks in the listing and the details are in [the command line reference](/docs/reference/cli/#which-snapshot-is-latest). To restore another one, give its id or any unique start of the id from the first column.

`lode restore` reads index shards of up to 1 GiB, and it stops the walk of a snapshot's directory tree that passes the limit of 2^30 entries or 512 GiB of directory data, with an error that names the limit.

Files land under the target folder at their original absolute path: `/etc/hosts` comes back as `./restored/etc/hosts`. The target folder is required.

## Smaller restores

```sh
lode ls latest --long
lode dump latest /etc/hosts > hosts
lode restore latest:/etc/nginx --target ./nginx
lode restore latest --include /home/anna --target ./restored --dry-run
lode restore latest --target ./restored --verify
```

- `lode ls` lists the files of a snapshot, or of one path with `latest:/etc`.
- `lode dump` writes one file to standard output.
- `SNAPSHOT:PATH` restores only that path, directly into the target.
- `--include` takes a path or a `.gitignore`-style pattern, anchored at `/`, and may be repeated.
- `--dry-run` shows what would be restored and creates nothing, not even the target.
- `--verify` reads every restored file back and compares it with the snapshot.
- An existing file is replaced only if its content differs. `--overwrite always` or `--overwrite never` changes that.

## The password and where it is read from

`lode` takes the secret from the first of: `--password-file`, `--password-command`, `LODE_PASSWORD_FILE`, `LODE_PASSWORD`, or a prompt on a terminal. A value that starts with `lode-rk1-` is a recovery key, whichever way it arrives. Prefer a file: `LODE_PASSWORD` is visible to every process that can read the environment of `lode`.

A recovery key that does not parse or fails its checksum makes `lode` exit with code 12.

## After the restore

Check the result before you declare it good: compare a few files, or run with `--verify`.