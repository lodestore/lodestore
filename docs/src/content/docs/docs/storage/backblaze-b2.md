---
title: Backblaze B2
description: The application key for Backblaze B2 without deleteFiles, why a key that can write can hide a file, and what has been tested only against MinIO.
---

Backblaze B2 is connected through its S3 API with `provider = b2`. The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Guarantees

| | Application key without `deleteFiles` |
|---|---|
| A host can delete backups | No file version: the key can hide a file, because `writeFiles` allows that, but the version stays |
| A host can overwrite backups | A write creates a new version and the earlier one stays; B2 does not apply `If-None-Match`, so the client writes without it |
| What protects older data | Bucket versioning: the older version stays |
| When a host is revoked | It keeps the application key until you replace it and delete the old one in B2 |
| Lifecycle rules | A rule can turn a hidden file into a deletion, so the level carries `lifecycle_unverified` unless the rules can be read |
| Level the check reports | Overwrite keeps old version at best |
| Tested against | The client with the B2 preset on MinIO. Real Backblaze B2: not yet |

## In the web interface

1. **Storage.** Choose the **Backblaze B2** tile. The wizard sets `provider = b2` for you.
2. **Connection.** **Region** (for example `us-west-004`), from which the wizard builds `https://s3.<region>.backblazeb2.com`, then **Bucket**, **Key prefix** and the server key: an application key ID and the key.
3. **Agent rights.** The step shows the `b2 key create` command of this page with your bucket and prefix. Run it, paste the application key ID and the key it printed, and choose **Check**. The level is **Overwrite keeps old version** at best.

**Skip, use the server key** lets agents use the server's own key; the level then is **Host can overwrite or delete** and the wizard asks you to accept it. On the repository page, **Limit agent rights** opens the same commands in a panel: after your password it checks the new key and switches the agents to it, and you disable the old key at Backblaze.

## Agent rights

`GET /api/v1/agent-policy?preset=backblaze-b2&bucket=<bucket>&prefix=<prefix>` returns a command; the mechanism is `commands`:

```sh
b2 key create --bucket <bucket> --name-prefix <prefix>/ lodestore-agent listBuckets,listFiles,readFiles,writeFiles
```

The capabilities contain no `deleteFiles`. The `--name-prefix` option appears when you pass a prefix. The endpoint template of the preset is `https://s3.{region}.backblazeb2.com`. The answer is marked not verified.

## What the check measures

The check writes with the key and reads the lifecycle rules. A service that does not answer the lifecycle request, as the S3 API of B2 may not, gives `lifecycle_unverified`.

## Limits

- A key that can write can hide an object. The hidden object's version stays, and Lodestore's cleanup does not remove objects that are only hidden; a snapshot hidden by anyone but the server stops cleanup until it is brought back (see [Cleanup](/docs/guides/gc/#sftp-and-backblaze-b2)).
- A B2 lifecycle rule can delete hidden versions. Check the rules of the bucket yourself.
- B2 does not apply `If-None-Match`, so the client writes without it, uses no multipart upload and never relies on a conditional write.
- Real Backblaze B2 has not been tested.

## Rotating agent credentials

Create a new application key and send it with `PUT /api/v1/repositories/{id}/agent-key`. Lodestore stops handing out the old key but does not delete it at Backblaze: delete it in B2 yourself. `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-key` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. A key equal to the server key is `server_key_shared` and is not rotated by `agent-key`.
