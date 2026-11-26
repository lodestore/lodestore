---
title: S3 access keys and Object Lock
description: The write-only policy for the agent key, the rights of the server key, and how to turn on Object Lock so that stolen keys cannot destroy your backups.
---

Lodestore uses two kinds of S3 keys, and the split between them is a security feature. A host that is taken over holds the agent key. That key can add backups and cannot delete them. This recipe shows the policies and then how to add Object Lock, which protects the data even from a key that can delete.

The server writes the same policy, and the commands for other S3 services, on request: `GET /api/v1/agent-policy?preset=<id>` answers for Amazon S3, MinIO, Wasabi, Backblaze B2, Cloudflare R2 and Hetzner Object Storage. The page of each service, such as [Amazon S3](/docs/storage/aws-s3/), [MinIO](/docs/storage/minio/) and [Cloudflare R2](/docs/storage/cloudflare-r2/), says what its key can and cannot do.

:::caution
Object Lock is checked by automated tests against MinIO, including a lock in `GOVERNANCE` and in `COMPLIANCE` mode. It has **not been verified** on real AWS S3 yet. The behavior of S3 locks is documented by AWS and Lodestore follows it, but until it is verified on AWS, test your setup on a throwaway bucket before you rely on it, and do not use `COMPLIANCE` mode on data you cannot afford to keep for the whole retention period.
:::

## The agent key: write-only

Agents and the `lode backup` command need to add objects and to read a few. They never delete. The policy below is the one the setup wizard shows when you add a repository, filled in with your bucket and folder. Copy it from the wizard; this is the same policy for a bucket named `my-bucket` with the repository at its root:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:PutObject"],
      "Resource": [
        "arn:aws:s3:::my-bucket/packs/*",
        "arn:aws:s3:::my-bucket/index/*",
        "arn:aws:s3:::my-bucket/snapshots/*"
      ]
    },
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject"],
      "Resource": [
        "arn:aws:s3:::my-bucket/config",
        "arn:aws:s3:::my-bucket/index/*",
        "arn:aws:s3:::my-bucket/locks/*"
      ]
    },
    {
      "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": ["arn:aws:s3:::my-bucket"],
      "Condition": {
        "StringLike": {
          "s3:prefix": ["index/*", "index/", "locks/*", "locks/"]
        }
      }
    },
    {
      "Effect": "Deny",
      "Action": ["s3:PutObject"],
      "Resource": [
        "arn:aws:s3:::my-bucket/packs/*",
        "arn:aws:s3:::my-bucket/index/*",
        "arn:aws:s3:::my-bucket/snapshots/*"
      ],
      "Condition": {"Null": {"s3:if-none-match": "true"}}
    }
  ]
}
```

What it grants, and what it leaves out:

- It adds objects under `packs/`, `index/` and `snapshots/`, and nothing else is written. No `locks/` write, no `config` write.
- It reads `config`, `index/` and `locks/`. It cannot read a pack or a snapshot, so a stolen agent key does not give anyone your data.
- It has no `s3:DeleteObject`, and no `s3:DeleteObjectVersion`.
- The final `Deny` refuses a write that does not carry `If-None-Match`. Lodestore sends it on every write, so a normal backup is unaffected, and a holder of the key cannot replace an object that exists. S3 would otherwise let `PutObject` overwrite.

If the repository lives in a folder of the bucket, the wizard puts the folder into every resource and prefix.

After you attach the policy, the wizard checks the key and reports one of four levels:

| Level | What a host can do | Confirmation |
| --- | --- | --- |
| Create-only enforced | Add objects. The storage refuses to replace or delete them. | None. |
| Overwrite keeps old version | Replace an object, but the storage keeps the earlier version and the key cannot delete it. | None. The level is shown as it is. |
| Host can overwrite or delete | Replace or delete its own backups. The wizard lists the reasons. | You confirm before the repository is created. |
| Unchecked | The check could not measure it. | You can check again, or confirm and continue. |

You get **Create-only enforced** when the storage refuses the overwrite, which the policy above does on AWS S3, and **Overwrite keeps old version** when the bucket has versioning or Object Lock. A lifecycle rule on the bucket that deletes objects or their earlier versions lowers **Overwrite keeps old version** to **Host can overwrite or delete**. If the lifecycle rules cannot be read, **Create-only enforced** carries a note and **Overwrite keeps old version** drops to **Host can overwrite or delete**. See [the threat model](/docs/security/threat-model/) for what this does and does not protect against.

### MinIO

MinIO rejects the `s3:if-none-match` condition, so the policy it gets is the same without the last statement. The wizard shows these commands for it, with your bucket in the policy:

```sh
mc admin policy create ALIAS lodestore-agent lodestore-agent-policy.json
mc admin user add ALIAS AGENT_ACCESS_KEY AGENT_SECRET_KEY
mc admin policy attach ALIAS lodestore-agent --user AGENT_ACCESS_KEY
```

On MinIO a stolen agent key can therefore still overwrite an object. Versioning or Object Lock on the bucket is what makes that harmless: the older version stays, and the wizard reports **Overwrite keeps old version** without asking you to confirm. Turn versioning on before the wizard:

```sh
mc version enable ALIAS/my-bucket
```

Without it, MinIO ends at **Host can overwrite or delete** and the wizard asks you to confirm.

## The server key

The server deletes data for retention and cleanup, with its own key. Besides what it needs to read and write the repository (`s3:GetObject`, `s3:PutObject`, `s3:ListBucket`), it needs:

- `s3:DeleteObject`
- `s3:DeleteObjectVersion`
- `s3:ListBucketVersions`
- `s3:GetObjectVersion`

and, on a bucket with Object Lock, `s3:GetObjectRetention`, `s3:GetObjectLegalHold` and `s3:GetBucketObjectLockConfiguration`. Without them cleanup still works, but it cannot say when a locked version is released. The setup wizard also needs `s3:GetBucketObjectLockConfiguration` to tell you whether Object Lock is on; without it the check shows a warning and does not fail.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:GetObjectVersion",
        "s3:DeleteObjectVersion",
        "s3:GetObjectRetention",
        "s3:GetObjectLegalHold"
      ],
      "Resource": ["arn:aws:s3:::my-bucket/*"]
    },
    {
      "Effect": "Allow",
      "Action": [
        "s3:ListBucket",
        "s3:ListBucketVersions",
        "s3:GetBucketObjectLockConfiguration"
      ],
      "Resource": ["arn:aws:s3:::my-bucket"]
    }
  ]
}
```

