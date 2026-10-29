CREATE TABLE check_settings (
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    level TEXT NOT NULL,
    enabled BIGINT NOT NULL,
    cron TEXT NOT NULL,
    timezone TEXT NOT NULL,
    limits_json TEXT,
    last_slot_ms BIGINT,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (repository_id, level)
);

CREATE TABLE check_runs (
    id TEXT PRIMARY KEY,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    level TEXT NOT NULL,
    origin TEXT NOT NULL,
    started_by TEXT NOT NULL,
    state TEXT NOT NULL,
    outcome TEXT,
    cancel_requested BIGINT NOT NULL DEFAULT 0,
    progress_json TEXT,
    report_json TEXT,
    findings_total BIGINT NOT NULL DEFAULT 0,
    error TEXT,
    created_at BIGINT NOT NULL,
    started_at BIGINT,
    finished_at BIGINT
);

CREATE UNIQUE INDEX check_runs_active_idx ON check_runs (repository_id)
    WHERE state IN ('queued', 'running');

CREATE INDEX check_runs_repository_idx ON check_runs (repository_id, level, created_at);

CREATE TABLE pack_checks (
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    pack_id TEXT NOT NULL,
    first_seen_at BIGINT NOT NULL,
    checked_at BIGINT,
    bad BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (repository_id, pack_id)
);
