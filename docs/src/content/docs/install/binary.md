---
title: Installing from release binaries
description: Download the Lodestore server, agent and lode binaries from GitHub Releases, check their signatures and run the server under systemd.
---

Every release publishes static Linux binaries on GitHub Releases. Use them when you cannot run Docker or Kubernetes. The main path is still [Docker](/lodestore/install/docker/).

## What a release contains

Each release has three programs for two architectures, `x86_64` and `aarch64`:

| File | What it is |
| --- | --- |
| `lodestore-server-linux-<arch>` | The server with the web interface |
| `lodestore-agent-linux-<arch>` | The agent that runs on each backed-up host |
| `lode-linux-<arch>` | The `lode` command line client |

Next to every file is its signature, `<file>.minisig`. `SHA256SUMS` lists the checksums of all six binaries and has its own signature, `SHA256SUMS.minisig`.

## Download

```sh
export VERSION=0.1.0
export ARCH=x86_64
base=https://github.com/lodestore/lodestore/releases/download/v$VERSION
for f in lodestore-server-linux-$ARCH lodestore-server-linux-$ARCH.minisig \
         lode-linux-$ARCH lode-linux-$ARCH.minisig SHA256SUMS SHA256SUMS.minisig; do
  curl --proto '=https' --tlsv1.2 -fsSLO "$base/$f"
done
```

Download `lodestore-agent-linux-$ARCH` and its `.minisig` the same way if the server should hand the agent to new hosts (see below).

## Check the files

Two checks, and they answer different questions. The checksum shows the download is complete. The signature shows the files were built by the project.

```sh
sha256sum -c --ignore-missing SHA256SUMS
```

Every file you downloaded must report `OK`.

The signatures are made with [minisign](https://jedisct1.github.io/minisign/). The public key is committed to the repository as `crates/lodestore-agent/keys/project.pub`; its key id is `8CA38864EEA52F78`. Get it from the tag of your release, and compare it with the key printed here:

```sh
curl --proto '=https' --tlsv1.2 -fsSLO \
  https://raw.githubusercontent.com/lodestore/lodestore/v$VERSION/crates/lodestore-agent/keys/project.pub
cat project.pub
```

```
untrusted comment: minisign public key 8CA38864EEA52F78
RWR4L6XuZIijjGDS/pggnqvsOKeXTnAUspdDGzyF983eIpcnS+mKEREI
```

Then verify the signatures:

```sh
minisign -Vm SHA256SUMS -p project.pub
minisign -Vm lodestore-server-linux-$ARCH -p project.pub
minisign -Vm lode-linux-$ARCH -p project.pub
```

Each command prints `Signature and comment signature verified` and, as its last line, `Trusted comment:` with the signed text. For a binary that text names the product, the version and the architecture, for example `Trusted comment: lodestore-server 0.1.0 x86_64`. For `SHA256SUMS` it is `Trusted comment: lodestore release 0.1.0`. Check that it is the one you asked for. Do not run a file that fails.

## Install

```sh
sudo install -m 0755 lodestore-server-linux-$ARCH /usr/local/bin/lodestore-server
sudo install -m 0755 lode-linux-$ARCH /usr/local/bin/lode
lodestore-server --version
lode --version
```

## Run the server under systemd

Create a user, the unlock key and the settings file. The unlock key seals the secrets the server keeps in its database; see [The unlock key](/lodestore/install/docker/#the-unlock-key) and keep a copy of it outside the server.

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin lodestore
sudo install -d -m 0755 /etc/lodestore
openssl rand -hex 32 | sudo tee /etc/lodestore/unlock.key >/dev/null
sudo chown root:lodestore /etc/lodestore/unlock.key
sudo chmod 0440 /etc/lodestore/unlock.key
```

`/etc/lodestore/server.env`:

```sh
LODESTORE_DATA_DIR=/var/lib/lodestore-server
LODESTORE_UNLOCK_KEY_FILE=/etc/lodestore/unlock.key
LODESTORE_PUBLIC_URL=https://lodestore.example.com
LODESTORE_GRPC_PUBLIC_ADDRESS=lodestore.example.com:8443
```

`/etc/systemd/system/lodestore-server.service`:

```ini
[Unit]
Description=Lodestore server
After=network-online.target
Wants=network-online.target

[Service]
User=lodestore
EnvironmentFile=/etc/lodestore/server.env
StateDirectory=lodestore-server
ExecStart=/usr/local/bin/lodestore-server serve
Restart=on-failure
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now lodestore-server
sudo journalctl -u lodestore-server | grep 'setup?token='
```

The server listens on ports 8080 and 8443. `/etc/lodestore/server.yaml` is read when it exists, and every setting also has a `LODESTORE_*` variable; the variables win. HTTPS, the proxy and the gRPC port are the same as for Docker: see [Networking](/lodestore/reference/networking/).

## Agents for new hosts

The install command that the web interface shows downloads the agent from your server. For that, put the agent binaries and their signatures where the server looks, `/usr/share/lodestore/agents`:

```sh
sudo install -d -m 0755 /usr/share/lodestore/agents
sudo install -m 0755 lodestore-agent-linux-$ARCH /usr/share/lodestore/agents/
sudo install -m 0644 lodestore-agent-linux-$ARCH.minisig /usr/share/lodestore/agents/
```

Without them the server answers that the agent for that architecture is not available. In that case run the install command with `--from-github`, which fetches the agent and its `SHA256SUMS` from the release instead. The Docker image already holds both architectures.
