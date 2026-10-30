# Metrics, dashboard and alert rules

The server exposes the state of the fleet in the Prometheus text format. The repository ships a Grafana dashboard and a set of Prometheus alert rules that work on these metrics without changes.

## Getting the metrics

There are two ways, and you can use both.

**On the main port, with a token.** `GET /metrics` on the address of the web interface answers to an API token that has the `metrics:read` scope, or to a signed-in administrator session. Create the token under Settings, API tokens, pick only `metrics:read` and an expiry you can live with, and renew the token before it ends. The token is shown once. Without a token the answer is `401`, with a token that lacks the scope `403`.

**On a separate address, without a token.** Set `metrics_listen` in `server.yaml`, the environment variable `LODESTORE_METRICS_LISTEN` or the flag `--metrics-listen`, for example `127.0.0.1:9464`. The server then serves only `/metrics` there and asks for no credentials. It is off by default.

```yaml
metrics_listen: "127.0.0.1:9464"
```

The separate address has no authentication, so keep it on loopback or on a network that only your monitoring can reach. Metrics name every host, policy and repository you have. In Kubernetes, expose the port only inside the cluster (`metrics.port` of the chart, see [Deploying with Helm](deploy/helm.md#server-configuration-and-notification-channels)) and restrict it with a NetworkPolicy, or use the token on the main port instead. Never publish it through an ingress.

The metrics are computed from the database on every scrape. A scrape every 30 seconds to a minute is plenty; a fleet of 500 hosts does not need a faster rate.

## Scrape configuration

With the token on the main port:

```yaml
scrape_configs:
  - job_name: lodestore
    scheme: https
    metrics_path: /metrics
    scrape_interval: 1m
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/secrets/lodestore-token
    static_configs:
      - targets: ["lodestore.example.com"]
```

Put the token alone in the file and keep the file readable only by Prometheus. Do not write the token into the configuration itself.

With `metrics_listen` on an internal address:

```yaml
scrape_configs:
  - job_name: lodestore
    scrape_interval: 1m
    static_configs:
      - targets: ["lodestore.monitoring.svc:9464"]
```

## Importing the dashboard and the rules

- **Dashboard.** In Grafana 10 or newer open Dashboards, New, Import, upload `deploy/grafana/lodestore.json` and pick your Prometheus data source. The dashboard has the fixed uid `lodestore-fleet`, the variables `host` and `repository`, shows the last 7 days and refreshes every minute. Rows: **Fleet** (hosts by state, storage used, deduplication), **Backups** (age of the last success per host and policy, duration, bytes added), **Repositories** (storage used, results of integrity checks, certificate expiry) and **Jobs** (jobs finished in the last 24 hours, jobs running).
- **Rules.** Add `deploy/prometheus/lodestore-rules.yml` to `rule_files` in the Prometheus configuration, or load it into your rule store, and check it with `promtool check rules deploy/prometheus/lodestore-rules.yml`.

| Alert | Severity | Expression |
|-------|----------|------------|
| `LodestoreBackupFailed` | critical | `lodestore_backup_last_status == 0` |
| `LodestoreNoRecentBackup` | warning | `lodestore_backup_overdue == 1` |
| `LodestoreAgentOffline` | warning | `lodestore_host_agent_up == 0` for 15 minutes |
| `LodestoreAgentUpdateFailed` | critical | `lodestore_host_agent_update_failed == 1` |
| `LodestoreCheckFailed` | critical | `lodestore_check_last_status == 0` |
| `LodestoreCertificateExpiring` | warning | an agent or server certificate with fewer than 14 days left |

They have the same names and titles as the alerts of Lodestore itself, see [Alerts and notifications](alerts.md). The rules and the built-in notifications work on the same facts, so use one or the other for a given person; with both on, a failure is reported twice. The 15 minutes and the 14 days are written into the rules, edit the file to change them. The dead-man limit is not: `lodestore_backup_overdue` already uses the per-policy limit the server works out, including `spec.alerts.maxAge`.

The dashboard and the rules use only the metrics below. A test in the repository scrapes a running server and checks that every metric they name exists.

## The metrics

Every metric is a gauge. A series is absent, not zero, where there is nothing to say yet, for example no success timestamp before the first success.

### Fleet and hosts

| Metric | Labels | Meaning |
|--------|--------|---------|
| `lodestore_build_info` | `version` | Always 1; the server version. |
| `lodestore_hosts` | `state` | Number of hosts in each state: `healthy`, `overdue`, `failed`, `offline`, `not_protected`. All five are always present. |
| `lodestore_host_agent_up` | `host_id`, `host` | 1 while the agent is connected, 0 otherwise. |
| `lodestore_host_agent_info` | `host_id`, `host`, `version` | Always 1; the agent version of each host that is not revoked. |
| `lodestore_host_agent_update_failed` | `host_id`, `host` | 1 while the last agent update of the host rolled back and the host is not on its target version, 0 otherwise. |
| `lodestore_host_agent_last_seen_timestamp_seconds` | `host_id`, `host` | When the agent was last seen, Unix time. |
| `lodestore_host_cert_expiry_timestamp_seconds` | `host_id`, `host` | When the agent certificate expires, Unix time. |
| `lodestore_server_cert_expiry_timestamp_seconds` | `cert` | When the server gRPC certificate expires, Unix time. `cert` is `grpc`. |

### Backups

One series per host and policy, with the labels `host_id`, `host` and `policy`.

| Metric | Meaning |
|--------|---------|
| `lodestore_backup_last_success_timestamp_seconds` | When the last successful backup finished, Unix time. Absent before the first success. |
| `lodestore_backup_last_status` | 1 when the last attempt succeeded, 0 when it failed. Absent before the first attempt. |
| `lodestore_backup_last_duration_seconds` | How long the last successful backup took. |
| `lodestore_backup_last_bytes_added` | Bytes the last successful snapshot added to the repository. |
| `lodestore_backup_last_bytes_total` | Size of the data in the last successful snapshot. |
| `lodestore_backup_overdue` | 1 when the last success is older than the limit, 0 otherwise. |
| `lodestore_backup_overdue_after_seconds` | The limit itself, see [the dead-man rule](alerts.md#no-recent-backup-the-dead-man-rule). Absent for a policy without a limit. |

To alert on the age yourself: `time() - lodestore_backup_last_success_timestamp_seconds`.

### Jobs

| Metric | Labels | Meaning |
|--------|--------|---------|
| `lodestore_jobs` | `kind`, `state` | Number of jobs of each kind in each state, including everything that finished earlier. Use `state="running"` or `state="queued"` for what is going on now. |
| `lodestore_jobs_finished_24h` | `kind`, `state` | Jobs that finished in the last 24 hours, by final state. |

`kind` is `backup`, `restore`, `download`, `gc` or `check`. States are `queued`, `running`, `waiting` (gc), `succeeded`, `warnings`, `failed`, `cancelled`, `interrupted` and `not_started` (download); each kind has only the ones that apply to it, all of them always present with 0. Jobs are counted in these gauges, not exported as counters, so do not apply `rate()` to them.

### Repositories and checks

| Metric | Labels | Meaning |
|--------|--------|---------|
| `lodestore_repository_physical_bytes` | `repository` | Size of the repository in the bucket, as counted by the last structure check. |
| `lodestore_repository_logical_bytes` | `repository` | Size of the data in live snapshots before deduplication. |
| `lodestore_repository_dedup_ratio` | `repository` | Logical size divided by the size in the bucket. |
| `lodestore_repository_counted_timestamp_seconds` | `repository` | When the size was last counted, Unix time. |
| `lodestore_pack_coverage_ratio` | `repository` | Share of the packs a pack-reading check has read in the last 30 days, 0 to 1. |
| `lodestore_check_last_success_timestamp_seconds` | `repository`, `level` | When the last passing check of the level finished. |
| `lodestore_check_last_status` | `repository`, `level` | 1 when the last finished check of the level passed, 0 when it found problems or stopped. |
| `lodestore_check_findings` | `repository`, `level` | Problems the last finished check reported. |
| `lodestore_check_last_duration_seconds` | `repository`, `level` | How long the last finished check ran. |

`level` is `structure`, `packs` or `restore`, see [Integrity checks](checks.md).

### Alerts

| Metric | Labels | Meaning |
|--------|--------|---------|
| `lodestore_alerts_active` | `type`, `silenced` | Alerts firing now, by type (`backup_failed`, `no_recent_backup`, `agent_offline`, `check_failed`, `certificate_expiring`) and whether their host is silenced (`true` or `false`). All combinations are always present. |
