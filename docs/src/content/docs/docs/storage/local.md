---
title: Local folder
description: What a local-folder repository guarantees, which is nothing against a host that can write to the folder, and why revoking a host does not change that.
---

A repository in a local folder lives on a path that the server and the agents can all reach. The levels and how they are measured are in [Storage backends and guarantee levels](/docs/reference/storage/).

## Guarantees

| | Folder |
|---|---|
| A host can delete backups | Yes: every host that can write to the folder can also remove its files |
| A host can overwrite backups | Yes |
| What protects older data | Nothing in Lodestore; use the file system (snapshots, permissions) |
| When a host is revoked | Nothing changes in the file system. `GET /api/v1/repositories/{id}/storage-access` answers `outside_lodestore` |
| Lifecycle rules | None |
| Level the check reports | **Host can overwrite or delete**, always (`not_enforceable`) |
| Tested against | A real folder |

## Agent rights

There is no preset and no generator: the path is a folder, and the rights come from the file system. The client creates a file by a hard link and never truncates, so Lodestore itself does not overwrite a stored file; that is not a protection against a host that is taken over.

## Rotating agent credentials

There are none. Revoking a host in Lodestore does not remove its access to the folder; change the permissions of the folder or the mount to do that.
