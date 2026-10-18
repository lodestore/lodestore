ALTER TABLE snapshots ADD COLUMN received_at BIGINT NOT NULL DEFAULT 0;
ALTER TABLE snapshots ADD COLUMN received_exact BIGINT NOT NULL DEFAULT 0;

UPDATE snapshots SET received_at = discovered_at;

CREATE INDEX snapshots_pair_idx ON snapshots (policy_name, host_id, received_at);

CREATE TABLE snapshot_tombstones (
    id TEXT PRIMARY KEY,
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    forgotten_at BIGINT NOT NULL,
    reason TEXT NOT NULL
);
