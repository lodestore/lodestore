# Deploying with Helm

```sh
helm install lodestore deploy/helm/lodestore \
  --set publicUrl=https://lodestore.example.com \
  --set grpc.publicAddress=lodestore.example.com:8443
```

## The unlock key

The server seals repository secrets, the CA key and S3 credentials with an unlock key that is stored in the Secret `<release>-lodestore` under `unlock-key`. If the key is lost or replaced, the database can no longer be opened, so the chart never regenerates it:

- On a real `helm install` the chart creates the key once, if the Secret does not exist yet. `helm upgrade` reuses the existing value.
- The Secret carries `helm.sh/resource-policy: keep`, so `helm uninstall` leaves it in place together with the data volume.
- When Helm cannot read the cluster (`helm template`, `helm install --dry-run`, `helm lint`, ArgoCD and Flux, which render charts without cluster access), the chart cannot tell a new install from an upgrade and stops with an error instead of inventing a key.

For GitOps tools and offline rendering, provide the key yourself in one of two ways:

1. Create the Secret out of band (sealed-secrets, external-secrets, `kubectl create secret generic`) with an `unlock-key` entry of 32 random bytes in hex or base64, and set `existingSecret` to its name. The chart then renders no Secret of its own.
2. Pass the key as a value: `--set-file unlockKey=./unlock.key`, or `valuesFrom` in a Flux `HelmRelease` and a secret value in an ArgoCD `Application`.

Back the key up outside the cluster; without it the repository master secrets stored in the database cannot be opened.

## Other values

- `adminPassword` creates the administrator from the Secret (at least 12 characters); without it the server prints a one-time setup link in its log.
- `externalDatabase.url` or `externalDatabase.existingSecret` selects PostgreSQL instead of the bundled SQLite volume.
- `grpc.publicAddress` must be reachable by agents and is mandatory; see [networking](networking.md) for layer 4 exposure.
