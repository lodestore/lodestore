# Public demo stand

A real Lodestore server on your own host, three agents in containers and MinIO, backing up synthetic data on a schedule. Visitors open the web interface without a password, read hosts and snapshots, and download an archive of a snapshot. Everything that writes is refused by the server (`LODESTORE_DEMO_MODE=true`).

The stand is deployed by hand. CI has no access to the host. The host name and domain come from `.env`, nothing is baked into the files.

No SLA and no external monitoring: `restart: unless-stopped` and the container health checks are all there is. There is no reset: retention (`keepLast: 6`, `keepDaily: 3` every 30 minutes) and the daily cleanup cycle keep the size flat.

`demo_mode` is a deliberate operator decision. It is not exposed in the Helm chart values or in the quickstart, and it must not be turned on for a server that holds real backups: anonymous visitors can download any snapshot the server can decrypt.

## What runs

| Service | Image | Notes |
| --- | --- | --- |
| `lodestore` | `ghcr.io/lodestore/lodestore:${LODESTORE_VERSION}` (or `LODESTORE_IMAGE`) | Only service with a published port, `127.0.0.1:${DEMO_PORT}`. gRPC (8443) is never published. |
| `minio` | built from `minio/Dockerfile` | Pinned release, `sha256` checked, the same pins as `scripts/pins.env`. Port 9000 is never published, the console is off. |
| `agent-1..3` | built from `agent/Dockerfile` | The agent binary is copied out of the server image, so agents are always the server's version. |
| `seed` | the `minio` image | One-off, profile `seed`. Creates the repository, policy, cleanup schedule and join tokens. |

Isolation: every container runs with `cap_drop: ALL`, `no-new-privileges`, a read-only root file system, memory, CPU and pids limits and rotated logs (`10m` x 3). There are no bind mounts of host directories and no Docker socket. The only host files mounted are two single read-only files from this directory: `server.yaml` (find limits) and `secrets/unlock_key`. `backend` is an `internal` network without a route out; only `lodestore` is also on `edge`. Memory limits add up to 2176 MiB, CPU limits to about 2.3 cores.

## Host requirements

The numbers below are those of the target host (Ubuntu 24.04, x86_64, 4 CPU, 7.6 GB RAM, one ext4 disk, no LVM or ZFS). Change them for another host.

- The Docker daemon must be rootless (or use `userns-remap`). `preflight.sh` refuses to continue otherwise. A compromised container then maps to an unprivileged user, not to root next to the other projects on the host.
- cgroup v2 with `cpu`, `memory` and `pids` delegated to the user, or the limits do nothing.
- The data of the stand lives on a file system of fixed size. Docker named volumes have no quotas, so the file system under the Docker `data-root` is the limit: `DEMO_DISK_LIMIT_GB` (default 15) is the ceiling and `preflight.sh` compares it with the size of the file system under `/data` in the `minio` container. The bucket quota (9 GiB, 60% of the limit, set by `seed.sh`) is an extra, not the main limit.
- A free loopback port. `127.0.0.1:8080` and several others are taken on the target host, so the default is `DEMO_PORT=18080`. Docker publishing bypasses ufw, so the stand only ever publishes on `127.0.0.1`.

## One-time host setup (rootless Docker)

Run this as an administrator on the host. It changes the host, so do it deliberately; nothing in this repository does it for you.

1. A dedicated user for the stand, with linger so its daemon survives logout:

   ```sh
   sudo adduser --disabled-password --gecos "" lodestore-demo
   sudo loginctl enable-linger lodestore-demo
   ```

2. Packages: `uidmap` (provides `newuidmap` and `newgidmap`), `docker-ce-rootless-extras` (provides `rootlesskit` and `dockerd-rootless-setuptool.sh`) and `dbus-user-session`. `/etc/subuid` and `/etc/subgid` need at least 65536 entries for the user: `grep lodestore-demo /etc/subuid /etc/subgid`.

3. AppArmor. Ubuntu 24.04 sets `kernel.apparmor_restrict_unprivileged_userns=1`, so `rootlesskit` needs a profile that allows user namespaces. The `docker-ce-rootless-extras` package ships one for `/usr/bin/rootlesskit`; check `ls /etc/apparmor.d | grep rootlesskit`. If there is none, create one for the binary the daemon will run (path of `which rootlesskit`):

   ```
   abi <abi/4.0>,
   include <tunables/global>

   /usr/bin/rootlesskit flags=(unconfined) {
     userns,
     include if exists <local/usr.bin.rootlesskit>
   }
   ```

   Save it as `/etc/apparmor.d/usr.bin.rootlesskit` and run `sudo systemctl restart apparmor.service`.

