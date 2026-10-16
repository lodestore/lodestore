CREATE TABLE snapshots (
    id TEXT PRIMARY KEY,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    host_id TEXT REFERENCES agents (id),
    hostname TEXT NOT NULL,
    time_ms BIGINT NOT NULL,
    paths_json TEXT NOT NULL,
    tags_json TEXT NOT NULL,
    policy_name TEXT,
    stdin_name TEXT,
    job_id TEXT,
    parent_id TEXT,
    incomplete BIGINT NOT NULL,
    errors_total BIGINT NOT NULL,
    warnings_total BIGINT NOT NULL,
    stats_json TEXT NOT NULL,
    agent_id_claim TEXT,
    discovered_at BIGINT NOT NULL
);

CREATE INDEX snapshots_host_idx ON snapshots (host_id, time_ms);
CREATE INDEX snapshots_repo_idx ON snapshots (repository_id, time_ms);

CREATE TABLE catalog_state (
    repository_id TEXT PRIMARY KEY REFERENCES repositories (id),
    last_sync_at BIGINT,
    last_error TEXT,
    skipped_json TEXT
);
