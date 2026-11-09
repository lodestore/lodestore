---
title: Quickstart
description: Install the server, connect a host and make a first backup in about ten minutes.
---

This page takes you from nothing to a first backup of one Linux host. It should take about ten minutes. A continuous integration job runs the marked commands of this page on a clean machine for every pull request. [How we test data safety](/docs/security/how-we-test/#the-quickstart-check) says what that job does not cover.

## What you need

- A machine with Docker where the server will run. It must be reachable from the host you want to back up on ports 8080 (web interface and API) and 8443 (agents).
- S3-compatible object storage (AWS S3, MinIO, Ceph and others), a bucket in it, and two access keys: a server key with full access and an agent key without delete rights. The setup wizard shows the exact policy for the agent key.
- A Linux host with systemd and root access, to back up. It can be the same machine as the server.

No bucket? See [Using a local or network folder](#using-a-local-or-network-folder).

### Why S3-compatible storage

Only there can a host hold a separate key without delete rights, and only there is Object Lock available. Those two are what keep a compromised host from destroying its own backups.

No delete rights does not mean no overwrite. The wizard tries to replace a throwaway object with the agent key and shows the result. **Write-only verified** means the storage refused. **Can overwrite** means it did not, and on MinIO this is what you will see: whoever steals the key can replace objects with garbage. Turn on versioning or Object Lock on the bucket, so that a replaced object keeps its earlier version. See [Agent credentials](/docs/security/threat-model/#agent-credentials).

Set the address of the machine that runs the server and the release to install. Use an address the backed-up host can reach.

```sh
export LODESTORE_HOST=192.0.2.10
export LODESTORE_VERSION=0.1.0
```

## Start the server

One command starts the server. The image contains the web interface and the agents, and keeps its state in SQLite in the `lodestore-data` volume.

<!-- ci:run-server -->
```sh
docker run -d --name lodestore \
  -p 8080:8080 -p 8443:8443 \
  -v lodestore-data:/var/lib/lodestore \
  -e LODESTORE_PUBLIC_URL="http://$LODESTORE_HOST:8080" \
  -e LODESTORE_GRPC_PUBLIC_ADDRESS="$LODESTORE_HOST:8443" \
  ghcr.io/lodestore/lodestore:$LODESTORE_VERSION
```

On first start the server prints a one-time link that creates the administrator.

<!-- ci:setup-link -->
```sh
docker logs lodestore 2>&1 | grep 'setup?token='
```

Open the link in a browser, choose a user name and a password.

This quickstart uses plain HTTP so that it works on any network. For anything beyond a trial, put the server behind HTTPS.

## Set up the first repository

The wizard opens after you sign in. It has four steps:

1. **Storage.** Choose S3-compatible storage and enter the bucket and, for storage other than AWS, the endpoint.
2. **Access.** Enter the server key and the agent key. Create the agent key first: the wizard shows the policy to attach to it, and checks what the agent key can do: it must be able to write, and it should be unable to delete or overwrite. A key that can do either is accepted with a warning.
3. **Recovery key.** Choose a repository password and save the recovery key. Either one opens the repository even if the server is lost.
4. **First host.** The wizard shows the install command for the first host. You can also skip this step and use **Add host** later.

## Add a host

Open **Hosts** and choose **Add host**. The page shows a command with a one-time join token. Run it as root on the host you want to back up. The installer downloads the agent from your server, checks its checksum and, when `minisign` is installed on the host, its signature, then installs a systemd service and registers the host.

<!-- ci:install-agent -->
```sh
curl -fsSL "http://$LODESTORE_HOST:8080/install.sh" | sudo sh -s -- --insecure --join "$JOIN_TOKEN"
```

The `--insecure` flag is there because this quickstart serves the installer over plain HTTP. Over plain HTTP anyone on the network can get root on this host: the installer, the agent binary and its checksum can all be replaced on the way, and the installer runs as root. With HTTPS in front of the server, the command has no such flag.

### Check the agent by hand

This step is optional. If `minisign` is installed on the host, the installer does the same check itself. Download the agent and its signature, then verify them with the project key:

```sh
curl -fsSL -o lodestore-agent "http://$LODESTORE_HOST:8080/install/agent/$(uname -m)"
curl -fsSL -o lodestore-agent.minisig "http://$LODESTORE_HOST:8080/install/agent/$(uname -m).minisig"
minisign -V -P RWR4L6XuZIijjGDS/pggnqvsOKeXTnAUspdDGzyF983eIpcnS+mKEREI -m lodestore-agent
```

The last line must be `Trusted comment: lodestore-agent <version> <arch>`. Then run the install command above.

This check does not protect the installer script: over plain HTTP the script can be replaced too, and with it the key it carries. Put the server behind HTTPS to close that gap. The key is also published on [Binary](/docs/install/binary/#check-the-files) and in the release tag, key id `8CA38864EEA52F78`; compare it before you trust the one the server shows.

The host appears under **Hosts** as online within seconds.

## Make the first backup

Open **Policies** and create a policy that names the repository and the folders to back up, for example `/etc` and `/home`. Choose the host by label, then run the policy. Progress shows under **Jobs**. When the job ends as succeeded, the snapshot is in your bucket.

## Restore

Open **Restore**, choose the snapshot and the files, choose the host and a folder to restore into, and start the restore. The agent restores only into the folders its `agent.yaml` allows, by default below `/var/lib/lodestore/restore`, and the form suggests one of them. To restore over the original files, enable it on the host: see [Restoring on the host](/docs/install/binary/#restoring-on-the-host).

## Restore without the server

The server only manages and catalogs. Your data is in the bucket, encrypted with the repository password, and `lode` restores it with nothing else. This is the promise the product is built around, so try it once before you rely on it.

Get `lode` from the [release binaries](/docs/install/binary/). Then set the storage address and the bucket, and put the repository password in a file. A recovery key works in place of the password.

```sh
export S3_ENDPOINT=https://s3.us-east-1.amazonaws.com
export S3_BUCKET=my-backups
umask 077
printf '%s' 'your-repository-password' > ./repository-password
```

For MinIO or another S3-compatible store, `S3_ENDPOINT` is its address, for example `http://192.0.2.20:9000`.

<!-- ci:restore-without-server -->
```sh
export AWS_ACCESS_KEY_ID=your-server-access-key
export AWS_SECRET_ACCESS_KEY=your-server-secret-key
export AWS_REGION=us-east-1
export LODE_REPOSITORY="s3:$S3_ENDPOINT/$S3_BUCKET"
export LODE_PASSWORD_FILE=./repository-password
lode restore latest --target ./restored
```

On AWS, set `AWS_REGION` to the region of the bucket, or the request is refused; `us-east-1` suits most other S3-compatible stores. Files land under the target folder at their original absolute path. `latest` is the newest snapshot the server signed, ordered by the time the storage wrote it, not by the time written in the snapshot, so a snapshot that a compromised host planted with a date in the future is not chosen. If `lode` warns that the repository has no server signatures yet, no Lodestore 0.2 server has opened it; see [Restoring without the server](/docs/recipes/restore-without-server/).

## Using a local or network folder

A repository can live in a folder: a local disk, or a network share such as NFS mounted at the same path on the server and on every agent. The file system must support hard links.

:::caution
In a folder there is no separate agent key: the agent can delete and overwrite backups, and a compromised host can destroy its own. There is no Object Lock either. Prefer S3-compatible storage for data a compromised host must not be able to destroy.
:::

The repository has to be a folder that both the server and the agent can see, at the same path. The agent runs as root and creates files only their owner can read, so the server container has to run as root too. Start it with this command instead of the one above:

```sh
sudo install -d -m 0700 /srv/lodestore-repo
docker run -d --name lodestore --user 0:0 \
  -p 8080:8080 -p 8443:8443 \
  -v lodestore-data:/var/lib/lodestore \
  -v /srv/lodestore-repo:/srv/lodestore-repo \
  -e LODESTORE_PUBLIC_URL="http://$LODESTORE_HOST:8080" \
  -e LODESTORE_GRPC_PUBLIC_ADDRESS="$LODESTORE_HOST:8443" \
  ghcr.io/lodestore/lodestore:$LODESTORE_VERSION
```

Choose **Local folder** in the first step of the wizard and enter `/srv/lodestore-repo`. The continuous integration job does not run this section.

## Next steps

Schedules and retention, alerts, garbage collection and the command line are described in the guides.
