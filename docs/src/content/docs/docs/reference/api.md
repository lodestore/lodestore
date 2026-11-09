---
title: "REST API"
---
The server describes its REST API as an OpenAPI 3.1 document and renders it as an interactive reference.

- `/api/docs` shows the API reference. Open it from the user menu in the UI (API reference). It needs a signed-in administrator session, like the rest of `/api/v1`.
- `/api/v1/openapi.json` is the raw OpenAPI document, with the same access rule.
- The reference works without internet access: the viewer (Scalar, MIT) is embedded in the server binary and nothing is loaded from a CDN.
- The same document is available offline with `lodestore-server openapi`.

## API tokens

Scripts and CI call the API with an API token instead of a session.

1. Create the token in the web interface under Settings, API tokens. Pick the scopes it needs and an expiry. The token starts with `lsk_` and is shown once.
2. Send it in the `Authorization` header: `Authorization: Bearer lsk_...`. A request with a Bearer header is judged by the token alone; a session cookie sent with it is ignored. No CSRF token is needed.

A route that takes a token needs one scope. Routes not listed below work with a signed-in administrator session only.

| Scope | Routes |
| --- | --- |
| `hosts:read` | `GET /api/v1/hosts`, `GET /api/v1/hosts/{id}`, `GET /api/v1/fleet`, `GET /api/v1/fleet/hosts/{id}`, `GET /api/v1/alerts`, `GET /api/v1/alerts/count`, `GET /api/v1/agent-updates` |
| `hosts:write` | `PUT /api/v1/hosts/{id}/labels`, `POST /api/v1/hosts/{id}/revoke` |
| `alerts:write` | `PUT /api/v1/hosts/{id}/silence`, `DELETE /api/v1/hosts/{id}/silence` |
| `agents:update` | `POST /api/v1/agent-updates`, `POST /api/v1/agent-updates/{id}/pause`, `POST /api/v1/agent-updates/{id}/resume`, `POST /api/v1/agent-updates/{id}/cancel`, `POST /api/v1/agent-updates/{id}/dismiss` |
| `policies:read` | `GET /api/v1/policies`, `GET /api/v1/policies/{name}`, `POST /api/v1/policies/validate`, `POST /api/v1/policies/{name}/retention-preview`, `GET /api/v1/hosts/{id}/policies` |
| `policies:write` | `POST /api/v1/policies`, `PUT /api/v1/policies/{name}`, `DELETE /api/v1/policies/{name}`, `POST /api/v1/policies/apply`, `PUT /api/v1/repositories/{id}/checks/settings` |
| `jobs:read` | `GET /api/v1/jobs`, `GET /api/v1/jobs/{id}`, `GET /api/v1/jobs/{id}/events`, `GET /api/v1/repositories/{id}/checks`, `GET /api/v1/repositories/{id}/checks/runs/{run_id}`, `GET /api/v1/repositories/{id}/checks/runs/{run_id}/report`, `POST /api/v1/repositories/{id}/checks/settings/validate` |
| `jobs:run` | `POST /api/v1/jobs/{id}/cancel`, `POST /api/v1/jobs/{id}/rerun`, `POST /api/v1/policies/{name}/run`, `POST /api/v1/hosts/{id}/run`, `POST /api/v1/repositories/{id}/checks/{level}/run`, `POST /api/v1/repositories/{id}/checks/runs/{run_id}/cancel` |
| `snapshots:read` | `GET /api/v1/hosts/{id}/snapshots`, `GET /api/v1/restore/hosts`, `GET /api/v1/restore/snapshots`, `GET /api/v1/snapshots/{id}`, `GET /api/v1/snapshots/{id}/entries`, `GET /api/v1/snapshots/{id}/search`, `POST /api/v1/snapshots/{id}/summary`, `POST /api/v1/find`, `GET /api/v1/find/{id}`, `GET /api/v1/find/{id}/groups/{index}`, `DELETE /api/v1/find/{id}` |
| `snapshots:delete` | `PUT /api/v1/snapshots/{id}/suspicious`, `DELETE /api/v1/snapshots/{id}` |
| `hooks:write` | No route of its own: a second scope next to `policies:write` or `hosts:write`, see below |
| `restore:download` | `POST /api/v1/downloads`, `GET /api/v1/downloads/{id}/archive` |
| `restore:host` | `POST /api/v1/restores` into a new folder (`in_place: false`) |
| `restore:in-place` | `POST /api/v1/restores` with `in_place: true`, and with `restore_owners: true` in either mode |
| `metrics:read` | `GET /metrics`, see [Metrics](/docs/reference/metrics/) |

