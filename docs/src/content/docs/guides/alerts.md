---
title: "Alerts and notifications"
---
Lodestore watches the fleet and raises an alert when something needs a person. Alerts show on the Overview page and on the host page. Notifications send them out to Telegram, Slack, email or a webhook, so you hear about a failed backup before you need the restore.

## Alert types

| Type | Severity | Fires when | Clears when |
|------|----------|------------|-------------|
| Backup failed | critical | The last attempt of a policy on a host failed. Consecutive failures are one alert and show the latest error. | The policy has a successful backup on that host. |
| No recent backup | warning | A policy has no successful backup on a host within its limit, see [the limit](#no-recent-backup-the-dead-man-rule). | The policy has a successful backup on that host. |
| Agent offline | warning | The agent has not been connected for longer than the offline threshold (default 15 minutes). | The agent connects. |
| Check failed | critical | The last finished [integrity check](/lodestore/guides/checks/) of a repository and level failed or stopped. | The next finished check of that level passes. |
| Certificate expiring | warning, critical once expired | A certificate expires in fewer days than the certificate threshold (default 14): an agent certificate or the server gRPC certificate. | The certificate is renewed or replaced. |
| Agent update failed | critical | The latest finished agent self-update of a host was rolled back and the host still runs another version than the target. The alert shows the reason. | The host runs the target version (a later update succeeded or the agent was reinstalled). A revoked host raises none. |

An alert is one object per cause, not per evaluation: it fires once, may remind, and resolves once. Resolved alerts stay in the history.

The certificate of an agent renews by itself while the agent is online, so an alert about one means the agent has been away for a long time. The built-in server certificate renews itself too. A certificate you supply with `grpc.tls_cert_file` is yours to renew. The web interface has no certificate of its own to watch; put TLS for it in your reverse proxy.

### Thresholds

Two thresholds can be changed under Settings, Notifications, or with `PUT /api/v1/notifications/thresholds`:

| Threshold | Default | Range |
|-----------|---------|-------|
| Agent offline after | `15m` | `5m` and up |
| Certificate expires in | `14` days | 1 to 60 days |

### No recent backup: the dead-man rule

A policy that stays silent is the failure no error message reports, so Lodestore also watches for the absence of success. The limit is worked out for each policy and host:

- If the policy sets `spec.alerts.maxAge`, that is the limit, see [Backup policies](/lodestore/guides/policies/#alerts).
- Otherwise it is twice the longest gap between the next scheduled runs plus the jitter. A daily policy with 15 minutes of jitter gets about 48 hours and 15 minutes.
- A policy with no schedule and no `maxAge` has no limit and never raises this alert.

The age counts from the last successful backup, or, when there is none yet, from the later of the time the host enrolled and the time the policy was created. The same limit decides which hosts the Overview shows as overdue and the value of the metric `lodestore_backup_overdue_after_seconds`.

After the server restarts, an agent that is not yet connected is not reported offline until the offline threshold has passed since the start, so a restart does not send a wave of alerts.

## Reminders and silences

A notification goes out when an alert fires and when it resolves. While it keeps firing, a reminder repeats it every 24 hours. Resolved messages are always sent for the alert types a channel has chosen.

To work on a host without noise, silence its alerts: **Silence alerts** on the host page offers 1 hour, 1 day, 1 week or until you remove it. A silenced host sends no new notifications and no reminders, but its alerts stay visible, marked as silenced. An alert that fires during a silence is announced when the silence ends if it is still firing; if it resolves before that, nothing is sent. A resolved message is sent only for an alert that was announced before, even if the host is silenced by then. Over the API:

```sh
curl -X PUT -H "Authorization: Bearer $LODESTORE_TOKEN" -H 'Content-Type: application/json' \
  -d '{"duration": "day"}' https://lodestore.example.com/api/v1/hosts/$HOST_ID/silence
curl -X DELETE -H "Authorization: Bearer $LODESTORE_TOKEN" \
  https://lodestore.example.com/api/v1/hosts/$HOST_ID/silence
```

The durations are `hour`, `day`, `week` and `forever`. The token needs `alerts:write`.

## Channels

A channel is a place alerts are sent to. Add them under Settings, Notifications, or define them in the server configuration. Every channel has a list of alert types it receives (all six by default) and an on/off switch. There is no routing by host, label or policy: an alert goes to every enabled channel that has its type checked.

| Type | What it needs |
|------|---------------|
| Telegram | Bot token from @BotFather and a chat id (a number, or `@channelname` for a public channel). |
| Slack | An incoming webhook URL. It must start with `https://`. |
| Email | SMTP host, port, security (`starttls` or `tls`), optional user name and password, a `from` address and the recipients. Without recipients the message goes to the administrator's email (Settings, User). Plain SMTP without TLS is not supported. |
| Webhook | A URL and an optional secret. See [the webhook payload](#the-webhook-payload). |

Use **Send test message** on a channel to check it end to end. The result of the last delivery is shown in the channel list. Secrets entered in the web interface are encrypted in the server database and are never shown again; leaving a secret field empty on edit keeps the saved one.

### Channels in server.yaml

Channels can live in `server.yaml` (`/etc/lodestore/server.yaml`, or the file named by `LODESTORE_CONFIG`), which suits Helm and GitOps. They show in the web interface read-only, marked **From server config**; **Send test message** works for them. Change them in the file and restart the server. A channel removed from the file is removed from the list at the next start. A channel made in the web interface cannot use the name of a channel from the file; the server refuses to start on such a clash.

Secrets are never written in this file. Each secret is a path to a file, as with `database_url_file`; a trailing newline is ignored and an empty file is an error.

```yaml
notifications:
  channels:
    - name: ops-telegram
      type: telegram
      bot_token_file: /etc/lodestore/channels/telegram-token
      chat_id: "-1001234567890"
    - name: ops-slack
      type: slack
      webhook_url_file: /etc/lodestore/channels/slack-url
      alerts: [backup_failed, check_failed]
    - name: ops-mail
      type: email
      smtp_host: smtp.example.com
      port: 587
      security: starttls
      username: lodestore
      password_file: /etc/lodestore/channels/smtp-password
      from: lodestore@example.com
      to: [ops@example.com]
    - name: ops-hook
      type: webhook
      url: https://hooks.example.com/lodestore
      secret_file: /etc/lodestore/channels/hook-secret
```

`allow_private_targets` sits next to `channels` and is `false` by default; see [Delivery and retries](#delivery-and-retries).

| Field | Meaning |
|-------|---------|
| `name` | Required, unique. |
| `type` | `telegram`, `slack`, `email` or `webhook`. |
| `enabled` | `true` by default. |
| `alerts` | List of `backup_failed`, `no_recent_backup`, `agent_offline`, `check_failed`, `certificate_expiring`, `agent_update_failed`. All when left out; an empty list is an error. |
| `bot_token_file`, `chat_id` | Telegram. |
| `webhook_url_file` | Slack. |
| `smtp_host`, `port`, `security`, `username`, `password_file`, `from`, `to` | Email. `port` is `587` by default, `security` is `starttls` by default (`tls` for a connection that is encrypted from the start, usually port 465). |
| `url`, `secret_file` | Webhook. `secret_file` is optional and is sent as a Bearer token. |

An unknown field, a missing secret file or an invalid address stops the server at start with a message that names the channel. In Kubernetes, mount the secrets from a Kubernetes Secret and the file from the chart value `config`, see [Deploying with Helm](/lodestore/install/helm/#server-configuration-and-notification-channels).

### The webhook payload

The webhook sends the format of the Alertmanager webhook receiver, version 4, so an Alertmanager-compatible receiver (a chat bridge, an incident tool, a script written for Alertmanager) works without an adapter. Each request is a `POST` with `Content-Type: application/json` and holds one alert. With a secret, the request carries `Authorization: Bearer <secret>`.

```json
{
  "version": "4",
  "groupKey": "lodestore:9c1f6b0a47e2d385",
  "truncatedAlerts": 0,
  "status": "firing",
  "receiver": "ops-hook",
  "groupLabels": { "alertname": "LodestoreBackupFailed" },
  "commonLabels": {
    "alertname": "LodestoreBackupFailed",
    "host": "web-1",
    "host_id": "0192a7c4-52f6-7a3b-8c1d-6a1f3e9b2c10",
    "policy": "etc-and-home",
    "severity": "critical"
  },
  "commonAnnotations": {
    "summary": "Backup failed on web-1",
    "description": "Policy etc-and-home: permission denied: /etc/shadow."
  },
  "externalURL": "https://lodestore.example.com",
  "alerts": [
    {
      "status": "firing",
      "labels": {
        "alertname": "LodestoreBackupFailed",
        "host": "web-1",
        "host_id": "0192a7c4-52f6-7a3b-8c1d-6a1f3e9b2c10",
        "policy": "etc-and-home",
        "severity": "critical"
      },
      "annotations": {
        "summary": "Backup failed on web-1",
        "description": "Policy etc-and-home: permission denied: /etc/shadow."
      },
      "startsAt": "2026-10-19T02:14:07Z",
      "endsAt": "0001-01-01T00:00:00Z",
      "generatorURL": "https://lodestore.example.com/hosts/0192a7c4-52f6-7a3b-8c1d-6a1f3e9b2c10",
      "fingerprint": "9c1f6b0a47e2d385"
    }
  ]
}
```

- `status` is `firing` for a new alert and for a reminder, and `resolved` when the cause is gone. A resolved message has the real `endsAt`.
- `receiver` is the name of the channel. `fingerprint` and `groupKey` stay the same for the whole life of an alert, so a receiver can match a resolved message to the alert it closes.
- `labels.alertname` is `LodestoreBackupFailed`, `LodestoreNoRecentBackup`, `LodestoreAgentOffline`, `LodestoreCheckFailed`, `LodestoreCertificateExpiring` or `LodestoreAgentUpdateFailed`, the same names as in the [Prometheus rules](/lodestore/reference/metrics/#importing-the-dashboard-and-the-rules). `severity` is `critical` or `warning`.
- Other labels depend on the type and are present only when they apply: `host` and `host_id`, `policy`, `repository` and `level` (a check), `cert` (`agent` or `grpc`).
- `annotations.summary` is the title shown in the web interface and `annotations.description` its detail line.
- A test message has `alertname` `LodestoreTest` and severity `info`.

The target is a receiver that accepts this body, not the API of Alertmanager itself, which expects a different request. If you run Prometheus, let Prometheus and Alertmanager raise alerts from the [rules](/lodestore/reference/metrics/#importing-the-dashboard-and-the-rules) instead.

## Delivery and retries

Notifications go through a queue, so a slow or broken channel never delays alert evaluation. A delivery that fails is tried again 1 minute, 5 minutes and 30 minutes later. After the fourth failed attempt it is marked failed and shown with its error in the channel list; the alert itself is not sent again before its next reminder. Sent deliveries are kept for 30 days; resolved alerts with their events, and failed deliveries, for 90 days.

Webhook, Slack and Telegram requests time out after 10 seconds and do not follow redirects. Webhook, Slack, Telegram and email refuse loopback, private, link-local and similar addresses, so a receiver on your own network needs `allow_private_targets: true` under `notifications:` in `server.yaml`; link-local, unspecified and multicast addresses stay refused. The error of a failed webhook or Slack request holds the HTTP status, never the body of the answer. Error messages never contain a secret.

## Without any channel

Alerts always show on the Overview, and the REST API lists them (`GET /api/v1/alerts`, with `state` of `active`, `resolved` or `all`, and `GET /api/v1/alerts/count`). If you run Prometheus, the metric `lodestore_alerts_active` and the ready-made rules in [Metrics](/lodestore/reference/metrics/) give you the same alerts through Alertmanager.
