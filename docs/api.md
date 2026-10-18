# REST API

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
| `hosts:read` | `GET /api/v1/hosts`, `GET /api/v1/hosts/{id}` |
| `hosts:write` | `PUT /api/v1/hosts/{id}/labels`, `POST /api/v1/hosts/{id}/revoke` |
| `policies:read` | `GET /api/v1/policies`, `GET /api/v1/policies/{name}`, `POST /api/v1/policies/validate`, `POST /api/v1/policies/{name}/retention-preview`, `GET /api/v1/hosts/{id}/policies` |
| `policies:write` | `POST /api/v1/policies`, `PUT /api/v1/policies/{name}`, `DELETE /api/v1/policies/{name}`, `POST /api/v1/policies/apply` |
| `jobs:read` | `GET /api/v1/jobs`, `GET /api/v1/jobs/{id}`, `GET /api/v1/jobs/{id}/events` |
| `jobs:run` | `POST /api/v1/jobs/{id}/cancel`, `POST /api/v1/jobs/{id}/rerun`, `POST /api/v1/policies/{name}/run`, `POST /api/v1/hosts/{id}/run` |
| `snapshots:read` | `GET /api/v1/hosts/{id}/snapshots`, `GET /api/v1/restore/hosts`, `GET /api/v1/restore/snapshots`, `GET /api/v1/snapshots/{id}`, `GET /api/v1/snapshots/{id}/entries`, `GET /api/v1/snapshots/{id}/search`, `POST /api/v1/snapshots/{id}/summary` |
| `snapshots:delete` | `PUT /api/v1/snapshots/{id}/suspicious`, `DELETE /api/v1/snapshots/{id}` |
| `restore` | `POST /api/v1/restores`, `POST /api/v1/downloads`, `GET /api/v1/downloads/{id}/archive` |

The OpenAPI document lists the scope next to the `bearer` requirement of each of these routes.

**401 and 403.** An unknown, revoked or expired token answers `401` `unauthorized`. A valid token without the scope of the route answers `403` `forbidden` with "The token needs the `<scope>` scope.".

**Retention needs a second scope.** A retention rule decides which snapshots the server deletes after the next backup. Because of that, a token may create a policy with `spec.retention`, or change `spec.retention` of a policy, only if it also has `snapshots:delete`. This applies to `POST /api/v1/policies`, `PUT /api/v1/policies/{name}` and `POST /api/v1/policies/apply`, dry runs included. Otherwise the request answers `403` `forbidden` with "The token needs the snapshots:delete scope to set or change retention." and nothing is saved. A policy without retention, or an edit that leaves retention as it is, needs only `policies:write`. A signed-in administrator session is not affected.

**What a token cannot do.** A token does not manage tokens, the administrator password, join tokens, repositories, the catalog or server settings. Those routes answer `401` to any token.

A restore or download started with a token records `token:<name>` as who started it, the same field that holds the administrator name for a session.

## Snapshot actions

Three requests change or remove a snapshot. Each works with a signed-in administrator session or with an API token that has the `snapshots:delete` scope; a token without it gets `403` with "The token needs the snapshots:delete scope.".

- `PUT /api/v1/snapshots/{id}/suspicious` with `{"suspicious": true, "reason": "..."}` marks the snapshot, and `{"suspicious": false, "reason": "..."}` clears the mark. The reason is required in both directions and holds 1 to 200 characters, otherwise the answer is `422` "Enter a reason.". The response is the updated snapshot: `suspicious` carries `automatic`, the detected `reasons`, the administrator's `note`, `marked_by` and `marked_at`, and is `null` when the snapshot is not marked. A marked snapshot is not counted by retention and is never forgotten automatically; clearing the mark returns it to the normal rules.
- `DELETE /api/v1/snapshots/{id}` removes any snapshot, marked or not, the same way retention forgets one. The answer is `{"id": "...", "storage_kept": false}` when the manifest was deleted from the repository. When the repository refuses the deletion (for example under Object Lock), the snapshot still leaves the catalog and the restore lists, and the answer is `{"storage_kept": true, "reason": "..."}`. A temporary storage failure answers `502` and keeps the snapshot. An unknown id answers `404`.

Snapshot lists (`GET /api/v1/hosts/{id}/snapshots`, `GET /api/v1/restore/snapshots`, `GET /api/v1/snapshots/{id}`) also report `received_at`, `suspicious` and `policy_deleted`, which is true when the snapshot was made by a policy that no longer exists. `GET /api/v1/hosts/{id}/snapshots` accepts a token with `snapshots:read`.

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
