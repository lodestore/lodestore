CREATE TABLE schedule_state (
    policy_id TEXT NOT NULL REFERENCES policies (id) ON DELETE CASCADE,
    host_id TEXT NOT NULL REFERENCES agents (id),
    fingerprint TEXT NOT NULL,
    last_slot_ms BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (policy_id, host_id)
);

ALTER TABLE jobs ADD COLUMN origin TEXT NOT NULL DEFAULT 'manual';
ALTER TABLE jobs ADD COLUMN missed_runs BIGINT;
