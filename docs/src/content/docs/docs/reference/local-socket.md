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

For a user the socket offers one operation, `backup_stdin`. It cannot read data, start a file backup, run hooks or print keys or credentials. Anything else is answered with `error` / `unsupported` and the connection is closed.

The operations whose names start with `pve_` belong to the Proxmox VE backup provider of the agent; see [Proxmox provider operations](#proxmox-provider-operations). Nobody else needs them, and the agent refuses them to every caller that is not that provider.

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

The header must arrive within 5 seconds. A connection that sends nothing in that time gets `bad_request` and is closed, so idle connections do not hold a connection slot for long. Root has 16 slots of its own for connections that have not sent their first line yet; every other caller shares another 16. A connection past them gets `busy`.

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

A stream is a job of its own on the host with source `cli`, a generated id, the stream name, the caller and the policy name from the header when there is one (`policy_name`). The server accepts the policy name only when the policy is in the repository of the agent and selects this host, when the job is created and again when it finishes; otherwise the job has no policy and a snapshot tagged with one is refused (`snapshot_policy_mismatch`). The server records a CLI job only for a stream: a job without a stream name is not recorded and the agent is told to forget it. A stream is not the policy's own run: its snapshot takes no retention slot and is never forgotten by retention, and it is not the policy's last successful backup for alerts, metrics or the anomaly baseline. A policy that sets `spec.retention.includeStdin: true` opts in, and its stdin snapshots then form a retention pool for each stream name (one pool per `--name`, not one shared pool), so they never push out scheduled or run-now snapshots or the snapshots of a stream of another name. A stdin snapshot is a snapshot of files: it competes for `latest` like any other `files` snapshot. It does not wait for file backups and runs while one is in progress; at most two streams run at the same time. The server keeps at most eight open CLI jobs per host and tells the agent to forget any it reports beyond that; a result that finished while the agent was offline is always accepted.

The job is stored on the agent. While the server is unreachable the job and its result are kept and reported after the agent reconnects. The snapshot appears under the host with its `stdin_name`. A snapshot written directly to the repository with `lode backup --stdin -r ...` appears after the next catalog sync.

The server signs the snapshot of a stream that went through the agent, after it has checked it like any other backup. The host in the signature comes from the agent's client certificate, not from the snapshot, so a stream written through one agent is never signed as another host's, and it counts as a signed snapshot in `latest`, in the host picker and in the last snapshot of the host. A snapshot written directly with `lode backup --stdin -r ...` is checked and signed by `lode` itself with the repository password in a repository the server has upgraded, and then shows as signed with the password, without a host. See [Snapshot provenance and `latest`](/docs/security/threat-model/#snapshot-provenance-and-latest).

A stream has a fixed time limit of 12 hours. A stream that is still being written after that is stopped and writes no snapshot (`time_limit`). The `maxRuntime` of the policy named in the header does not apply to it: the agent does not know the policies, and the limit is what lets garbage collection prove that no running backup references data about to be deleted. Backups that need longer than 12 hours have to run as policy jobs.

The snapshot written through the agent has no parent snapshot, because the agent cannot read the repository.

## Proxmox provider operations

On a Proxmox VE node the agent installs a backup provider (a Perl module that vzdump and `pct restore` load through the `lodestore` storage). The provider only passes messages: it holds no repository key and writes no backup data. The provider and the storage plugin find the socket in `LODESTORE_AGENT_SOCKET`, which the agent sets for the processes it starts, then in `socket` of `/etc/lodestore/agent.yaml`, then at `/run/lodestore/agent.sock`. The storage counts as active only while a connection to the socket succeeds, so a socket file left by a stopped agent does not. It talks to the agent over this socket with these operations:

| `op` | Called when | Fields besides `v` and `token` |
| --- | --- | --- |
| `pve_job_check` | vzdump starts a job for the storage | none |
| `pve_ct_open` | vzdump has prepared a container backup | `vmid`, `info` (what vzdump passes as `$info`: `directory`, `sources`, `backup-user-id`, and when present `bandwidth-limit`, `firewall-config`, `guest-uid`, `guest-rtime`, `cluster-id`) |
| `pve_ct_data` | vzdump hands over the container configuration | `config` (UTF-8 text), `excludes` |
| `pve_ct_finish` | vzdump ends the backup | `success`, `error` |
| `pve_restore_config` | `pct restore` asks for the configuration of the snapshot (twice per restore) | `restore_id` |
| `pve_restore_open` | `pct restore` asks where the files are | `restore_id` |
| `pve_restore_close` | `pct restore` is done with the files | `restore_id` |

You do not call these operations. The agent serves them only when all of this holds:

- The caller is root (`SO_PEERCRED`).
- `token` is the token of a job the agent started, 64 lowercase hex digits. The agent gives it to the process it starts in the environment variable `LODESTORE_JOB_TOKEN`, one token per container and per restore, and compares it in constant time. The token never appears in the agent's log, in a reply or in the provider's output.
- The token was issued for this `vmid`, or for this `restore_id`.
- The calling process descends from the process the agent started for the job.

A backup runs over one connection: `pve_ct_open`, then `pve_ct_data`, then `pve_ct_finish`. The agent answers `pve_ct_data` when it has finished reading the container, and `pve_ct_finish` when the snapshot is committed or dropped. A token opens one backup connection only. The restore operations each use a connection of their own, and the agent returns for `pve_restore_config` exactly the bytes it registered for the restore, so the two calls of one restore always agree. The reply never carries firewall rules: the snapshot's firewall configuration is not restored, because Proxmox VE writes whatever the provider returns for it into `/etc/pve/firewall/{vmid}.fw` without checking it.

### Messages

One JSON object per line, in both directions, at most 262 144 bytes per line. Every request carries `"v"`, the provider protocol version, which is `1` today. A request from a provider outside the range the agent supports is refused with `protocol_mismatch` and the text `run lodestore-agent pve install on this node`: update the plugin files of the node.

```json
{"v":1,"op":"pve_job_check","token":"<job token>"}
```

A reply is `{"ok":true,...}` or `{"ok":false,"code":"...","message":"..."}`. `pve_ct_finish` answers with `archive_size`, `pve_restore_config` with `config`, and `pve_restore_open` with `directory`. After a refusal, and after `pve_job_check`, `pve_ct_finish` and the restore operations, the agent closes the connection.

The first line of a connection must arrive within 5 seconds, like the header of `backup_stdin`. At most 8 provider connections are served at the same time; a ninth gets `busy`. They count apart from the connections of `backup_stdin`. Only root gets a provider slot, and a caller that is not root cannot take one of the 16 slots root has for its first line, so no other member of the `lodestore` group can keep the provider out.

### Refusal codes

| Code | Meaning |
| --- | --- |
| `forbidden` | The caller is not root |
| `bad_token` | The token is malformed, unknown, no longer valid, or already used for a backup connection |
| `wrong_guest` | The token was issued for another guest or another restore |
| `wrong_process` | The calling process is not part of the job |
| `protocol_mismatch` | The provider's protocol version is outside the supported range |
| `bad_request` | A malformed, missing or oversized field, a line over the limit, or a message out of order |
| `unsupported` | An unknown `pve_` operation |
| `busy` | Too many provider connections |
| `refused` | The agent declined the step; `message` says why |
| `aborted` | The job is no longer running on the agent |

A backup that does not come from Lodestore (the Proxmox VE GUI, Datacenter → Backup, a vzdump started by hand) has no token and fails in the provider with `backups to this storage are started by Lodestore; start this backup from Lodestore`. Deleting backups is refused too: the storage plugin answers `deletion is managed by the Lodestore server`, and the agent has no operation that deletes data.
