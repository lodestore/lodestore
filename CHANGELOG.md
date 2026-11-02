# Changelog

All notable changes to Lodestore are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-11-02

First release.

### Added

- Backup server in a single binary, shipped as a Docker image and a Helm chart. It keeps its catalog in SQLite by default and in PostgreSQL for production.
- Host agent for Linux on x86_64 and arm64. It registers with a one-time join token, receives its own client certificate, keeps an outbound connection to the server and needs no inbound ports.
- Encrypted, deduplicated repository format v1. Agents write straight to S3-compatible object storage (AWS S3, MinIO, Ceph and others), or to a local or network folder. On S3-compatible storage the agent key has no delete rights; overwrite is refused only where the storage enforces `If-None-Match`, so use versioning or Object Lock elsewhere. In a local folder the agent can delete. Restoring needs only the bucket and a password or recovery key, even if the server is gone.
- Backups of files and directories with exclusions and pre/post hooks that run named scripts from a host-side allowlist.
- Restore from the web interface and the command line: in place, to another host, or as a downloadable archive. Search for a file across the snapshots of all hosts.
- Backup policies written as YAML or edited in a form, with cron schedules in an IANA time zone, jitter and catch-up of missed runs, and retention by daily, weekly, monthly, last-N and within-period rules. `lode apply -f` applies policies from CI, with `--dry-run` and `--prune`.
- Two-phase cleanup that runs only on the server: a dry-run report, marking, a pause longer than the longest backup, a second check, then deletion, with pack rewriting and index compaction. It works with S3 Object Lock and versioning.
- Scheduled integrity checks: repository structure, pack reads and a test restore, with a report in the interface.
- Alerts for failed backups, missing backups, offline hosts, failed checks and expiring certificates, delivered to Telegram, Slack, email and webhooks. Prometheus metrics, alert rules and a Grafana dashboard.
- Web interface with fleet overview, hosts, policies, jobs, restore and settings; the `lode` command line client with API tokens and scopes.
- Signed agent updates. The agent installs an update only after checking its signature against the project key or a key trusted on the host. The server does not verify the signature, it only reads the signature's trusted comment.
- Detection of suspicious snapshots: a snapshot marked by the anomaly detector or an administrator counts toward no retention rule and is never forgotten.
- `demo_mode`, a public read-only demonstration for a throwaway server with synthetic data, never for real backups. A demo stand is in `deploy/demo`.

## [0.1.0-rc.1] - 2026-11-02

First release candidate for 0.1.0. The full list of changes is in the 0.1.0 section of [CHANGELOG.md](https://github.com/lodestore/lodestore/blob/main/CHANGELOG.md).