4. cgroup delegation, so the limits in `compose.yaml` take effect:

   ```sh
   sudo mkdir -p /etc/systemd/system/user@.service.d
   printf '[Service]\nDelegate=cpu cpuset io memory pids\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
   sudo systemctl daemon-reload
   ```

5. A disk of fixed size for the daemon's `data-root` (volumes, images and logs all count against it). An ext4 image on a loop device works on any file system:

   ```sh
   sudo fallocate -l 15G /var/lib/lodestore-demo.img
   sudo mkfs.ext4 -q -m 0 /var/lib/lodestore-demo.img
   sudo mkdir -p /srv/lodestore-demo-docker
   echo '/var/lib/lodestore-demo.img /srv/lodestore-demo-docker ext4 loop,nofail 0 0' | sudo tee -a /etc/fstab
   sudo mount /srv/lodestore-demo-docker
   sudo chown lodestore-demo:lodestore-demo /srv/lodestore-demo-docker
   ```

   If the host has LVM or a ZFS dataset with a quota, point the `data-root` at a volume or dataset of that size instead. Keep `DEMO_DISK_LIMIT_GB` equal to its size in whole GB, at most.

6. As `lodestore-demo` (a real login, for example `sudo machinectl shell lodestore-demo@`), install and configure the daemon:

   ```sh
   dockerd-rootless-setuptool.sh install
   mkdir -p ~/.config/docker
   printf '{"data-root": "/srv/lodestore-demo-docker"}\n' > ~/.config/docker/daemon.json
   systemctl --user restart docker
   export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/docker.sock
   docker info --format '{{.SecurityOptions}}'
   ```

   The last command must show `rootless`.

## Configure the stand

As `lodestore-demo`, in a working copy of `deploy/demo` at the tag you deploy:

```sh
cp env.example .env
chmod 600 .env
```

Fill `.env`. Every `CHANGE_ME` must be replaced, `preflight.sh` refuses otherwise. The demo secrets are separate: do not reuse a password from anywhere else, and do not reuse one value for two of them. Generate them, for example `openssl rand -hex 24`.

| Variable | Meaning |
| --- | --- |
| `LODESTORE_VERSION` | Release tag of the image. |
| `LODESTORE_IMAGE` | Optional full image reference that replaces the registry image. Leave empty normally. |
| `LODESTORE_DEMO_MODE` | `false` while seeding and during maintenance, `true` when serving visitors. |
| `DEMO_PUBLIC_URL` | The public https address. The server checks the `Origin` of writes against it. |
| `DEMO_PORT` | Loopback port of the server. Default `18080`. |
| `DEMO_EDGE_SUBNET`, `DEMO_TRUSTED_PROXIES` | The subnet of the `edge` network and the proxy addresses the server trusts for `X-Forwarded-For`. The proxy reaches the container from the gateway of that subnet. If the sign-in limiter sees the proxy and not the visitor, `smoke.sh` fails. |
| `DEMO_DISK_LIMIT_GB` | Ceiling for the file system under the data. Default `15`. |
| `DEMO_ADMIN_USERNAME`, `DEMO_ADMIN_PASSWORD` | The administrator that owns the stand. Nobody signs in with it in demo mode. Its address is the synthetic `admin@example.com`. |
| `DEMO_REPO_PASSWORD` | Password of the backup repository. |
| `MINIO_ROOT_USER`, `MINIO_ROOT_PASSWORD` | MinIO root credentials. |

The unlock key of the server is a file, so that the web interface does not show the "unlock key stored next to the database" warning next to the demo banner:

```sh
mkdir -m 0700 secrets
openssl rand -hex 32 > secrets/unlock_key
chmod 0444 secrets/unlock_key
```

`secrets/` is ignored by git, as is `.env`. Keep a copy of the key somewhere safe: without it the stored repository credentials cannot be read.

## First start

Order matters. The server must run with `LODESTORE_DEMO_MODE=false` while seeding, because the demo refuses every write the seed needs. Keep the proxy closed for this whole sequence (see "Maintenance window").

```sh
docker compose build
./preflight.sh
docker compose up -d lodestore minio
docker compose --profile seed run --rm seed
docker compose up -d
docker compose --profile seed run --rm seed first-backup
```

