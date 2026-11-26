---
title: Amazon S3
description: The agent key policy for Amazon S3 that cannot delete or replace objects, what the storage check measures, and what has been tested only on MinIO.
---

The agents of an S3 repository write with an agent key that is separate from the key of the server. This page covers Amazon S3; the levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Guarantees

| | Agent key with the Lodestore policy |
|---|---|
| A host can delete backups | No: the policy has no delete right |
| A host can overwrite backups | No: the policy denies any `PutObject` without `If-None-Match`, and the client always sends it |
| What protects older data | The policy. Object Lock or versioning add protection against a key that can delete or overwrite |
| When a host is revoked | It keeps the agent key until you replace it and disable the old one in IAM |
| Lifecycle rules | Read by the server; a rule that deletes objects or versions lowers the level (`lifecycle_deletes`), an unreadable one adds `lifecycle_unverified` |
| Level the check reports | Create-only enforced; Host can overwrite or delete when the key can delete or overwrite |
| Tested against | MinIO (pinned). The `Deny` statement of the policy has no test, and real AWS: not yet |

## Agent rights

`GET /api/v1/agent-policy?preset=aws-s3&bucket=<bucket>&prefix=<prefix>` returns the IAM policy as text; the mechanism is `policy`. The policy lets the key add objects under `packs/`, `index/` and `snapshots/`, read `config`, `index/` and `locks/`, and list `index/` and `locks/`. It contains no delete right and no write under `locks/`, and a final `Deny` refuses any `PutObject` without `If-None-Match`. The answer is marked not verified on the live service.

The same policy, the rights of the server key and Object Lock are in [S3 access keys and Object Lock](/docs/recipes/s3-iam-object-lock/).

## What the check measures

The check writes, repeats and deletes small objects with the agent key under `packs/`, `index/` and `snapshots/`, reads the lifecycle rules of the bucket with the server key, and reports the level. A policy that depends on the source of the request is not seen by the check.

## Limits

- The check does not test the bucket-configuration rights of the agent key (`s3:PutLifecycleConfiguration`, `s3:PutBucketVersioning`, `s3:PutBucketPolicy`, `s3:BypassGovernanceRetention`). Give the key only the rights of the policy.
- The reader of the lifecycle rules was tried against MinIO only.
- Real Amazon S3 has not been tested.

## Rotating agent credentials

Replace the key with `PUT /api/v1/repositories/{id}/agent-key`. The server checks the new key with a full probe, then sends it to online agents at once and to the others at their next connection. This stops Lodestore from handing out the old key; it does not disable it at AWS. Delete or deactivate the old access key in IAM yourself.

`GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-key`, and the status `revoked_hosts_hold_access` while revoked hosts hold a key that was not replaced, and `rotated_disable_old_key_at_provider` for hosts revoked before a replacement. Lodestore cannot see whether you disabled the old key at AWS, so that status stays. When the agent key is the same as the server key, the answer is `server_key_shared` and `agent-key` does not rotate it: create a separate agent key first.
