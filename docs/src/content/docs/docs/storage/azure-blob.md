---
title: Azure Blob Storage
description: What a host can do to its backups on Azure Blob Storage, how the server signs a short-lived SAS for each agent, what ends a revoked host's access, and what has been tested only on an emulator.
---

An agent writes to the container directly. On Azure the server can hand each agent a short-lived SAS that creates, reads and lists blobs and cannot delete or replace them, so the account key never reaches a host. This page lists what each way of giving the agents access guarantees and where the limits are. The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Guarantees

| | SAS from the server | Your own SAS in `agent_options` | The account key shared with hosts |
|---|---|---|---|
| A host can delete backups | No: the SAS has no delete right | No, if the SAS has only read, create and list rights | Yes |
| A host can overwrite backups | No: the SAS has no write right, and the client uploads with `If-None-Match: *`; Azurite refuses a replacement, real Azure not yet tried | No, with the same rights | Yes |
| What protects older data | The rights of the SAS | The rights of the SAS | Nothing the host cannot undo; use blob versioning or immutability policies on the container |
| When a host is revoked | The server signs no new SAS for it; its access ends when the last SAS issued to it expires | It keeps the SAS until you replace it and disable it at Azure | It keeps the account key; only a new repository and a new account key close it |
| Lifecycle rules | Not read (`lifecycle_unverified`) | Not read (`lifecycle_unverified`) | Not read (`lifecycle_unverified`) |
| Level the check reports | Create-only enforced | Create-only enforced | Host can overwrite or delete (`shared_credentials`) |
| Tested against | Azurite 3.37.0; real Azure: not yet | Azurite 3.37.0; real Azure: not yet | Azurite 3.37.0; real Azure: not yet |

The refusal of `Put Block` and the size limit of a single `Put Blob` were observed on Azurite. They have not been confirmed on the real service, so this page does not rely on them.

## In the web interface

1. **Storage.** Choose the **Azure Blob Storage** tile.
2. **Connection.** **Storage account name**, **Container**, **Folder**, and **Endpoint** only for a non-default address (empty means `https://<account>.blob.core.windows.net`). Choose the server credential: the **Account key** or a **SAS token** with the rights the server needs.
3. **Agent rights.** With the account key the step says that Lodestore signs a short SAS without delete for every agent session and there is nothing to run; there is no **Skip** here, and the container must belong to this repository alone. With a SAS as the server credential, the step asks for a second SAS with read, create and list rights only, for the agents; **Skip, use the server key** hands them the server's SAS instead and needs your acceptance of the weaker level. Choose **Check** to see the level.

On the repository page, **Limit agent rights** replaces your own agent SAS after your password and a full check. A repository that gets its SAS from the server has nothing to replace, and one that skipped the step on shared credentials changes only with a new repository.

## Agent rights

`GET /api/v1/agent-policy?preset=azure-blob` answers with the mechanism `server_issued` and no commands: there is nothing for you to create. The answer is not verified on the live service.

A repository gets a SAS from the server when all of these hold:

- it was created through the wizard endpoint (`POST /api/v1/repositories/wizard`) with the storage account key and `account_name`;
- it has no `agent_options`;
- no other repository of this server uses the same container.

The server signs the SAS locally with the account key; it makes no call to Azure. The SAS is a container SAS with the permissions read, create and list (`rcl`), valid for 24 hours counted from a start time 15 minutes before it was issued, so that a host whose clock runs a little behind can use it. The clock of the server must be correct to within 15 minutes. The SAS policy of the storage account must allow a lifetime of at least one day.

The server sends the SAS when a host enrols and at the start of each session, and signs a new one every hour for every online host. An agent that announces `sas_refresh_v1` reads the new SAS from its location file at most every 5 minutes and uses it in the running job. The server stores the expiry of each SAS it sends before it sends it.

`PUT /api/v1/repositories/{id}/agent-options` answers `409` on a repository that gets a SAS from the server: there is nothing to replace.

## Limits

- **Connected and older repositories get no SAS.** A repository attached with `connect`, and a repository created before this version, keeps its shared credentials: its hosts hold the account key, and the level stays **Host can overwrite or delete** (`shared_credentials`). Only a new repository created through the wizard moves to a SAS from the server. The server never turns it on for an existing repository, because its hosts already hold the key.
- **One container, one repository.** The SAS covers the whole container. A second repository on the same container gets no SAS, and the level of the first one drops.
- **Other repositories on the same account.** An account key is valid for the whole account. When the agents of any other repository of this server on the same account hold the account key, an account SAS, or a SAS with more rights than read, create and list, the level of a repository with a SAS from the server (and of one with your own SAS) drops to **Host can overwrite or delete** with the reason `shared_credentials`. This happens when the server registers such a repository or widens its credentials, at creation, and in the daily check. The server finds the account by its name: the `account_name` option, or the name in an endpoint of the form `<account>.blob.core.windows.net`, `<account>.privatelink.blob.core.windows.net` or `<account>.zNN.blob.storage.azure.net`. When the name cannot be seen (a custom domain or a CDN), the server treats the repository as being on the same account.
- **Server downtime.** A SAS lasts about 23 hours after it was issued. When the server is unreachable for about that long, backups start to fail with a storage refusal and continue after the server is back.
- **Old agents.** An agent without the capability `location_v1` gets the SAS only when it enrols and stops writing after about a day; the server writes a warning to its log. An agent without `sas_refresh_v1` keeps the SAS it started a job with, so a job longer than about 23 hours can fail. Update such agents; the answer of [`GET /api/v1/repositories/{id}/storage-access`](#rotating-agent-credentials) counts them in `hosts_need_agent_update`.
- **A repository the server cannot open** (it has an `unavailable_reason`) gets no new SAS until it opens again.
- **No lifecycle read.** The lifecycle rules of Azure (management policies) can be read only through Azure Resource Manager with Entra credentials, which the server does not use, so the level carries `lifecycle_unverified`. A rule that deletes blobs removes data without any agent involved, so check the rules yourself:

  ```sh
  az storage account management-policy show --account-name <account> --resource-group <group>
  ```

## Your own SAS

A repository without the account key on the server, or one that must not get a SAS from the server, can carry `agent_options.sas_token`: a container SAS with only read, create and list rights. The check measures the level with that SAS, so a SAS that cannot delete or replace gives **Create-only enforced**. The SAS is static: it stops working when it expires, and you replace it with `PUT /api/v1/repositories/{id}/agent-options`. Use a stored access policy or a short lifetime so that you can end its validity.

## Rotating agent credentials

- **SAS from the server.** There is nothing to rotate. After a host is revoked, its access lasts until the last SAS issued to it expires, at most about 24 hours later. `GET /api/v1/repositories/{id}/storage-access` answers with `after_revoke: expires`, the status `access_expires` or `access_expired`, and `access_ends_by`, the time the last issued SAS runs out.
- **Your own SAS.** Send `PUT /api/v1/repositories/{id}/agent-options` with the new SAS. Lodestore stops handing out the old one, but the old SAS works until it expires or you end it at Azure (rotate the account key, or delete the stored access policy it was signed with). Until then `storage-access` reports `rotated_disable_old_key_at_provider`.
- **Shared account key.** A new repository with its own credentials is the only way; then rotate the account key at Azure.

`storage-access` returns the endpoint to use in `rotate_via` where there is one.