The OpenAPI document lists the scope next to the `bearer` requirement of each of these routes.

**401 and 403.** An unknown, revoked or expired token answers `401` `unauthorized`. A valid token without the scope of the route answers `403` `forbidden` with "The token needs the `<scope>` scope.".

**Retention needs a second scope.** A retention rule decides which snapshots the server deletes after the next backup. Because of that, a token may create a policy with `spec.retention`, or change `spec.retention` of a policy (`includeStdin` included), only if it also has `snapshots:delete`. The same holds for making the schedule of a policy with retention run more often: the smallest gap between two runs over the next 366 days, in the time zone of the policy, must not get smaller; a new schedule with no two runs in that period (such as `* * 29 2 *`), or one with more than 100000 runs in it, counts as smaller. A cron limited to some months, such as `0 3 * 12 *`, is compared over the whole horizon, so it is caught in any month; a rarer schedule or a shifted time needs only `policies:write`. This applies to `POST /api/v1/policies`, `PUT /api/v1/policies/{name}` and `POST /api/v1/policies/apply`, dry runs included. Otherwise the request answers `403` `forbidden` with "The token needs the snapshots:delete scope to set or change retention." (for a more frequent schedule: "... to run a policy with retention more often.") and nothing is saved. A policy without retention, or an edit that leaves retention and schedule as they are, needs only `policies:write`. A signed-in administrator session is not affected.

**Hooks need a second scope.** A hook runs a command on the host as root. A token needs `hooks:write` in addition to the scope of the route to create a policy with hooks, to add, change or remove hooks of a policy, and to widen a policy that has hooks, whichever hosts are enrolled: any change of `repository`, and any change of `selector` that is not provably narrower (only a shorter `hosts` list, or the same `matchLabels` with labels added, is narrower; an empty selector, a changed or removed label, a new host id and a switch between `hosts` and labels are not). `PUT /api/v1/hosts/{id}/labels` with labels that bring the host under such a policy needs it too. This applies to `POST /api/v1/policies`, `PUT /api/v1/policies/{name}` and `POST /api/v1/policies/apply`, dry runs included. Otherwise the request answers `403` `forbidden` with "The token needs the hooks:write scope to change hooks or the hosts they run on (policies: a, b)." and nothing is saved. `PUT /api/v1/policies/{name}` answers `409` `policy_conflict` before any of these checks when `base_revision` is not the stored revision, and `PUT /api/v1/hosts/{id}/labels` answers `409` when the labels changed while the request ran. Deleting a policy needs only `policies:write`. A signed-in administrator session is not affected.

**Restore needs the scope of its kind.** `POST /api/v1/restores` takes `restore:host` for a restore into a new folder and `restore:in-place` for a restore in place; neither includes the other or `restore:download`. A token with neither scope answers `403` `forbidden` with "The token needs the restore:host or restore:in-place scope."; a token with the wrong one answers "The token needs the restore:host scope to restore into a folder on the host." or "... the restore:in-place scope to restore in place.". `restore_owners: true`, which restores file owners, setuid/setgid bits and file capabilities, needs `restore:in-place` of a token in both modes (the agent keeps setuid/setgid bits and file capabilities only in a restore in place; into a new folder it restores the owners and drops them), with "... to restore file owners, setuid/setgid bits or file capabilities."; `lode server restore` sends it by default, so with a token that holds only `restore:host` add `--no-owners`. Tokens that held `restore` before an upgrade hold `restore:download` only; a CI job that started restores gets `403` with the name of the scope it needs and needs a new token.

