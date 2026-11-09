CREATE TABLE gc_quarantine (
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    snapshot_id TEXT NOT NULL,
    object_key TEXT NOT NULL,
    reason TEXT NOT NULL,
    confirmable BIGINT NOT NULL,
    detected_at BIGINT NOT NULL,
    confirmed_at BIGINT,
    confirmed_by TEXT,
    PRIMARY KEY (repository_id, snapshot_id)
);
