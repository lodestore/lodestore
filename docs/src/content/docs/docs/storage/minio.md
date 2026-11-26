---
title: MinIO
description: The mc commands that create an agent user without delete rights on MinIO, why the key can still overwrite without versioning, and what has been tested against the pinned MinIO.
---

MinIO is the S3-compatible storage Lodestore is tested against. This page covers the agent user. The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Guarantees

| | Agent user with the Lodestore policy |
|---|---|
| A host can delete backups | No: the policy has no delete right |
| A host can overwrite backups | Yes without versioning: MinIO rejects the `If-None-Match` condition key, so the policy cannot refuse a write to an existing name. With versioning the old version is kept |
| What protects older data | Bucket versioning or Object Lock |
| When a host is revoked | It keeps the agent user until you replace the key and remove the old user in MinIO |
| Lifecycle rules | Read by the server; a rule that deletes objects or versions lowers the level (`lifecycle_deletes`) |
| Level the check reports | Host can overwrite or delete without versioning; Overwrite keeps old version on a versioned bucket |
| Tested against | The pinned MinIO release, including Object Lock |

## In the web interface

1. **Storage.** Choose the **MinIO** tile.
2. **Connection.** **Endpoint** (the URL of your MinIO), **Region** if you use one, **Bucket** and **Key prefix**; path-style addressing is on, as MinIO needs. Then the server key: the access key and the secret of a user with full access to the bucket.
3. **Agent rights.** The step shows the `mc` commands of this page. Replace `ALIAS`, `AGENT_ACCESS_KEY` and `AGENT_SECRET_KEY`, run them, paste the key you made and choose **Check**. The Result panel has the Bucket group; without versioning or Object Lock the level is **Host can overwrite or delete**, and the note that MinIO rejects `If-None-Match` stays visible until the conditional-write check has passed.

**Skip, use the server key** lets agents use the server's own key; the level then is **Host can overwrite or delete** and the wizard asks you to accept it. On the repository page, **Limit agent rights** opens the same commands in a panel: after your password it checks the new key and switches the agents to it, and you disable the old key at MinIO (`mc admin user remove`).

## Agent rights

`GET /api/v1/agent-policy?preset=minio&bucket=<bucket>&prefix=<prefix>` returns `mc` commands; the mechanism is `commands`. They write the policy to a file, create it with `mc admin policy create`, create a user with `mc admin user add` and attach the policy with `mc admin policy attach`. Replace `ALIAS`, `AGENT_ACCESS_KEY` and `AGENT_SECRET_KEY` with your own values. The policy is the Amazon S3 policy without the `If-None-Match` statement; see [Amazon S3](/docs/storage/aws-s3/) for what it allows.

This preset is marked verified: the commands and the levels below are run against the pinned MinIO.

## What the check measures

The check writes, repeats and deletes small objects with the agent user, reads the lifecycle rules and, on a versioned bucket, the earlier versions. The tests that pin the two outcomes are `probe_s3_minio_agent_can_overwrite` (without versioning the key can overwrite) and `probe_s3_versioned_write_only_keeps_old_version` (with versioning the old version stays). `wizard_s3_versioned_bucket_needs_no_consent` covers the wizard: a versioned bucket needs no consent, an unversioned one does.

## Limits

- On the repository page, an S3 endpoint that is not Amazon S3 and matches no provider template is treated as MinIO, so the **Limit agent rights** panel shows `mc` commands. For a Ceph gateway or another S3-compatible storage, use the policy of [Amazon S3](/docs/storage/aws-s3/) with the tools of that storage.
- Without versioning or Object Lock, a host that is taken over can replace its backups. Turn one of them on; see [S3 access keys and Object Lock](/docs/recipes/s3-iam-object-lock/).
- The check does not test the bucket-configuration rights of the key; the policy gives none.

## Rotating agent credentials

Create a new user with the commands above, then send the new key with `PUT /api/v1/repositories/{id}/agent-key`. The server checks it with a full probe and sends it to the agents. Lodestore stops handing out the old key but does not remove the old user: delete it in MinIO (`mc admin user remove`). `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-key` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. When the agent key equals the server key, the answer is `server_key_shared`, and `agent-key` does not rotate it.
