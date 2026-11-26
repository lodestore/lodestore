---
title: Google Cloud Storage
description: The custom role and service account for agents on Google Cloud Storage without delete rights, how the server reads the bucket lifecycle, and what has been tested only on an emulator.
---

The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Guarantees

| | Service account with the Lodestore role in `agent_options` |
|---|---|
| A host can delete backups | No: the role has no `storage.objects.delete` |
| A host can overwrite backups | No: overwriting an object on Google Cloud Storage needs the delete permission, which the role lacks, and every upload starts with `ifGenerationMatch=0` |
| What protects older data | The role |
| When a host is revoked | It keeps the service account key until you replace it and delete the old key in Google Cloud |
| Lifecycle rules | Read by the server when its account has `storage.buckets.get`; a rule that deletes under the repository prefix lowers the level (`lifecycle_deletes`) |
| Level the check reports | Create-only enforced; Host can overwrite or delete without `agent_options` (`shared_credentials`) |
| Tested against | The client on fake-gcs-server. The refusal on the service side is **not tested**, because fake-gcs-server has no IAM. Real Google Cloud Storage: not yet |

## In the web interface

1. **Storage.** Choose the **Google Cloud Storage** tile.
2. **Connection.** **Bucket**, **Folder** and the server's **Service account key**, the key file in base64 (`base64 -w0 key.json`). The wizard has no field for a custom endpoint, so an emulator or a gateway is not connected through it.
3. **Agent rights.** The step shows the `gcloud` commands of this page, and the one command that lets the server's own account read the bucket lifecycle. Run them, paste the new service account's key in base64, and choose **Check**.

**Skip, use the server key** makes the agents use the server's service account key. The level then is **Host can overwrite or delete** (`shared_credentials`), you accept it, and only a new repository with a limited key changes that: **Limit agent rights** is not offered for such a repository. For a repository created with its own agent key, the panel replaces that key after your password and a full check, and you delete the old service account key in Google Cloud.

## Agent rights

`GET /api/v1/agent-policy?preset=google-cloud-storage&bucket=<bucket>` returns `gcloud` commands; the mechanism is `commands`. They create a custom role with `storage.objects.create`, `storage.objects.get` and `storage.objects.list` (no delete, no update), create a service account, bind the role to the bucket, and create a key. The key, base64 encoded (`base64 -w0 agent.json`), is the value of `agent_options.credential`. The answer also carries `server_text`: one command that grants `roles/storage.legacyBucketReader` to the account the server uses (`SERVER_SA_EMAIL`), which the server needs to read the lifecycle. The answer is marked not verified on the live service.

An agent never reads Google credentials from its own host; it signs with the `credential` the server sends it.

## Lifecycle rules

The server reads the lifecycle of the bucket with its own credentials, in the order `token`, `credential`, `credential_path`, `skip_signature` or `allow_anonymous`. It needs the permission `storage.buckets.get` for that (`roles/storage.legacyBucketReader` has it). The result:

- A rule with the action Delete under a prefix that overlaps the repository prefix lowers the level to Host can overwrite or delete (`lifecycle_deletes`). So does a Delete rule without a non-empty `matchesPrefix`, any key or action type the server does not know, and an answer that fills the 64 KiB read limit.
- When the server is refused permanently (no credentials, `401`, `403`, `404`, an answer that is not JSON), the level carries `lifecycle_unverified`.
- When the answer is temporary (`408`, `429`, `5xx`, no answer), the check settles nothing and the level is **Unchecked** until the next check.

fake-gcs-server returns a bucket without a lifecycle, so the reader has been tried against that and against recorded answers, not against the real service.

## Limits

- Without `agent_options`, the agents use the server's credentials, which can delete: the level is at most Host can overwrite or delete with the reason `shared_credentials`. A repository created that way keeps them; only a new repository can have its own.
- Real Google Cloud Storage has not been tested.

## Rotating agent credentials

Create a new key for the service account and send it with `PUT /api/v1/repositories/{id}/agent-options`. The server checks it in full first, keeps the old ones when that fails, and sends the new location to online agents at once. Lodestore stops handing out the old key but does not delete it: delete the old service account key in Google Cloud. `GET /api/v1/repositories/{id}/storage-access` reports `rotate_via: agent-options` and, for hosts revoked before the replacement, `rotated_disable_old_key_at_provider`. A repository on shared credentials answers `new_repository`.
