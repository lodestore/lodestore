---
title: "Agent local socket"
description: "Stream a database dump to Lodestore through the agent's local unix socket, with no repository password or key on the host."
---
The agent listens on a unix socket so that a user on the host can hand it a stream, for example a database dump, without holding any repository password or key. The agent writes the stream to the repository with its own keys and reports the job to the server like any other backup.

`lode backup --stdin` uses this socket when no repository is configured (no `-r`, no `LODE_REPOSITORY`, no profile repository) and the socket exists. `--agent` and `--agent-socket <PATH>` select it explicitly.

```sh
set -o pipefail; sudo -u postgres pg_dump app | lode backup --stdin --name app.dump
```

`set -o pipefail` matters: when `pg_dump` fails, `lode` still sees a clean end of its input. Only the shell can tell the two apart.

## Socket

| Setting | Value |
| --- | --- |
| Path | `socket` in `/etc/lodestore/agent.yaml`, default `/run/lodestore/agent.sock` |
| Mode | `0660` |
| Owner | `root:lodestore` when the agent runs as root and the group `lodestore` exists |

Only root and members of the `lodestore` group can connect. A user outside the group gets a permission error from the kernel. If the agent cannot bind the socket it logs a warning and keeps running; backups driven by the server are not affected.

The agent reads the caller's user and group id from the connection (`SO_PEERCRED`) and records them on the job as `caller_uid` and `caller_gid`.

## Operations

The socket offers exactly one operation, `backup_stdin`. It cannot read data, start a file backup, run hooks or print keys or credentials. Anything else is answered with `error` / `unsupported` and the connection is closed.

## Protocol, version 1

Every message from the agent is one JSON object per line with `"v": 1` and a `"type"`.

### Request header

One line of JSON ending in `\n`, at most 4096 bytes including the newline. Unknown fields are rejected.

```json
{"v":1,"op":"backup_stdin","name":"app.dump","policy":null}
```

| Field | Meaning |
| --- | --- |
| `v` | Protocol version, `1` |
| `op` | `backup_stdin` |
| `name` | Name of the file inside the snapshot: 1 to 1024 bytes, no `/`, no NUL, not `.` or `..` |
| `policy` | Optional policy name; adds the tag `policy=<name>` to the snapshot. The agent does not check it. The server keeps the name on the job only when the policy is in the repository of the agent and selects this host, and checks that again when the job finishes. Otherwise the job runs without a policy, and the server refuses the snapshot with the tag `policy=<name>`: the job ends `failed` with `snapshot_policy_mismatch` and the snapshot is not linked to the job |

The header must arrive within 10 seconds.

### Frames

After the header the client sends the data as frames: a `u32` big-endian length followed by that many bytes. A frame holds 1 to 1 048 576 bytes.

A frame of length 0 is the commit frame. It states that the stream is complete. Without it the agent does not write a snapshot, whatever the reason the data stopped: a closed connection, a killed `lode`, a failed producer. The job ends as `failed` with `stream_incomplete`. A stream of zero data frames followed by the commit frame is valid and produces a snapshot with an empty file.

### Replies

| `type` | Fields |
| --- | --- |
| `accepted` | `job_id` |
| `backup_progress` | `files_done`, `bytes_done`, `bytes_added`, `current_path`, `files_total`, `bytes_total`, `scan_done`, `bytes_processed`; at most once a second |
| `backup_summary` | `snapshot_id`, `incomplete`, `parent` (always `null`), `stats`, `errors_total`, `warnings_total` |
| `error` | `code`, `message` |

After `backup_summary` or `error` the agent closes the connection.

### Error codes

| Code | Meaning |
| --- | --- |
| `unsupported` | Unknown `op` or protocol version |
| `bad_request` | Malformed or oversized header, invalid `name` or `policy` |
| `busy` | Two streams are already being written; retry later |
| `stream_incomplete` | The stream ended without the commit frame, or a frame was invalid |
| `backup_failed` | The repository could not be opened or written, or the job was cancelled |

## Jobs

A stream is a job of its own on the host with source `cli`, a generated id, the stream name, the caller and the policy name from the header when there is one (`policy_name`). The server accepts the policy name only when the policy is in the repository of the agent and selects this host, when the job is created and again when it finishes; otherwise the job has no policy and a snapshot tagged with one is refused (`snapshot_policy_mismatch`). The server records a CLI job only for a stream: a job without a stream name is not recorded and the agent is told to forget it. A stream is not the policy's own run: its snapshot takes no retention slot and is never forgotten by retention, and it is not the policy's last successful backup for alerts, metrics or the anomaly baseline. A policy that sets `spec.retention.includeStdin: true` opts in, and its stdin snapshots then form a retention pool of their own, so they never push out scheduled or run-now snapshots. It does not wait for file backups and runs while one is in progress; at most two streams run at the same time. The server keeps at most eight open CLI jobs per host and tells the agent to forget any it reports beyond that; a result that finished while the agent was offline is always accepted.

The job is stored on the agent. While the server is unreachable the job and its result are kept and reported after the agent reconnects. The snapshot appears under the host with its `stdin_name`. A snapshot written directly to the repository with `lode backup --stdin -r ...` appears after the next catalog sync.

The server signs the snapshot of a stream that went through the agent, after it has checked it like any other backup. The host in the signature comes from the agent's client certificate, not from the snapshot, so a stream written through one agent is never signed as another host's, and it counts as a signed snapshot in `latest`, in the host picker and in the last snapshot of the host. A snapshot written directly with `lode backup --stdin -r ...` is checked and signed by `lode` itself with the repository password in a repository the server has upgraded, and then shows as signed with the password, without a host. See [Snapshot provenance and `latest`](/docs/security/threat-model/#snapshot-provenance-and-latest).

A stream has a fixed time limit of 12 hours. A stream that is still being written after that is stopped and writes no snapshot (`time_limit`). The `maxRuntime` of the policy named in the header does not apply to it: the agent does not know the policies, and the limit is what lets garbage collection prove that no running backup references data about to be deleted. Backups that need longer than 12 hours have to run as policy jobs.

The snapshot written through the agent has no parent snapshot, because the agent cannot read the repository.