**Starting jobs is limited for tokens.** A token can start a policy on a host (`POST /api/v1/policies/{name}/run`, `POST /api/v1/hosts/{id}/run`, `POST /api/v1/jobs/{id}/rerun`) at most 6 times in an hour per pair of policy and host. The next request answers `429` `rate_limited` with a `Retry-After` header holding the seconds until a slot is free, and no job is created. Sessions are not limited, but their runs count in the hour. Two parallel requests can get one run past the limit, so the limit holds exactly for sequential requests and allows at most 7 under a race.

**What a token cannot do.** A token does not manage tokens, the administrator password, join tokens, repositories, the catalog or server settings. Those routes answer `401` to any token.

A restore or download started with a token records `token:<name>` as who started it, the same field that holds the administrator name for a session.

## Directory tree limit

A request that walks a snapshot's directory tree (`GET /api/v1/snapshots/{id}/entries`, an archive download) stops when the walk passes 2^30 entries or 512 GiB of directory data, counting every visit, and answers `400` `tree_too_large` with a message that names the limit. A restore job for an agent that hits it fails with the same message. The search reports it as `stop_reason` `walk_limit`, see [Find a file](#find-a-file).

## Snapshot actions

Three requests change or remove a snapshot. Each works with a signed-in administrator session or with an API token that has the `snapshots:delete` scope; a token without it gets `403` with "The token needs the snapshots:delete scope.".

- `PUT /api/v1/snapshots/{id}/suspicious` with `{"suspicious": true, "reason": "..."}` marks the snapshot, and `{"suspicious": false, "reason": "..."}` clears the mark. The reason is required in both directions and holds 1 to 200 characters, otherwise the answer is `422` "Enter a reason.". The response is the updated snapshot: `suspicious` carries `automatic`, the detected `reasons`, the administrator's `note`, `marked_by` and `marked_at`, and is `null` when the snapshot is not marked. A marked snapshot is not counted by retention and is never forgotten automatically; clearing the mark returns it to the normal rules.
- `DELETE /api/v1/snapshots/{id}` removes any snapshot, marked or not, the same way retention forgets one. The answer is `{"id": "...", "storage_kept": false}` when the manifest was deleted from the repository. When the repository refuses the deletion (for example under Object Lock), the snapshot still leaves the catalog and the restore lists, and the answer is `{"storage_kept": true, "reason": "..."}`. A temporary storage failure answers `502` and keeps the snapshot. An unknown id answers `404`.

Snapshot lists (`GET /api/v1/hosts/{id}/snapshots`, `GET /api/v1/restore/snapshots`, `GET /api/v1/snapshots/{id}`) also report `received_at`, `suspicious`, `unverified_reason` and `policy_deleted`, which is true when the snapshot was made by a policy that no longer exists. `unverified_reason` is the reason the server could not verify the data of the snapshot after the backup (the data failed the check, or the check could not finish within an hour), and is `null` when the snapshot is not marked. `GET /api/v1/hosts/{id}/snapshots` accepts a token with `snapshots:read`.

Snapshot lists are ordered by `received_at`, the time the server saw the snapshot, newest first (ties: the larger id first), never by the time written in the snapshot manifest, which anyone with write access to the bucket can set. `verified_by` says who signed the provenance of a snapshot: `server` when the server checked a backup job of the host named by its certificate, `owner` when `lode backup` signed it with the repository password, and `null` when nothing signed it. A snapshot that nothing signed is never the latest of its host, is not counted by retention and cannot be restored in place.

`GET /api/v1/restore/hosts` reports, for each host, `latest_snapshot_ms`, the server time (`received_at`) of its newest signed snapshot, `null` when it has none, and `unverified_count`, the number of its snapshots that nothing signed. `GET /api/v1/restore/snapshots` adds `latest_verified_id`, the id of the newest signed snapshot of the host, whatever page is returned; its `next` cursor counts `received_at`.