Keep this key off the backed-up hosts. Only the server uses it. [Cleanup](/docs/guides/gc/#server-key) explains what each right is for.

## Turn on Object Lock

In COMPLIANCE mode, Object Lock makes S3 refuse to delete or overwrite an object version until its retention period ends, whoever asks. In GOVERNANCE mode, a user with the `s3:BypassGovernanceRetention` right can still delete it. With either mode, even a stolen server key, or a stolen agent key on MinIO, cannot destroy backups that are still inside the period, as long as the key has no bypass right, which the policies on this page do not grant.

Create the bucket with Object Lock on. This also turns on versioning:

```sh
aws s3api create-bucket --bucket my-bucket --region eu-central-1 \
  --create-bucket-configuration LocationConstraint=eu-central-1 \
  --object-lock-enabled-for-bucket
```

Then set a default retention, so that every new object is locked without the agent having to ask:

```sh
aws s3api put-object-lock-configuration --bucket my-bucket \
  --object-lock-configuration \
  '{"ObjectLockEnabled":"Enabled","Rule":{"DefaultRetention":{"Mode":"GOVERNANCE","Days":30}}}'
```

On MinIO:

```sh
mc mb --with-lock ALIAS/my-bucket
mc retention set --default GOVERNANCE 30d ALIAS/my-bucket
```

### GOVERNANCE or COMPLIANCE

| Mode | Who can delete a locked version before the period ends |
| --- | --- |
| `GOVERNANCE` | Only a user who has the `s3:BypassGovernanceRetention` right and asks for it. |
| `COMPLIANCE` | Nobody, including the account owner. The period cannot be shortened. |

Lodestore never asks the storage to bypass a lock. In both modes a locked version is simply kept, and cleanup reports it as held. The difference is for you: with `GOVERNANCE` you can still free space by hand in an emergency, and with `COMPLIANCE` you cannot. Start with `GOVERNANCE`.

### What changes in daily use

- **Space comes back later.** When retention forgets a snapshot, or cleanup removes packs, the locked versions stay until their period ends. The cleanup report shows them as `Held by Object Lock` with the date they free up. Storage cost goes up by the data you hold for the period. Pick a period that matches how long an attacker could be undetected, not longer.
- **Repacking adds before it removes.** On a locked bucket the old packs stay until their locks end, so usage can rise before it falls.
- **Backups are not affected.** The agent only adds new objects, which the lock allows.
- **Setup check.** The wizard shows **Object Lock** as passed when the bucket reports it enabled. A bucket without a lock, or a storage that does not implement the call, is shown as neutral and does not block anything.

Read [Cleanup](/docs/guides/gc/#object-lock-and-versioning) for the full behavior.
