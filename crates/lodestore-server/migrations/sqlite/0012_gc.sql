CREATE TABLE gc_settings (
    repository_id TEXT PRIMARY KEY REFERENCES repositories (id),
    enabled BIGINT NOT NULL,
    cron TEXT NOT NULL,
    timezone TEXT NOT NULL,
    pause_ms BIGINT NOT NULL,
    repack_below_pct BIGINT NOT NULL,
    max_repack_bytes BIGINT NOT NULL,
    last_slot_ms BIGINT,
    probe_json TEXT,
    probe_at BIGINT,
    updated_at BIGINT NOT NULL
);

CREATE TABLE gc_cycles (
    id TEXT PRIMARY KEY,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    state TEXT NOT NULL,
    dry_run BIGINT NOT NULL,
    origin TEXT NOT NULL,
    started_by TEXT NOT NULL,
    settings_json TEXT NOT NULL,
    gc_id TEXT,
    created_at BIGINT NOT NULL,
    started_at BIGINT,
    published_at BIGINT,
    delete_after BIGINT,
    sweep_started BIGINT NOT NULL DEFAULT 0,
    finished_at BIGINT,
    progress_json TEXT,
    resurrected_json TEXT,
    summary_json TEXT,
    report_json TEXT,
    error TEXT,
    error_after_delete BIGINT NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX gc_cycles_active_idx ON gc_cycles (repository_id)
    WHERE state NOT IN ('done', 'failed', 'aborted');

CREATE INDEX gc_cycles_repository_idx ON gc_cycles (repository_id, created_at);

CREATE TABLE gc_events (
    cycle_id TEXT NOT NULL REFERENCES gc_cycles (id) ON DELETE CASCADE,
    seq BIGINT NOT NULL,
    at BIGINT NOT NULL,
    level TEXT NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (cycle_id, seq)
);
