CREATE TABLE policies (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    yaml TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    revision BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    host_id TEXT NOT NULL REFERENCES agents (id),
    policy_id TEXT REFERENCES policies (id) ON DELETE SET NULL,
    policy_name TEXT,
    stdin_name TEXT,
    source TEXT NOT NULL,
    state TEXT NOT NULL,
    spec BLOB NOT NULL,
    phase TEXT,
    cancel_requested BIGINT NOT NULL DEFAULT 0,
    queued_at BIGINT NOT NULL,
    assigned_at BIGINT,
    started_at BIGINT,
    finished_at BIGINT,
    snapshot_id TEXT,
    parent_id TEXT,
    incomplete BIGINT NOT NULL DEFAULT 0,
    errors_total BIGINT NOT NULL DEFAULT 0,
    warnings_total BIGINT NOT NULL DEFAULT 0,
    stats_json TEXT,
    progress_json TEXT,
    error_code TEXT,
    error_message TEXT,
    error_detail_json TEXT,
    log_seq BIGINT NOT NULL DEFAULT 0,
    log_lines BIGINT NOT NULL DEFAULT 0,
    logs_purged BIGINT NOT NULL DEFAULT 0,
    caller_uid BIGINT,
    caller_gid BIGINT
);

CREATE UNIQUE INDEX jobs_coalesce_idx ON jobs (host_id, policy_id)
    WHERE state IN ('queued', 'assigned', 'running') AND policy_id IS NOT NULL;

CREATE UNIQUE INDEX jobs_host_active_idx ON jobs (host_id)
    WHERE state IN ('assigned', 'running') AND source = 'manual';

CREATE INDEX jobs_state_idx ON jobs (state, queued_at);
CREATE INDEX jobs_host_idx ON jobs (host_id, queued_at);
CREATE INDEX jobs_policy_idx ON jobs (policy_id, queued_at);

CREATE TABLE job_events (
    job_id TEXT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    seq BIGINT NOT NULL,
    at BIGINT NOT NULL,
    level TEXT NOT NULL,
    kind TEXT NOT NULL,
    path TEXT,
    script TEXT,
    message TEXT NOT NULL,
    PRIMARY KEY (job_id, seq)
);
