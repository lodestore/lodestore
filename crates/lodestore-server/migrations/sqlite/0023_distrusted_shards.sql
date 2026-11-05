CREATE TABLE distrusted_shards (
    repository_id TEXT NOT NULL REFERENCES repositories (id),
    shard_id TEXT NOT NULL,
    at BIGINT NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (repository_id, shard_id)
);