`POST /api/v1/restores` refuses a snapshot that nothing signed: with `in_place` it answers `400` `restore_unverified_in_place`; into another folder it answers `400` `restore_unverified_not_confirmed` unless the body holds `"accept_unverified": true`. A signed snapshot ignores the field. Downloads are not restricted.

The agent restores only into the folders of `restore.allowed_restore_roots` in its `agent.yaml` (`/var/lib/lodestore/restore` when the key is not set) and restores in place only with `restore.allow_in_place: true`, which is off by default. It checks this for every job, whoever started it. The server answers early from what the agent reported: `400` `restore_outside_roots` for a new folder outside the roots and `400` `restore_in_place_disabled` for a restore in place on a host that does not allow it, each with the roots or the setting to change. A token also gets `400` `restore_agent_outdated` when the agent does not report the capability `restore_roots_v1` (an agent from 0.1); a session is not refused for that. The server checks it again when the job is handed to the agent and fails the job with `restore_agent_outdated` if the agent no longer reports it. These answers are hints; the agent decides when the job runs. `GET /api/v1/restore/hosts` reports for each host `restore_roots`, the folders the agent restores new folders into (`null` when the agent has not reported them, which is an agent from 0.1 or a host without an agent, and `[]` when it allows no restore into a new folder), and `allow_in_place`, `true`, `false` or `null` when unknown.

## Find a file

`POST /api/v1/find` searches the file trees of all live snapshots of all hosts and answers `202` with `{"id": "..."}`. It needs a session or a token with `snapshots:read`.

The body is `{"pattern": "...", "hosts": [], "labels": [], "since": null, "until": null}`. `pattern` holds 1 to 255 characters, case does not matter, and has three modes:

- no `/` and no glob characters: a name fragment, `nginx.conf` matches every file whose name contains it;
- no `/` but with glob characters (`*`, `?`, `[...]`, `{a,b}`): a glob for file names, `*.conf`;
- with a `/`: a glob for the whole path, `/etc/nginx/**`; `*` does not cross a `/`, `**` does.

The filters narrow the search. `hosts` lists host keys (an agent id or `agentless:{hostname}`); `labels` lists `key=value` pairs. The two add up (a union): the search covers the snapshots of the hosts in `hosts` and of the hosts whose agent carries all of the `labels`, so a host without an agent is found through `hosts` only; with both empty it covers every host; `since` and `until` are the times the server received the snapshots, in milliseconds, both ends included, and narrow the whole union. `hosts` holds at most 500 keys (repeats count once) and `labels` at most 20. A host key the server does not know, too many hosts or labels, a label without `=` and `since` later than `until` answer `422` with the name of the field in `errors[].path`. At most 8 searches are queued or running at a time; one more answers `429`.

Poll `GET /api/v1/find/{id}`. The answer holds `state`, the progress (`scanned`, `total`, `skipped`), `searched_hosts` (the hosts whose snapshots the search reads), `hosts` (the hosts with a match) and `groups`, one per host and path, each with its versions (distinct contents) and the first snapshots of each version. All times in the answer (`last_seen_ms`, `first_ms`, `last_ms` and the `time_ms` of a snapshot) are when the server received the snapshot, not the time in the snapshot's manifest, which the host chooses. A version has `verified_by` (`server`, `owner` or `null`, as on a snapshot): the signer of a snapshot holding it, `null` when none is signed. Versions are listed signed first, then by `last_ms`, newest first, so a snapshot that nothing signed does not appear as the newest version of a file. Each snapshot of a version has `verified_by` too, and the snapshots of a version are listed signed first, then newest first, so the first snapshot of a signed version, the one the interface and `lode server find` restore, is signed. `?after=<revision>` returns only the groups that changed since that revision of the session. `GET /api/v1/find/{id}/groups/{index}` returns one group with every snapshot of each version.

