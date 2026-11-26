---
title: Cloudflare R2
description: Why a Cloudflare R2 token cannot be limited to writing without deleting, how a bucket lock rule protects the repository instead, and what has not been verified on the live service.
---

The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Guarantees

| | Token with Object Read & Write |
|---|---|
| A host can delete backups | Yes: the token that can write can also delete, and scopes are per bucket, not per prefix |
| A host can overwrite backups | Yes, unless a bucket lock rule covers the prefix |
| What protects older data | A bucket lock rule on the repository prefix |
| When a host is revoked | It keeps the token until you replace it and revoke the old one in Cloudflare |
| Lifecycle rules | The S3 reader runs; a reply it cannot read gives `lifecycle_unverified` |
| Level the check reports | What the check measures. Without a lock the token deletes, so Host can overwrite or delete |
| Tested against | Not verified on the live service. The 409 answer is simulated with a fault proxy |

## In the web interface

1. **Storage.** Choose the **Cloudflare R2** tile.
2. **Connection.** **Account ID**, from which the wizard builds `https://<account id>.r2.cloudflarestorage.com` (the region is `auto`), then **Bucket**, **Key prefix** and the server key: the access key ID and the secret of an API token with Object Read & Write on the bucket.
3. **Agent rights.** The step has no commands and no field for a second key, because an R2 token cannot be limited. It shows the instruction of this page: turn on a bucket lock rule for the repository prefix in Cloudflare. Agents use the same token as the server. Choose **Check**: the Result panel shows the level the storage gives, which is **Host can overwrite or delete** until a lock rule covers the prefix.

There is no **Skip** on this tile, because there is nothing to skip, and no **Limit agent rights** later: on the repository page the panel shows the bucket lock instruction with a **Close** button only.

## Agent rights

R2 cannot give an agent a key without delete. `GET /api/v1/agent-policy?preset=cloudflare-r2&bucket=<bucket>` returns an instruction, not a policy; the mechanism is `instruction`:

1. Create an API token with Object Read & Write limited to the bucket.
2. Turn on a bucket lock rule for the repository prefix, so that stored objects cannot be deleted or replaced.
3. Connect the repository; the check measures the guarantee you actually got.

The endpoint template of the preset is `https://{account_id}.r2.cloudflarestorage.com` with region `auto`. The preset is not blocked or hidden: the check measures the level, and a level of Host can overwrite or delete needs your consent.

## Limits

- A repeat write to an object under a bucket lock rule is answered by R2 with `409` `ObjectLockedByBucketPolicy`. The client then reads the object back and counts the write as present only when the stored bytes equal what it wrote; any other `409` stays an error. This is covered by the test `s3_fault_bucket_lock_conflict_is_present`, which uses a fault proxy. How the real R2 answers a `PutObject` on a locked object has not been tried.
- The check does not see a lock rule that is added or removed later.
- Real Cloudflare R2 has not been tested.

## Rotating agent credentials

Create a new token and send it with `PUT /api/v1/repositories/{id}/agent-key`. Lodestore stops handing out the old token but does not revoke it: revoke it in Cloudflare. `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-key` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. A key equal to the server key is `server_key_shared` and is not rotated by `agent-key`.
