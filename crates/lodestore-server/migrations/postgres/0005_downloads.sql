CREATE TABLE downloads (
    id TEXT PRIMARY KEY,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    host_id TEXT REFERENCES agents (id),
    hostname TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    items_json TEXT NOT NULL,
    format TEXT NOT NULL,
    started_by TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    started_at BIGINT,
    finished_at BIGINT,
    bytes_sent BIGINT NOT NULL DEFAULT 0,
    error_code TEXT
);

CREATE INDEX downloads_created_idx ON downloads (created_at);
