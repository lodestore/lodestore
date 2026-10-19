# Agent local socket

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
| `policy` | Optional policy name; adds the tag `policy=<name>` to the snapshot. The agent cannot check it: it is a label asserted by the caller, so retention, listing and health must not treat a stream snapshot as belonging to a policy on this basis alone. The server tells stream jobs apart by their origin (`JobCreated`, with `caller_uid`) |

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

A stream is a job of its own on the host with source `cli`, no policy, a generated id, the stream name and the caller. It does not wait for file backups and runs while one is in progress; at most two streams run at the same time.

The job is stored on the agent. While the server is unreachable the job and its result are kept and reported after the agent reconnects. The snapshot appears under the host with its `stdin_name`. A snapshot written directly to the repository with `lode backup --stdin -r ...` appears after the next catalog sync.

A stream has a fixed time limit of 12 hours. A stream that is still being written after that is stopped and writes no snapshot (`time_limit`). The `maxRuntime` of the policy named in the header does not apply to it: the agent does not know the policies, and the limit is what lets garbage collection prove that no running backup references data about to be deleted. Backups that need longer than 12 hours have to run as policy jobs.

The snapshot written through the agent has no parent snapshot, because the agent cannot read the repository.
