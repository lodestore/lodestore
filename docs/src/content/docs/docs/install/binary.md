---
title: Installing from release binaries
description: Download the Lodestore server, agent and lode binaries from GitHub Releases, check their signatures and run the server under systemd.
---

Every release publishes static Linux binaries on GitHub Releases. Use them when you cannot run Docker or Kubernetes. The main path is still [Docker](/docs/install/docker/).

## The short way

```sh
export VERSION=0.2.0
export ARCH=$(uname -m)
base=https://github.com/lodestore/lodestore/releases/download/v$VERSION
curl --proto '=https' --tlsv1.2 -fsSLO "$base/lodestore-server-linux-$ARCH"
curl --proto '=https' --tlsv1.2 -fsSLO "$base/lodestore-server-linux-$ARCH.minisig"
minisign -Vm lodestore-server-linux-$ARCH -P RWR4L6XuZIijjGDS/pggnqvsOKeXTnAUspdDGzyF983eIpcnS+mKEREI && sudo install -m 0755 lodestore-server-linux-$ARCH /usr/local/bin/lodestore-server
```

Before it installs anything, minisign prints `Trusted comment: lodestore-server <version> <arch>`. If the signature does not match, the `&&` stops the chain and the file is not installed. The sections below explain the checksums, the key from the release tag, the `lode` client and the agent.

## What a release contains

Each release has three programs for two architectures, `x86_64` and `aarch64`:

| File | What it is |
| --- | --- |
| `lodestore-server-linux-<arch>` | The server with the web interface |
| `lodestore-agent-linux-<arch>` | The agent that runs on each backed-up host |
| `lode-linux-<arch>` | The `lode` command line client |

Next to every file is its signature, `<file>.minisig`. `SHA256SUMS` lists the checksums of all six binaries and has its own signature, `SHA256SUMS.minisig`. From 0.2.0 a release also holds the build provenance of the binaries and an SBOM, `lodestore-<version>.provenance.jsonl` and `lodestore-<version>.spdx.json`; neither is listed in `SHA256SUMS`.

## Download

```sh
export VERSION=0.2.0
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

Each command prints `Signature and comment signature verified` and, as its last line, `Trusted comment:` with the signed text. For a binary that text names the product, the version and the architecture, for example `Trusted comment: lodestore-server 0.2.0 x86_64`. For `SHA256SUMS` it is `Trusted comment: lodestore release 0.2.0`. Check that it is the one you asked for. Do not run a file that fails.

## Check the build provenance

The signatures show that the project made the files. The build provenance shows how: it is a record, signed through GitHub, that the release workflow `release.yml` built these exact files from the tag of the release. It is published from 0.2.0 on, as `lodestore-$VERSION.provenance.jsonl` next to the binaries. Download it and check the binaries you downloaded with [gh](https://cli.github.com/); you do not need to sign in to GitHub for this:

```sh
curl --proto '=https' --tlsv1.2 -fsSLO "$base/lodestore-$VERSION.provenance.jsonl"
for f in lodestore-server-linux-$ARCH lode-linux-$ARCH; do
  gh attestation verify "$f" --repo lodestore/lodestore --bundle lodestore-$VERSION.provenance.jsonl \
    --cert-identity https://github.com/lodestore/lodestore/.github/workflows/release.yml@refs/tags/v0.2.0 \
    --cert-oidc-issuer https://token.actions.githubusercontent.com \
    --source-ref refs/tags/v$VERSION
done
```

The release also holds an SBOM, `lodestore-$VERSION.spdx.json`. It lists the dependencies declared in the lock files of the release commit (Rust crates and web interface packages). The same SBOM is attached to the Docker image, see [Check the image](/docs/install/docker/#check-the-image). The provenance and the SBOM are not listed in `SHA256SUMS`.

## Install

```sh
sudo install -m 0755 lodestore-server-linux-$ARCH /usr/local/bin/lodestore-server
sudo install -m 0755 lode-linux-$ARCH /usr/local/bin/lode
lodestore-server --version
lode --version
```

## Run the server under systemd

Create a user, the unlock key and the settings file. The unlock key seals the secrets the server keeps in its database; see [The unlock key](/docs/install/docker/#the-unlock-key) and keep a copy of it outside the server.

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

The server listens on ports 8080 and 8443. `/etc/lodestore/server.yaml` is read when it exists, and every setting also has a `LODESTORE_*` variable; the variables win. HTTPS, the proxy and the gRPC port are the same as for Docker: see [Networking](/docs/reference/networking/).

## Agents for new hosts

The install command that the web interface shows downloads the agent from your server. For that, put the agent binaries and their signatures where the server looks, `/usr/share/lodestore/agents`:

```sh
sudo install -d -m 0755 /usr/share/lodestore/agents
sudo install -m 0755 lodestore-agent-linux-$ARCH /usr/share/lodestore/agents/
sudo install -m 0644 lodestore-agent-linux-$ARCH.minisig /usr/share/lodestore/agents/
```

Without them the server answers that the agent for that architecture is not available. In that case run the install command with `--from-github`, which fetches the agent and its `SHA256SUMS` from the release instead. The Docker image already holds both architectures.

On a host where `minisign` is installed, the installer does not accept an agent without a signature that matches the project key or a key given with `--trust-key`, so put both files in the directory. `--from-github` needs `minisign` on the host: it verifies `SHA256SUMS.minisig` with the project key, then the agent against `SHA256SUMS`. Without `minisign` it stops before downloading anything. Where to get it:

| System | How |
| --- | --- |
| Debian 12 or newer, Ubuntu 24.04 or newer | `apt install minisign` |
| Fedora | `dnf install minisign` |
| RHEL, AlmaLinux, Rocky | `dnf install minisign` from EPEL |
| Ubuntu 22.04 | No package. Download a binary from [github.com/jedisct1/minisign/releases](https://github.com/jedisct1/minisign/releases) |

## Restoring on the host

An agent writes restored files only where its own `/etc/lodestore/agent.yaml` allows it. The server cannot change this file, and the agent checks the rule itself for every restore, whoever started it: a token, the web interface, or a server that someone else controls.

```yaml
restore:
  allowed_restore_roots:
    - /var/lib/lodestore/restore
    - /srv/recovery
  allow_in_place: false
```

| Key | Meaning |
|-----|---------|
| `allowed_restore_roots` | Absolute paths, without `..`, `.` or a trailing `/`. The root `/` is not accepted: the agent ignores it with a warning, so a list with only `/` allows nothing. A restore into a new folder must end up below one of them; symlinks below a root are not followed out of it. Default when the key is missing: the one root `restore` in the agent's state directory, `/var/lib/lodestore/restore` unless you changed `--state-dir`. Folders the agent creates for a restore get mode `0700` |
| `allow_in_place` | `true` allows restoring over the original files. Default `false`: the agent refuses with `restore_in_place_disabled` |

A restore outside the roots fails with `restore_outside_roots`. The agent reports its roots and the `allow_in_place` flag to the server, so the web interface offers an allowed folder and the server refuses a wrong request with 400 before it reaches the host.

The file belongs to root and must not be writable by group or others; otherwise the agent ignores it with a warning and uses the defaults.

To allow restores in place when installing a host, add `--allow-in-place-restore` to the install command:

```sh
curl -fsSL https://lodestore.example.com/install.sh | sudo sh -s -- --join <token> --allow-in-place-restore
```

On a host without `agent.yaml` the installer writes it with `restore: { allow_in_place: true }`, mode `0600`, owner root. When `agent.yaml` already exists and does not enable `allow_in_place`, the installer stops with an error and changes nothing; add the key by hand and run it again.