`state` is `running`, `done`, `incomplete`, `failed` or `cancelled`. `incomplete` means a limit stopped the search and what it found is kept; `stop_reason` is then `limit` (more files than `max_matches`), `timeout` or `walk_limit` (a snapshot's directory tree passed the budget of 2^30 entries or 512 GiB of directory data for one walk). A snapshot that cannot be read, for example because retention or GC removed it during the search, is counted in `skipped` and does not fail the search. So is a snapshot whose tree holds more paths than a search keeps in memory. `gone` compares a file only with later complete snapshots of the same host and source (policy, stdin name or backup paths). The time limit counts from the moment the search starts running, not while it waits for a slot. A failed search reports a generic `error`; the details are in the server log. `DELETE /api/v1/find/{id}` stops a search (`204`, also when it is over already); an unknown id answers `404`.

Searches live in the server's memory. A finished search is forgotten an hour later, and only the 32 newest finished searches are kept. Two searches run at a time.

The limits come from the optional `find` section of `server.yaml`; zero and negative values fail the start:

```yaml
find:
  max_matches: 1000
  timeout_secs: 600
  cache_max_mb: 1024
```

`max_matches` is the most files (groups) a search keeps, `timeout_secs` the time one search may take, `cache_max_mb` the size of the path-list cache. The values shown are the defaults.

The first search that walks a snapshot writes its list of files and symlinks to `<data_dir>/find-cache/`, encrypted under the unlock key; any later search, with any pattern, reads that list instead of the bucket. The list goes away with the snapshot, and the least recently used lists are dropped once together they pass `cache_max_mb`. The cache only helps while the working set fits the cap: the snapshots in the search scope, times the files in each. A search over more than fits evicts its own lists as it walks and gains almost nothing from the cache.

To size it, count about 40 bytes per file in a snapshot after compression (a record is roughly 100 bytes before: a path of about 80 bytes, the size, the modification time and a 16-byte content id). So `cache_max_mb` should be at least the number of snapshots in scope times the files in one snapshot times 40 bytes; the default 1 GiB holds about 25 million records, for example 100 snapshots of 250 000 files. The figure is an estimate; the real size depends on how well the paths compress. For a search across the whole fleet without filters, raise `cache_max_mb` or narrow the search by host and period.

## Retention preview

`POST /api/v1/policies/{name}/retention-preview` shows what retention rules would forget before they are saved. It needs a session or a token with `policies:read`.

The body holds the rules and, optionally, a time zone: `{"retention": {"keepLast": 3, "keepDaily": 7}, "timezone": "Europe/Kyiv"}`. The policy's other settings stay as saved. The rules are planned over the snapshots the policy already made with the same function that applies retention after a backup, so the preview and the real run agree.

The answer is `{"total", "forget", "floor", "suspicious", "truncated", "hosts"}`. Each host entry has `host_id`, `hostname`, `total`, `forget` and the `snapshots` that would be forgotten (`id`, `time`, `received_at`), newest first. At most 200 snapshots are listed over the whole answer; the rest is counted in `truncated`. Marked snapshots are counted in `suspicious` and never listed. `floor` counts the snapshots no rule keeps that stay only because the two newest complete snapshots of each host are never forgotten. Invalid rules answer `422` with the first problem, an unknown policy `404`.

## Jobs

`GET /api/v1/jobs` and `GET /api/v1/jobs/{id}` also report where a job came from and what it did:

- `origin` is `manual`, `schedule` or `catchup`; `missed_runs` is set for a catch-up job.
- `snapshot_suspicious` is true when the job's snapshot is marked.
- `progress.upload_speed_bps` is the live upload speed of a running job.
- `retention` (job detail) is the result of the retention pass after a backup: `state` (`running`, `done`, `skipped`), `forgot`, `kept` and `blocked`, the snapshots the repository refused to delete, each with `reason` and `kept_in_catalog`.
- `limits` (job detail) holds the limits the job ran with (`upload_rate_bps`, `nice`, `ionice_class`, `ionice_level`) and is `null` when there were none.

## Fleet and alerts

These routes take a session or a token with the scope shown in the table above. They are the data of the Overview page. [Alerts and notifications](/docs/guides/alerts/) explains the alerts themselves.

- `GET /api/v1/fleet` returns every host with one state (`failed`, `overdue`, `offline`, `not_protected` or `healthy`), its last successful backup, its worst policy, the last snapshot and whether its alerts are silenced, plus the `totals` per state and the `storage` of all repositories (size in the bucket, logical size, deduplication, when it was counted). `GET /api/v1/fleet/hosts/{id}` returns one host. The last successful backup and the last snapshot are the newest complete snapshot that the server has signed, by the time the server received it, so a snapshot planted with the writer keys neither hides an overdue backup nor shows up here.
- `GET /api/v1/alerts` lists alerts, newest first. `state` is `active` (the default), `resolved` or `all`; `host_id` keeps one host; `limit` is 1 to 500 (50 by default); `before` is the `id` of the last alert of the previous page. Alerts of silenced hosts stay in the list and are marked `silenced`. `total` counts all matches, not only the page. `GET /api/v1/alerts/count` returns `active` and `unsilenced`.
- `PUT /api/v1/hosts/{id}/silence` with `{"duration": "hour" | "day" | "week" | "forever"}` silences a host and replaces an earlier silence; `DELETE` lifts it. A revoked host answers `409`.

## Agent updates

An update replaces the agent binary on a host, and the agent runs as root. Starting one therefore needs its own scope, `agents:update`; `hosts:write` is not enough. Reading the state needs `hosts:read`. The scope lets a token start, pause, resume, cancel and dismiss an update, so it chooses when hosts install a newer agent binary that a trusted key signed, and can hold updates back; it cannot make a host install a binary signed by an unknown key, an older version or one for another architecture.

- `GET /api/v1/agent-updates` returns the signed versions found in the server's agent directory (`target_version` is the highest; `signed` is `false` when there is none), the `settings` of a rollout and every host with its `version`, `arch`, `outdated`, `updatable` and `state`. For an outdated host that cannot be updated, `blocker` says why: `no_self_update` (the agent predates self-update or does not run under systemd; reinstall it once with `install.sh`), `unsigned` (the agent directory has no readable signature) or `no_binary` (no signed binary of the target version for the host's architecture). `state` is `queued`, `waiting_for_job` (a backup is running), `offline_queued`, `updating` or `rolled_back`, with the `reason` for a rollback and the `target_version` of that update, which stays the same when a newer build appears. `rollout` is the running or paused rollout, or one that finished in the last 24 hours and was not dismissed, with `total`, `updated`, `rolled_back`, `rollbacks_since_resume` (the rollbacks that count towards `pause_after`), the counts per wave and the five latest rollbacks. `pause_reason` says why the server paused it, when something other than rollbacks did; a pause by hand has no reason.
- `POST /api/v1/agent-updates` starts an update by hand; nothing updates on its own. The body is either `{"hosts": ["<id>", ...]}` or `{"outdated": true}`. It answers `202` with the rollout `id`, the number of `hosts` queued and the number `skipped`. Hosts that are current, revoked, already in an update, without self-update or without a signed binary of the target version for their architecture are skipped. `409` means another update is running or paused; `422` means no signed agent on the server, no host to update, or a body that is neither of the two forms.
- The first `canary` hosts form wave 0, hosts connected at that moment first; the others follow in blocks of `wave`. No later wave starts before at least one host of wave 0 succeeded. If wave 0 has no host left and none succeeded, the update pauses with `pause_reason`; `resume` then starts the next wave without a canary. The same pause follows when hosts are queued in a later wave and, for `health_timeout_secs` after the start or the last `resume`, wave 0 has no host in progress, no success and no connected host. An update with no later wave does not pause for this; its offline hosts update when they return. After that the server starts the next wave only when no host of the update is still in progress; hosts of an earlier wave that are offline or running a backup do not hold it back and are updated when they come back. It pauses the update when more than `pause_after` hosts have rolled back (since the last `resume`); hosts already in progress finish. It also pauses, with `pause_reason`, when the signed binary of the target for a host's architecture is gone from the agent directory, for example because a newer version replaced it; those hosts stay queued and do not count as rollbacks. Put the binary back and `resume`, or `cancel` and start a new update. A host with a backup or restore running waits for it, and no backup starts on a host while its update is in progress. Set the numbers in `server.yaml`:

