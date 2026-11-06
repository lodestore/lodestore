---
title: GitOps with Helm and lode apply
description: Keep backup policies in git and apply them from CI, and deploy the Lodestore server from the OCI Helm chart with Argo CD or Flux.
---

This recipe covers two separate parts of a GitOps setup. The first keeps your **backup policies** in git and applies them with `lode apply`. The second deploys the **server** itself from the Helm chart with Argo CD or Flux. You can use either part alone.

## Policies in git

A backup policy is one strict YAML document, the same one the web interface and the API use. Keep them in a folder of a git repository:

```
policies/
  etc-and-home.yaml
  postgres-nightly.yaml
```

`lode apply` makes the server match the files. See [Backup policies](/docs/guides/policies/) for the fields and [the database hook recipe](/docs/recipes/database-backup-hook/) for a complete example.

### A token for CI

Create an API token in the web interface under Settings, API tokens. It is shown once. Give it only these scopes:

| Scope | Why |
| --- | --- |
| `policies:read` | `lode apply` reads the current policies to plan the change |
| `policies:write` | `lode apply` creates, updates and, with `--prune`, deletes them |
| `snapshots:delete` | Only if your files set or change a `retention` block, or make the schedule of a policy with retention run more often. Retention makes the server delete snapshots, so the server refuses the change without it (exit code 13) |
| `hooks:write` | Only if your files add, change or remove `hooks`, or bring more hosts under a policy with hooks. Hooks run commands as root on the hosts, so the server refuses the change without it (exit code 13) |

Store the token as a secret of your CI system, and the server address as an ordinary variable. `lode` reads them from `LODESTORE_SERVER` and `LODESTORE_TOKEN`. There is no flag for the token, so it does not show in the process list. Use an `https://` address; over plain HTTP the token travels unprotected and `lode` warns about it.

### Plan on a pull request, apply on main

`lode apply --dry-run` changes nothing and exits with code 4 when the server differs from the files. A pull request can show the plan, and the main branch can apply:

```sh
code=0
lode apply -f policies/ --prune --dry-run || code=$?
[ "$code" -eq 0 ] || [ "$code" -eq 4 ]
```

```sh
lode apply -f policies/ --prune
```

The first prints the plan and fails only when the command itself fails. The second applies it. The output lists each change, for example:

```
+ policy/etc-and-home    create
~ policy/db-nightly      update
    spec.schedule.cron          "0 2 * * *" -> "30 2 * * *"
Plan: 1 to create, 1 to update, 0 to delete, 4 unchanged.
```

The server checks all files first. When one is invalid, nothing is applied, and the problem is printed as `file.yaml:LINE:COL: message` with exit code 2.

A GitHub Actions workflow that does both. It installs `lode` from the release and checks its checksum; [Binary](/docs/install/binary/) also shows how to check the signature:

```yaml
name: policies
on:
  pull_request:
    paths: ["policies/**"]
  push:
    branches: [main]
    paths: ["policies/**"]
permissions:
  contents: read
env:
  LODESTORE_SERVER: ${{ vars.LODESTORE_SERVER }}
  LODESTORE_TOKEN: ${{ secrets.LODESTORE_TOKEN }}
  LODE_VERSION: "0.1.0"
jobs:
  policies:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false
      - name: Install lode
        run: |
          base=https://github.com/lodestore/lodestore/releases/download/v$LODE_VERSION
          curl --proto '=https' --tlsv1.2 -fsSLO "$base/lode-linux-x86_64"
          curl --proto '=https' --tlsv1.2 -fsSLO "$base/SHA256SUMS"
          sha256sum -c --ignore-missing SHA256SUMS
          sudo install -m 0755 lode-linux-x86_64 /usr/local/bin/lode
      - name: Plan
        if: github.event_name == 'pull_request'
        run: |
          code=0
          lode apply -f policies/ --prune --dry-run || code=$?
          [ "$code" -eq 0 ] || [ "$code" -eq 4 ]
      - name: Apply
        if: github.event_name == 'push'
        run: lode apply -f policies/ --prune
```

### What `--prune` deletes

`--prune` removes the policies of one **manager** that the files no longer contain. The manager is `lode-apply` unless you pass `--manager NAME`. Policies of other managers, and policies made in the web interface, are never deleted. Use a different manager name for every repository that applies policies to the same server.

A policy of the same name that was made in the web interface is an error until you pass `--adopt`. Without a terminal, as in CI, `--prune` is the consent and deletes without asking, so read the plan on the pull request first. `--prune` refuses to run when the files contain no policy at all, so a template that renders to nothing cannot delete everything. See [Command line](/docs/reference/cli/#gitops-with-lode-apply) for all options.

## The server from the OCI chart

Argo CD and Flux render the chart without access to the cluster. The chart creates the [unlock key](/docs/install/helm/#the-unlock-key) only when it can look in the cluster, so under a GitOps tool it stops with an error instead of inventing a key that a later render would replace. You therefore create the key as a Secret first and name it with `existingSecret`.

Create the Secret in the namespace of the release. Use sealed-secrets, external-secrets or whatever your cluster uses to keep secrets out of git. For a first test, `kubectl` is enough:

```sh
kubectl create namespace lodestore
kubectl -n lodestore create secret generic lodestore-secrets \
  --from-literal=unlock-key="$(openssl rand -hex 32)"
```

The Secret needs an `unlock-key` entry of 32 random bytes in hex or base64. Keep a copy of the key outside the cluster: without it the server cannot open its database. With PostgreSQL, add a `database-url` entry and set `externalDatabase.existingSecret` to the same Secret.

The release values, whichever tool you use:

```yaml
existingSecret: lodestore-secrets
publicUrl: https://lodestore.example.com
grpc:
  publicAddress: lodestore.example.com:8443
```

The chart creates the Service `<release>-grpc` of type `LoadBalancer` for the agents. The gRPC port must not sit behind a proxy that terminates TLS; see [Networking](/docs/reference/networking/). The other values, including notification channels and metrics, are in [Helm](/docs/install/helm/).

### Argo CD

For an OCI chart, the repository address starts with `oci://` and ends with the chart name, the path is `.` and the revision is the chart version. Pin the version.

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: lodestore
  namespace: argocd
spec:
  project: default
  source:
    repoURL: oci://ghcr.io/lodestore/charts/lodestore
    targetRevision: 0.1.0
    path: .
    helm:
      valuesObject:
        existingSecret: lodestore-secrets
        publicUrl: https://lodestore.example.com
        grpc:
          publicAddress: lodestore.example.com:8443
  destination:
    server: https://kubernetes.default.svc
    namespace: lodestore
```

### Flux

An `OCIRepository` fetches the chart and a `HelmRelease` installs it:

```yaml
apiVersion: source.toolkit.fluxcd.io/v1
kind: OCIRepository
metadata:
  name: lodestore
  namespace: lodestore
spec:
  interval: 1h
  url: oci://ghcr.io/lodestore/charts/lodestore
  ref:
    tag: 0.1.0
  layerSelector:
    mediaType: application/vnd.cncf.helm.chart.content.v1.tar+gzip
    operation: copy
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: lodestore
  namespace: lodestore
spec:
  interval: 1h
  chartRef:
    kind: OCIRepository
    name: lodestore
  values:
    existingSecret: lodestore-secrets
    publicUrl: https://lodestore.example.com
    grpc:
      publicAddress: lodestore.example.com:8443
```

To upgrade with either tool, change the version in git. The chart and the server image have the same version, so one change moves both. The data volume and your Secret stay as they are.
