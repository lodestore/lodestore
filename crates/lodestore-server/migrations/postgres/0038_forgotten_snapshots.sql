CREATE TABLE forgotten_snapshots (
    id TEXT NOT NULL,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    forgotten_at BIGINT NOT NULL,
    PRIMARY KEY (repository_id, id)
);