```yaml
agent_updates:
  canary: 2
  wave: 10
  pause_after: 1
  health_timeout_secs: 150
```

  All four must be greater than zero, and `health_timeout_secs` at least 120. `health_timeout_secs` is how long the server waits for a switched agent to report in with the new version, and how long a download may make no progress; the agent rolls itself back on its own after 120 seconds.
- `POST /api/v1/agent-updates/{id}/pause` stops new hosts from starting; `resume` continues, forgets the rollbacks so far and clears `pause_reason`; `cancel` drops the hosts still waiting (hosts already sent the update finish it); `dismiss` hides a finished or cancelled update. A wrong state answers `409`, an unknown id `404`.

An agent trusts only the signing key built into it and the keys installed on its host, so an update cannot add a key to an agent or make it run a binary signed with another one. This covers the update path only; see the [threat model](/docs/security/threat-model/#agent-updates).

## Integrity checks

Checks of a repository, see [Integrity checks](/docs/guides/checks/). A token needs `jobs:read` to read, `jobs:run` to start or stop a check and `policies:write` to change the settings.

- `GET /api/v1/repositories/{id}/checks` returns, for each of `structure`, `packs` and `restore`, the settings, the next scheduled runs, the running check and the last finished one, plus the size counted from the bucket, the pack coverage and `blocked` when a check cannot start now.
- `POST /api/v1/repositories/{id}/checks/{level}/run` starts a check and answers `202` with `run_id`. It answers `409` while a cleanup is running or another check is active, and `422` for an unknown level.
- `GET /api/v1/repositories/{id}/checks/runs/{run_id}` returns the state and progress of a run; `POST .../runs/{run_id}/cancel` stops it; `GET .../runs/{run_id}/report` returns the full report with the findings and notes.
- `PUT /api/v1/repositories/{id}/checks/settings` saves the settings of all three levels: `{"structure": {...}, "packs": {...}, "restore": {...}}`, each with `enabled`, `cron` and `timezone`, and the limits that level has (`read_per_run` for `packs`; `files_per_run` and `max_data_per_run` for `restore`). A limit left out keeps its saved value. Invalid fields answer `422` with `errors` listing each field `path` and `message`. `POST .../checks/settings/validate` does the same check without saving and also returns the next run times.

## Notifications and account

These routes work with a signed-in administrator session only; no token can use them, because a channel can send data out of the server and a secret must not be reachable by a script.

- `GET /api/v1/notifications/channels`, `POST` to add one, `GET`, `PUT` and `DELETE` on `/api/v1/notifications/channels/{id}`. A channel has a `type` (`telegram`, `slack`, `email` or `webhook`), a `name`, `enabled`, `alert_types` (all eight when left out), its `settings` and one `secret`: the bot token, the Slack webhook URL, the webhook bearer secret or the SMTP password. The answer never contains the secret, only `secret_set`. On `PUT`, leaving `secret` out or empty keeps the stored one. Channels from `server.yaml` have `source: "config"`, can be read and tested and answer `409` to `PUT` and `DELETE`.
- `POST /api/v1/notifications/channels/test` sends a test message to a saved channel (`channel_id`) or to an unsaved draft (`channel`). A delivery that fails still answers `200`, with `ok: false` and the error of the receiver.
- `GET` and `PUT /api/v1/notifications/thresholds` read and change `offline_after` and `cert_days`. `PUT` may send one of them; the other keeps its value.
- `GET` and `PUT /api/v1/account` read and change the administrator's email, the address email channels use when they name no recipient. `POST /api/v1/account/password` changes the password and signs out the other sessions.

Invalid input answers `422` with `errors`, a list of `{path, message}`.

## Metrics

`GET /metrics` is not part of the OpenAPI document. It answers in the Prometheus text format to a token with `metrics:read` or to a session, and without credentials on the optional `metrics_listen` address. See [Metrics](/docs/reference/metrics/).
