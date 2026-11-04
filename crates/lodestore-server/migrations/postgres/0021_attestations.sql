ALTER TABLE repositories ADD COLUMN attest_upgraded_at BIGINT;

ALTER TABLE jobs ADD COLUMN verify_state TEXT;

ALTER TABLE snapshots ADD COLUMN unverified_reason TEXT;

CREATE INDEX jobs_verify_state_idx ON jobs (verify_state);

CREATE TABLE verified_shards (
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    shard_id TEXT NOT NULL,
    verified_at BIGINT NOT NULL,
    PRIMARY KEY (repository_id, shard_id)
);
