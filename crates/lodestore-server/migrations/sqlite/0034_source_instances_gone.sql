CREATE TABLE source_instances_gone (
    policy_name TEXT NOT NULL,
    host_id TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    gone_at BIGINT NOT NULL,
    PRIMARY KEY (policy_name, host_id, source_ref)
);