`seed` creates the bucket, the write-only agent user, the repository, the policy `demo` (`policies/demo.yaml`), the daily cleanup schedule and three one-hour join tokens in the `seed` volume. The agents start with `up -d`, read their token and delete it. `seed first-backup` waits for three online hosts and runs the policy once, so visitors see snapshots at once.

Then switch to serving visitors: set `LODESTORE_DEMO_MODE=true` in `.env` and run `docker compose up -d`. The server ends every session when the mode changes. Check it:

```sh
./smoke.sh http://127.0.0.1:18080
```

It ends with `demo ok`. The limiter check needs the direct loopback port; it is skipped for an `https://` URL because the proxy rewrites the client address. From another machine, `./smoke.sh https://demo.example.com --external demo.example.com` runs the rest through the proxy and also opens a TCP connection to 8443 and 9000, which must fail. That check needs `timeout` from coreutils.

`smoke.sh` checks: `demo_mode` is true, the password-less session works, at least two hosts are online, snapshots exist, a write answers 403 `demo_mode`, no policy has hooks, an archive downloads and is a readable tar.gz, `/metrics` serves nothing, and the sign-in limiter tells visitors apart.

## Reverse proxy

`proxy/nginx.conf` is a site file for the layout that includes `/etc/nginx/sites-enabled/*.conf` inside the `http` block. It declares its own `limit_req_zone` and `limit_conn_zone` with a `lodestore_demo_` prefix, so it does not clash with zones of other sites. It sets rate limits per IP for sign-in, downloads and the agent binary route, a body limit of 64 KiB and a download timeout above the server's own 120 second deadline, and it sends the visitor address in `X-Forwarded-For` (overwritten, never appended to what the visitor sent). Connections are limited per IP in two zones: 64 for the site, because over HTTP/2 every parallel request of a page counts as one, and 2 for archive downloads. It works with the nginx 1.24 of Ubuntu 24.04: `listen ... http2` is the form that version accepts, newer versions accept it with a deprecation warning.

Replace `demo.example.com` and the certificate paths, make sure the upstream port equals `DEMO_PORT`, then:

```sh
sudo cp proxy/nginx.conf /etc/nginx/sites-enabled/lodestore-demo.conf
sudo nginx -t && sudo systemctl reload nginx
```

For another proxy keep the same limits: per-IP rate limits on `/api/v1/auth/demo` and `/api/v1/downloads`, a body limit, download timeouts and a trusted `X-Forwarded-For`. Never route 8443 or 9000 anywhere.

## Maintenance window

Anything that restarts the server with `LODESTORE_DEMO_MODE=false` (seeding, repairs through the API) is a maintenance window. Close the proxy before the restart and open it only after the stack is back with `true`:

```sh
sudo rm /etc/nginx/sites-enabled/lodestore-demo.conf && sudo systemctl reload nginx
# ... maintenance ...
sudo cp proxy/nginx.conf /etc/nginx/sites-enabled/lodestore-demo.conf && sudo systemctl reload nginx
```

The server drops every session when the mode changes, but a visitor who still reaches the server while it runs with `false` would face a normal administrator interface for as long as the window lasts.

To reach the interface during the window, use an SSH tunnel to the loopback port and sign in as the administrator.

## Update

Everything in the stack updates together. Update the working copy of `deploy/demo` to the tag of the new version, set `LODESTORE_VERSION` in `.env`, close the proxy (the same rule applies), then:

```sh
docker compose pull && docker compose up -d --build
```

The order is required: `pull` downloads the server image, and `--build` rebuilds the agent images from it, so the agents get the new binary with the same command. It also rebuilds `minio` from the pinned release. The demo mode does not need to be switched off for this; open the proxy again after `./smoke.sh` prints `demo ok`.

## Logs and disk

Container logs are rotated (`10m` x 3 per container) and count against the same file system. `docker system df` shows the use. The size of the repository is held by retention and by the daily cleanup cycle (04:00 UTC, pause 24 hours).

## Local trial

On a development machine with no other Docker projects, `ALLOW_UNSAFE=1 ./preflight.sh` skips the rootless check and the disk-size check (it still prints both numbers). Use a build of the server image for your architecture in `LODESTORE_IMAGE` and `DOCKER_DEFAULT_PLATFORM` to match. Remove everything with `docker compose down -v` when done.
