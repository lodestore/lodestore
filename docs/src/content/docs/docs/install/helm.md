---
title: "Deploying with Helm"
description: Install the Lodestore server on Kubernetes from the chart in the OCI registry.
---
Every release publishes the chart to GitHub's container registry, with the same version as the server image. Install it from there:

```sh
helm install lodestore oci://ghcr.io/lodestore/charts/lodestore --version 0.1.0 \
  --set publicUrl=https://lodestore.example.com \
  --set grpc.publicAddress=lodestore.example.com:8443
```

`--version` is required for a pre-release such as `0.2.0-rc.1`: Helm skips pre-releases when it is not given. Without a version it picks the newest stable one. The chart starts the image of the same version, so the chart and the server move together.

To work on the chart itself, install it from a checkout of the repository:

```sh
helm install lodestore deploy/helm/lodestore \
  --set publicUrl=https://lodestore.example.com \
  --set grpc.publicAddress=lodestore.example.com:8443
```

A checkout starts the image that its `appVersion` names. To deploy Lodestore from a GitOps tool, see [GitOps with Helm](/docs/recipes/gitops-helm/).

## The unlock key

The server seals repository secrets, the CA key and S3 credentials with an unlock key that is stored in the Secret `<release>-lodestore` under `unlock-key`. If the key is lost or replaced, the database can no longer be opened, so the chart never regenerates it:

- On a real `helm install` the chart creates the key once, if the Secret does not exist yet. `helm upgrade` reuses the existing value.
- The Secret carries `helm.sh/resource-policy: keep`, so `helm uninstall` leaves it in place together with the data volume.
- When Helm cannot read the cluster (`helm template`, `helm install --dry-run`, `helm lint`, ArgoCD and Flux, which render charts without cluster access), the chart cannot tell a new install from an upgrade and stops with an error instead of inventing a key.

For GitOps tools and offline rendering, provide the key yourself in one of two ways:

1. Create the Secret out of band (sealed-secrets, external-secrets, `kubectl create secret generic`) with an `unlock-key` entry of 32 random bytes in hex or base64, and set `existingSecret` to its name. The chart then renders no Secret of its own.
2. Pass the key as a value: `--set-file unlockKey=./unlock.key`, or `valuesFrom` in a Flux `HelmRelease` and a secret value in an ArgoCD `Application`.

Back the key up outside the cluster; without it the repository master secrets stored in the database cannot be opened.

## Server configuration and notification channels

`config` is the content of `server.yaml`. When it is not empty the chart writes it into a ConfigMap, mounts it at `/etc/lodestore/server.yaml`, where the server looks for it, and puts a checksum of it on the pod so a change restarts the server. Without it the chart renders exactly what it did before.

The main use is [alert channels](/docs/guides/alerts/#channels-in-serveryaml). A channel names its secrets as files, so keep the secrets in a Kubernetes Secret and mount that with `extraVolumes` and `extraVolumeMounts`:

```sh
kubectl create secret generic lodestore-channels \
  --from-file=telegram-token=./telegram-token \
  --from-file=smtp-password=./smtp-password
```

```yaml
config:
  notifications:
    channels:
      - name: ops-telegram
        type: telegram
        bot_token_file: /etc/lodestore/channels/telegram-token
        chat_id: "-1001234567890"
      - name: ops-mail
        type: email
        smtp_host: smtp.example.com
        security: starttls
        username: lodestore
        password_file: /etc/lodestore/channels/smtp-password
        from: lodestore@example.com
        to: [ops@example.com]

extraVolumes:
  - name: channels
    secret:
      secretName: lodestore-channels

extraVolumeMounts:
  - name: channels
    mountPath: /etc/lodestore/channels
    readOnly: true
```

**Never put a secret in `config`.** The ConfigMap is readable by anyone who can read ConfigMaps in the namespace and is shown by `helm get values`. Put only names, addresses and `*_file` paths there; the server rejects a channel that carries a secret inline. A Secret mounted through `extraVolumes` is the place for tokens and passwords. `config` can hold any other `server.yaml` setting too, but the `LODESTORE_*` variables the chart already sets (the gRPC address, the unlock key file, the database) win over the same setting in the file.

A channel defined here appears in the web interface read-only, marked from the server config. Change it by editing `config` and upgrading the release.

## Metrics

`metrics.port` is `0` by default and the server serves no extra port. Set it, for example `metrics.port=9464`, and the chart sets `LODESTORE_METRICS_LISTEN=0.0.0.0:9464`, opens the container port `metrics` and publishes it in its own ClusterIP Service `<release>-lodestore-metrics`. The shared Service `<release>-lodestore` never carries it.

```sh
helm upgrade lodestore deploy/helm/lodestore --reuse-values --set metrics.port=9464
```

That port answers `/metrics` to anyone who can reach it, with no token, so the chart's NetworkPolicy lets in only the sources you list in `networkPolicy.metricsFrom`. With the list empty the port is closed to every pod. Name your Prometheus:

```yaml
networkPolicy:
  metricsFrom:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: monitoring
      podSelector:
        matchLabels:
          app.kubernetes.io/name: prometheus
```

Do not route the port through an ingress. If you would rather not open it, leave `metrics.port` at 0 and scrape the main port with a `metrics:read` token. See [Metrics](/docs/reference/metrics/) for the scrape configuration, the dashboard and the alert rules.

## Pod security defaults

The server pod is closed by default:

- The root filesystem is read-only. The server writes only to the data volume and to `/tmp`, which is a 64Mi `emptyDir`.
- The pod runs as the non-root user 65532, with `seccompProfile: RuntimeDefault`, no privilege escalation and every capability dropped.
- The chart creates its own ServiceAccount and does not mount its token into the pod, because the server never talks to the Kubernetes API. Set `serviceAccount.create=false` to use an existing account (`serviceAccount.name`, or `default` when empty); the token stays unmounted either way.
- `networkPolicy.enabled` (on by default) renders a NetworkPolicy that restricts ingress only: the web and agent ports are open to every source, and the metrics port follows `networkPolicy.metricsFrom` as above. Set it to `false` if your cluster has its own policy.
- Egress stays open, because the server reaches S3, notification channels and the database. To restrict it, set `networkPolicy.egress` to a list of NetworkPolicy egress rules; once it is set, only those destinations are allowed, so include DNS yourself.

## Other values

- `adminPassword` creates the administrator from the Secret (at least 12 characters); without it the server prints a one-time setup link in its log.
- `externalDatabase.url` or `externalDatabase.existingSecret` selects PostgreSQL instead of the bundled SQLite volume.
- `grpc.publicAddress` must be reachable by agents and is mandatory; see [networking](/docs/reference/networking/) for layer 4 exposure.
