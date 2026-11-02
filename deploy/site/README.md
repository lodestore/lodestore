# Project site

The static site at `https://lodestore.dev` (landing page, docs, comparisons, blog), built from `docs/` with Astro and served by the host's native nginx from `/opt/lodestore.dev/public`. The directory belongs to the `lodestore-site` user; nginx only reads it.

Unlike the demo stand, which is deployed by hand, the site is deployed by CI. On every push to `main` that touches `docs/**`, the `deploy` job in `.github/workflows/docs.yml` uploads the built site over SSH. The key it uses can only run `rrsync` in write-only mode on the site directory: no shell, no read access, nothing outside `/opt/lodestore.dev/public`.

Deploys are off until the repository variable `SITE_DEPLOY` is `true`, so the workflow can be merged before the host is ready.

| File | Purpose |
| --- | --- |
| `nginx.conf` | Server blocks for `lodestore.dev` and the `www` redirect. Install as `/etc/nginx/sites-enabled/lodestore.dev.conf`. |
| `lodestore-site-headers.conf` | Security headers. Install as `/etc/nginx/snippets/lodestore-site-headers.conf`. |
| `test-config.sh` | Runs both files in an `nginx:1.24-alpine` container against a built `docs/dist` and checks headers, cache, gzip and redirects. |

`<host>` below is the address of the server. It is not written anywhere in the repository: CI reads it from an environment secret.

## One-time host setup

Run as an administrator on the host. Nothing in this repository does it for you.

1. DNS: `A` (and `AAAA`, if the host has IPv6) records for `lodestore.dev` and `www.lodestore.dev` pointing at `<host>`.

2. A user that owns the site directory and receives the uploads. The shell must be `/bin/sh`, not `nologin`: sshd runs the forced command through the login shell.

   ```sh
   sudo adduser --system --group --shell /bin/sh --home /home/lodestore-site lodestore-site
   sudo install -d -o lodestore-site -g lodestore-site -m 755 /opt/lodestore.dev/public
   sudo install -d -o lodestore-site -g lodestore-site -m 700 /home/lodestore-site/.ssh
   ```

   Keep the site directory free of anything that is not part of the build: every deploy deletes the rest.

3. The deploy key. Generate it on your own machine, never on the host, and keep the private half out of chats and logs:

   ```sh
   ssh-keygen -t ed25519 -N '' -C lodestore-site-deploy -f ./lodestore-site-deploy
   ```

   Only the public half goes to the host, as one line in `/home/lodestore-site/.ssh/authorized_keys` (mode 600, owner `lodestore-site`):

   ```
   restrict,command="/usr/bin/rrsync -wo /opt/lodestore.dev/public" ssh-ed25519 AAAA… lodestore-site-deploy
   ```

   `restrict` turns off the pty, port, agent and X11 forwarding. `rrsync -wo` allows rsync to write inside the site directory and nothing else.

4. The certificate. certbot on this host uses the nginx authenticator, so the HTTP block has to be live first. Install the config without the HTTPS blocks, or temporarily with a self-signed certificate, then:

   ```sh
   sudo certbot certonly --nginx -d lodestore.dev -d www.lodestore.dev
   ```

5. nginx:

   ```sh
   sudo install -m 644 deploy/site/lodestore-site-headers.conf /etc/nginx/snippets/lodestore-site-headers.conf
   sudo install -m 644 deploy/site/nginx.conf /etc/nginx/sites-enabled/lodestore.dev.conf
   sudo nginx -t && sudo systemctl reload nginx
   ```

   The file does not mark itself as the default server and does not touch the shared `nginx.conf`.

## GitHub settings

1. The `site` environment, limited to deployments from `main`:

   ```sh
   gh api -X PUT repos/lodestore/lodestore/environments/site \
     -F 'deployment_branch_policy[protected_branches]=false' \
     -F 'deployment_branch_policy[custom_branch_policies]=true'
   gh api -X POST repos/lodestore/lodestore/environments/site/deployment-branch-policies -f name=main
   ```

2. Two environment secrets. The key is read from the file, the address is typed at the prompt, so neither lands in the shell history:

   ```sh
   gh secret set SITE_DEPLOY_KEY --env site < ./lodestore-site-deploy
   gh secret set SITE_HOST --env site
   ```

   Delete the private key file once the secret is set.

3. Repository variables (not environment variables: the job-level `if:` and the build job cannot see those):

   ```sh
   gh variable set SITE_DEPLOY --body true
   gh variable set PUBLIC_LIVE_DEMO_URL --body https://<demo-domain>
   ```

   `SITE_DEPLOY=true` switches deploys on. `PUBLIC_LIVE_DEMO_URL` shows the live demo button and menu item once the public demo answers; leave it unset until then.

## Host key

The host key is pinned in `.github/workflows/docs.yml`, in the `SITE_KNOWN_HOSTS` value of the deploy step, as `lodestore-site ssh-ed25519 AAAA…`. It is a literal under the alias `lodestore-site`, not a repository setting: ssh runs with `-o HostKeyAlias=lodestore-site -o StrictHostKeyChecking=yes -o UserKnownHostsFile=known_hosts`, so it looks the key up by alias, and pointing the `SITE_HOST` secret at another machine fails the host key check instead of sending the site there. Changing the pinned key takes a pull request and a review.

When the host is reinstalled or its key changes, take the new line:

```sh
ssh-keyscan -t ed25519 <host> | awk '{print "lodestore-site", $2, $3}'
```

Before merging, compare the fingerprint of that line with the one printed on the host itself, over a session you already trust:

```sh
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
```

The two `SHA256:…` values must be equal. Repeat the comparison at cutover.

## First deploy and checks

```sh
gh workflow run docs.yml --ref main
```

Then:

```sh
curl -sI https://lodestore.dev/ | grep -iE 'content-security-policy|strict-transport-security|x-content-type-options'
curl -sI "https://lodestore.dev/$(curl -s https://lodestore.dev/docs/quickstart/ | grep -oE '_astro/[^"]+\.css' | head -n 1)" | grep -i cache-control
curl -sI https://www.lodestore.dev/ | grep -iE '^(HTTP|location)'
```

Expect the three security headers, `immutable` on the `_astro` file and a 301 from `www` to `https://lodestore.dev/`. Files are uploaded with `--chmod=D755,F644`; if `rrsync` rejects that option, drop the flag from the workflow and rely on the default umask 022.

## Old address

The old GitHub Pages address stops being rebuilt once the deploy job replaces the Pages job. After `lodestore.dev` answers, publish the redirect stubs for it once:

```sh
gh workflow run pages-redirects.yml --ref main
```

Every old docs page and every comparison gets a page with a canonical link, a meta refresh and a script that keeps the `#anchor`, all pointing at the permanent `https://lodestore.dev/…` address. Check one with an anchor, for example `/lodestore/security/threat-model/#agent-credentials` on the Pages host.

## Switch off and revoke

Stop deploying: `gh variable set SITE_DEPLOY --body false`. The site that is already live stays.

Revoke the key: delete its line from `/home/lodestore-site/.ssh/authorized_keys` on the host, then remove the secret with `gh secret delete SITE_DEPLOY_KEY --env site`. If the key leaked, the worst case is a replaced site, not access to the host.

## Check the config locally

```sh
pnpm --dir docs build
deploy/site/test-config.sh docs/dist
```

The script needs Docker and uses the local ports 18081 and 18443. It prints `site config ok` when `nginx -t` passes and the headers, cache rules, gzip, trailing-slash redirect, 404 page and both redirects behave.
