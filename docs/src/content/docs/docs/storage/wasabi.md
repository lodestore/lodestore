---
title: Wasabi
description: The IAM policy for a Wasabi agent key without delete rights, why versioning or Object Lock is needed, and that nothing here has been verified on the live service.
---

Wasabi is S3-compatible. The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Guarantees

| | Agent key with the Lodestore policy |
|---|---|
| A host can delete backups | No: the policy has no delete right |
| A host can overwrite backups | Yes unless versioning or Object Lock keeps the old version: the policy has no `If-None-Match` condition, because Wasabi does not document one |
| What protects older data | Bucket versioning or Object Lock |
| When a host is revoked | It keeps the agent key until you replace it and disable the old one in Wasabi |
| Lifecycle rules | The S3 reader runs; a rule that deletes objects or versions lowers the level (`lifecycle_deletes`); a reply it cannot read gives `lifecycle_unverified` |
| Level the check reports | The check measures it: Host can overwrite or delete without versioning, Overwrite keeps old version with it |
| Tested against | Not verified on the live service. The client and the reader are tried against MinIO only |

## In the web interface

1. **Storage.** Choose the **Wasabi** tile.
2. **Connection.** **Region**; the wizard builds the endpoint `https://s3.<region>.wasabisys.com` from it and shows it. Then **Bucket**, **Key prefix** and the server key.
3. **Agent rights.** The step shows the IAM policy of this page. Attach it to a new Wasabi user, paste that user's access key ID and secret and choose **Check**. Without bucket versioning or Object Lock the level is **Host can overwrite or delete**.

**Skip, use the server key** lets agents use the server's own key; the level then is **Host can overwrite or delete** and the wizard asks you to accept it. On the repository page, **Limit agent rights** opens the same commands in a panel: after your password it checks the new key and switches the agents to it, and you disable the old key at Wasabi.

## Agent rights

`GET /api/v1/agent-policy?preset=wasabi&bucket=<bucket>&prefix=<prefix>` returns an IAM policy; the mechanism is `policy`. It is the policy of [Amazon S3](/docs/storage/aws-s3/) without the `If-None-Match` statement. The endpoint template of the preset is `https://s3.{region}.wasabisys.com`. The answer is marked not verified.

## Limits

- Turn on versioning or Object Lock; see [S3 access keys and Object Lock](/docs/recipes/s3-iam-object-lock/). The behavior of Wasabi's versioning and locks has not been tried by Lodestore.
- The check does not test the bucket-configuration rights of the key; the policy gives none.

## Rotating agent credentials

Send the new key with `PUT /api/v1/repositories/{id}/agent-key`. Lodestore stops handing out the old key but does not disable it at Wasabi: delete the old access key there. `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-key` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. A key equal to the server key is `server_key_shared` and is not rotated by `agent-key`.
